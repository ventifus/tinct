# Implementation Roadmap

See DONE.md for the full history of completed sprints.

For future work beyond the active sprints below, see:
- `doc/whatif/index.md §Adopt Now` — features ready to implement (String Interpolation Phase 1, Let Binding, Structural Contracts Phase 1, ADTs Phase 1, Source Snippets, Circular Dep Error Paths, Eval Semantics Verification Phase 1)
- `doc/whatif/index.md §Wait for Trigger` — features with complete designs pending a concrete trigger

## I/O and Capabilities

See doc/whatif/io.md.

- [x] Accept io — see doc/whatif/io.md (State: Accepted — 2026-05-04)






## Templating: Text Output and Formatters

See doc/whatif/templating.md.

- [x] Accept templating — see doc/whatif/templating.md (State: Accepted — 2026-05-04)


### templating-phase2: Standard Formatters

**Depends on:** `templating-phase1`

See doc/whatif/templating.md §Phase 2.

- [x] Create `stdlib/fmt/` with yaml.llt, toml.llt, json-pretty.llt, env.llt, csv.llt — all pure tinct
- [x] 8 corpus tests for formatters; arena ThunkId cross-context bug fixed
- [x] doc/11-stdlib.md: Standard Formatters section documented

### templating-phase3: String Interpolation (complete)

- [x] `i"..."` lexer + parser desugaring; `$ident`, `${expr}`, `$$` escape all supported
- [x] Formatter: `i"..."` round-trip preserved (heuristic detection); 5 formatter tests
- [x] 8 corpus tests; doc/02-syntax.md §2.3.5 documented

### templating-phase4: Literate Mode

See doc/whatif/templating.md §Phase 4.

- [x] `tinct literate tangle|eval|weave <file>` subcommand (`src/main.rs`, `src/literate.rs`)
- [x] tangle: extract ```tinct/```llt blocks, join with `---`; eval: tangle + evaluate; weave: annotate results inline
- [x] 10 CLI integration tests; doc/09-documents.md §Literate Mode

## Template-Polarity Research

- [ ] Research template-polarity embedding — evaluate after Phases 1-3 adoption whether `emit` + `i"..."` + formatters cover use cases or whether `tinct template` with `{{ expr }}` delimiters is needed. See doc/whatif/templating.md §Part 3.

## Error Diagnostics: Source Snippets

Deferred phases from the accepted `source-text-availability` proposal. Phase 1 (REPL + CLI snippet rendering) is complete. See `doc/whatif/source-text-availability.md`.

### source-text-multiline: Multi-Line Span Rendering

Extends `render_span_snippet` to show all lines of a multi-line span. Currently only the first line + `...` is shown. See doc/whatif/source-text-availability.md §Phase 2.

- [x] render_span_snippet: shows all lines (start→EOL, middles, col0→end); consistent gutter width
- [x] test_render_span_snippet_multiline updated; integration test added (`src/error.rs`, `src/lib.rs`)

### source-text-lsp: LSP DiagnosticRelatedInformation

**Depends on:** `source-text-multiline`

Populates `related_information` on LSP diagnostics with a source snippet. All three diagnostic constructors in `src/lsp/analysis.rs` currently have `related_information: None`; the source string is already in scope at each call site. See doc/whatif/source-text-availability.md §Phase 3.

- [x] eval_error_to_diagnostic populates related_information (mat-span + stack frames)
- [x] parse/type errors: correctly leave related_information None (no multi-span)
- [x] codespan-reporting: assessed, not adopted (requires Files registry + ANSI output)
- [x] 2 unit tests for eval_error_to_diagnostic related_information (`src/lsp/analysis.rs`)
