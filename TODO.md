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
- [x] 8 corpus tests for formatters
- [x] Fix arena ThunkId cross-context bug: arenas now shared via Rc<RefCell<ThunkArena>>
- [ ] Document standard formatters in `doc/11-stdlib.md`

### templating-phase3: String Interpolation

See doc/whatif/templating.md §Phase 3 and doc/whatif/string-interpolation.md.

- [x] `i"..."` lexer token with InterpolatedPart (Literal, VarRef); parser desugars to `[str ...]` at parse time (`src/lexer.rs`, `src/parser.rs`)
- [x] `$ident` interpolation supported; `${expr}` deferred; `$$` escape for literal `$`
- [x] 8 corpus tests (4 valid syntax + 4 eval); doc/02-syntax.md §2.3.5 documented
- [ ] Formatter: preserve `i"..."` round-trip (currently formats as `[str ...]`) (`src/formatter.rs`)
- [ ] `${expr}` expression interpolation — Phase 2 of string interpolation (`src/lexer.rs`)

### templating-phase4: Literate Mode

See doc/whatif/templating.md §Phase 4.

- [ ] Add `tinct literate` subcommand to CLI (`src/main.rs`)
- [ ] Implement `tinct literate tangle` — extract ` ```tinct ` code blocks as `---`-separated pipeline (`src/literate.rs`)
- [ ] Implement `tinct literate eval` — tangle + evaluate + print result (`src/literate.rs`)
- [ ] Implement `tinct literate weave` — evaluate blocks, render results inline via markers (`src/literate.rs`)
- [ ] Thread `%` between code blocks in document order
- [ ] Corpus tests for tangle and eval modes (`tests/corpus/`)
- [ ] Document literate mode semantics (`doc/09-documents.md`)

## Template-Polarity Research

- [ ] Research template-polarity embedding — evaluate after Phases 1-3 adoption whether `emit` + `i"..."` + formatters cover use cases or whether `tinct template` with `{{ expr }}` delimiters is needed. See doc/whatif/templating.md §Part 3.

## Error Diagnostics: Source Snippets

Deferred phases from the accepted `source-text-availability` proposal. Phase 1 (REPL + CLI snippet rendering) is complete. See `doc/whatif/source-text-availability.md`.

### source-text-multiline: Multi-Line Span Rendering

Extends `render_span_snippet` to show all lines of a multi-line span. Currently only the first line + `...` is shown. See doc/whatif/source-text-availability.md §Phase 2.

- [ ] Extend `render_span_snippet` to render all lines of a multi-line span: first line from start_col to EOL, middle lines at full width, last line from col 0 to end_col (`src/error.rs`)
- [ ] Update `test_render_span_snippet_multiline` to assert full multi-line output — all lines shown, not just `...` marker (`src/error.rs`)
- [ ] Add integration test: error spanning multiple lines shows all span lines in output (`src/lib.rs`)

### source-text-lsp: LSP DiagnosticRelatedInformation

**Depends on:** `source-text-multiline`

Populates `related_information` on LSP diagnostics with a source snippet. All three diagnostic constructors in `src/lsp/analysis.rs` currently have `related_information: None`; the source string is already in scope at each call site. See doc/whatif/source-text-availability.md §Phase 3.

- [ ] Populate `related_information` in `eval_error_to_diagnostic`: add definition span snippet as first entry; add materialization span as second entry when present (`src/lsp/analysis.rs`)
- [ ] Assess whether parse and type errors also benefit from `related_information`; add if so (`src/lsp/analysis.rs`)
- [ ] Evaluate `codespan-reporting` crate vs. hand-rolled rendering; adopt only if it reduces code (`Cargo.toml`)
- [ ] Unit test: `eval_error_to_diagnostic` with a real span produces non-None `related_information` (`src/lsp/analysis.rs`)
