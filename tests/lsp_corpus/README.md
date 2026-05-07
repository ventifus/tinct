# LSP Corpus Tests

This directory contains file-based tests for the Tinct LSP server.

## Status

**Superseded for diagnostics:** The labeled-section approach in `tests/corpus/` (using `=== out`, `=== warn`, `=== error` sections) is the primary testing mechanism for diagnostics. LSP diagnostics are validated via the same corpus files that drive eval tests.

**Active for LSP-specific features:** This directory is retained for future LSP-specific tests that have no eval-corpus equivalent: hover content, completion suggestions, go-to-definition targets. See TODO.md (`lsp-include-prelude` sprint) for tracking.

## Format

Each test case consists of two files with the same base name:

```
tests/lsp_corpus/
  hover_basic.llt              # LLT source file under test
  hover_basic.expected.json    # Expected LSP responses, keyed by cursor position
```

### Source File (`.llt`)

A valid (or intentionally invalid) LLT source file. Use `$CURSOR` markers to
annotate positions of interest:

```llt
[x: 1  y: [call $+ $CURSOR 2]]
```

### Expected Response File (`.expected.json`)

A JSON object mapping each `$CURSOR` index (0-based, in source order) to the
expected LSP response for that position:

```json
{
  "cursors": [
    {
      "description": "hover on $+ in call position",
      "request": "hover",
      "expected": {
        "contents": {
          "kind": "markdown",
          "value": "**$+** — integer addition\n\nSignature: `(Int, Int) -> Int`"
        }
      }
    }
  ]
}
```

Supported request types:
- `"hover"` — textDocument/hover response
- `"completion"` — textDocument/completion item list
- `"definition"` — textDocument/definition location
- `"diagnostics"` — publishDiagnostics list (no cursor needed; use position `null`)

## Test Runner

No runner exists yet. When implemented, it should:

1. Start the LSP server (`cargo run --bin tinct -- lsp`)
2. Send `initialize` + `initialized`
3. Send `textDocument/didOpen` with the `.llt` source
4. For each `$CURSOR` position, send the corresponding request
5. Compare the response against the `.expected.json` entry
6. Report pass/fail per cursor

See TODO.md `test-tooling` section: "Integration tests for REPL/LSP".
