# What If: Source Text Availability at EvalError Display Time

**State:** Accepted — 2026-05-04

What would it take to include source snippets with caret annotations in tinct error output?

## Current State

`EvalError` carries a `Span` (definition site and optional materialization site) but not the source text that produced it. The `Display` implementation for `EvalError` renders the span coordinates (`line:col`) but cannot show the source line itself or a caret (`^`) pointing at the error location.

```
error E020: type mismatch: expected Int, got String
  at 3:12 (materialized at 7:8)
```

The source text is always available at the site where the error is displayed — the REPL has the user's input, the CLI has the file contents, the LSP has the document text — but `EvalError` doesn't carry it.

### What's Missing

1. Source line display: "the line of code that caused the error" with a caret under the problematic expression
2. Multi-line span rendering for expressions that span multiple lines
3. Consistent snippet format across REPL, CLI, and LSP error paths

## Why Source Snippets Matter for tinct

Lazy evaluation makes error sites non-obvious: a type mismatch in a dict field may be triggered several call sites away from its definition. Showing the source line at `def_span` gives users an immediate visual anchor — they see exactly which expression failed, not just its coordinates.

## Design

**Caller-pairs-with-source (option c).** The source text is not stored in `EvalError`. Instead, each display site passes its locally available source string to a rendering helper. This matches Nickel's `to_diagnostic(files: &Files<String>)` pattern and requires no change to the `EvalError` struct.

### `render_span_snippet(source: &str, span: Span) -> Option<String>`

A standalone helper (in `src/error.rs` or a new `src/error_render.rs`) that extracts the relevant source lines and renders a caret annotation:

```rust
pub fn render_span_snippet(source: &str, span: Span) -> Option<String> {
    // Suppress for synthetic spans (Span::origin() = line 1, col 1, offset 0)
    if span == Span::origin() { return None; }
    // Extract line(s) by splitting on '\n' and indexing by span.start.line (1-based)
    // Render: line text, then "    " + "^" repeated for (end_col - start_col), clamped to line length
    // Multi-line: first line: "^" from start_col to line end; middle lines: full "^"; last line: "^" to end_col
    ...
}
```

### REPL integration (`src/repl.rs`)

`eval_input` has `input: &str` in scope when errors occur. Change the error conversion from `format!("{e}")` to `render_eval_error(input, &e)`:

```rust
// Before:
.map_err(|e| format!("{e}"))?

// After:
.map_err(|e| {
    let mut msg = format!("{e}");
    if let Some(snippet) = render_span_snippet(input, e.definition_span) {
        msg.push('\n');
        msg.push_str(&snippet);
    }
    msg
})?
```

`StepResult = Result<String, String>` is unchanged. No interface change required.

### CLI integration (`src/main.rs`)

The CLI reads source from a file before calling `eval_file`. At the point where `EvalError` is displayed, the source string is available. Wrap display:

```rust
// After reading source_text and calling eval:
if let Err(e) = result {
    eprintln!("{e}");
    if let Some(snippet) = render_span_snippet(&source_text, e.definition_span) {
        eprintln!("{snippet}");
    }
}
```

### LSP integration (`src/lsp/analysis.rs`)

LSP errors are surfaced as `Diagnostic` structs, not displayed as strings. The `related_information` field on `Diagnostic` can include a source snippet via `DiagnosticRelatedInformation { location, message }`. The document source is available in `DocumentState`. This is a separate phase — the REPL and CLI are Phase 1.

## What Would Change

### `src/error.rs`

**Current:** `EvalError::Display` renders coordinates only.
**Proposed:** Add `pub fn render_span_snippet(source: &str, span: Span) -> Option<String>` as a public helper. No change to the `EvalError` struct or its `Display` impl.
**Impact:** Minor.

### `src/repl.rs`

**Current:** `eval_input` converts `EvalError` to `String` via `format!("{e}")`.
**Proposed:** `render_eval_error(input, &e)` renders error + snippet. `StepResult` signature unchanged.
**Impact:** Minor.

### `src/main.rs`

**Current:** Errors displayed via `eprintln!("{e}")`.
**Proposed:** Errors displayed with snippet appended.
**Impact:** Minor.

## Phased Adoption

### Phase 1: REPL + CLI snippet rendering

Add `render_span_snippet`. Wire into `eval_input` (REPL) and `main.rs` (CLI). Single-line span rendering only; multi-line as stretch goal.

### Phase 2: Multi-line spans

Extend `render_span_snippet` to handle spans crossing multiple lines.

### Phase 3: LSP `related_information`

Populate `DiagnosticRelatedInformation` with source snippet for spans in the LSP path.

### Prerequisites

- Phase 1: no prerequisites (source text is already available at all display sites)
- Phase 2: Phase 1 complete
- Phase 3: Phase 1 complete; assess whether `codespan-reporting` crate provides value over hand-rolled rendering

### Trigger

- Phase 1: when REPL error messages are confusing without source context (already the case)
- Phase 2: when multi-line expressions commonly appear in REPL usage
- Phase 3: when LSP users report difficulty identifying error locations without source snippets

## References

- Nickel source: `EvaluationError::to_diagnostic(files)` — passes source files at display boundary, not stored in error
- rustc `SourceMap`: global registry; `Diagnostic` does not carry source. Different from tinct's simpler single-file model.
