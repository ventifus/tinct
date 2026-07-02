# Source Text Availability at EvalError Display Time

## Overview

Source snippets with caret annotations appear in tinct error output. Lazy
evaluation makes error sites non-obvious: a type mismatch in a dict field may
be triggered several call sites away from its definition. Showing the source
line at `def_span` gives users an immediate visual anchor — they see exactly
which expression failed, not just its coordinates.

Before this feature:

```text
error E020: type mismatch: expected Int, got String
  at 3:12 (materialized at 7:8)
```

Now errors include the source line and caret annotation pointing at the
problematic expression.

All phases implemented, including CLI and LSP `related_information`
(completed 2026-05-05).

## Design

**Caller-pairs-with-source.** The source text is not stored in `EvalError`.
Instead, each display site passes its locally available source string to a
rendering helper. This matches Nickel's `to_diagnostic(files: &Files<String>)`
pattern and requires no change to the `EvalError` struct.

### `render_span_snippet(source: &str, span: Span) -> Option<String>`

A standalone helper (in `src/error.rs` or `src/error_render.rs`) that extracts
the relevant source lines and renders a caret annotation:

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

### CLI integration (`src/main.rs`)

The CLI reads source from a file before calling `eval_file`. At the point where
`EvalError` is displayed, the source string is available:

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

LSP errors are surfaced as `Diagnostic` structs. The `related_information`
field on `Diagnostic` includes a source snippet via
`DiagnosticRelatedInformation { location, message }`. The document source is
available in `DocumentState`.

## Implementation

### `src/error.rs`

`EvalError::Display` renders coordinates only; no change to the struct or its
`Display` impl. Added `pub fn render_span_snippet(source: &str, span: Span) ->
Option<String>` as a public helper.

### `src/main.rs`

Errors displayed with snippet appended via `render_span_snippet`.

## References

- Nickel source: `EvaluationError::to_diagnostic(files)` — passes source files at display boundary, not stored in error
- rustc `SourceMap`: global registry; `Diagnostic` does not carry source. Different from tinct's simpler single-file model.
