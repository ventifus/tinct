# Implementation Roadmap

See DONE.md for the full history of completed sprints.

For future work beyond the active sprints below, see:
- `doc/whatif/index.md §Adopt Now` — features ready to implement (String Interpolation Phase 1, Let Binding, Structural Contracts Phase 1, ADTs Phase 1, Source Snippets, Circular Dep Error Paths, Eval Semantics Verification Phase 1)
- `doc/whatif/index.md §Wait for Trigger` — features with complete designs pending a concrete trigger

## I/O and Capabilities

See doc/whatif/io.md.

- [x] Accept io — see doc/whatif/io.md (State: Accepted — 2026-05-04)




### io-phase3: Atomic Writes, Streaming Fetch, Sandbox Hardening

**Depends on:** `io-phase2`

See doc/whatif/io.md §Phase 3.

- [ ] Implement atomic file writes via temp file + rename (`src/builtins.rs`)
- [ ] Add `write-atomic` stdlib function using temp + rename pattern (`stdlib/io.llt`)
- [ ] Enable streaming fetch response body via `lines` over socket handle (`stdlib/net.llt`)
- [ ] Harden `--no-pwd --no-stdin --no-env` enforcement (`src/eval.rs`, `src/main.rs`)
- [ ] Add error messages for missing caps (e.g., `open pwd ...` when `--no-pwd`) (`src/builtins.rs`)
- [ ] Corpus tests for fully sandboxed invocations and handle lifecycle (`tests/corpus/io/`)

### io-phase4: Cap Types in Type Checker

**Depends on:** `io-phase3`

See doc/whatif/io.md §Phase 4.

- [ ] Add `Type::DirCap`, `Type::NetCap`, `Type::Handle` variants (`src/types.rs`)
- [ ] Infer cap types in `infer_expr` for builtin calls (`src/typecheck.rs`)
- [ ] Update builtin signatures with cap types (`src/builtins.rs`)
- [ ] Corpus tests for cap type inference and errors (`tests/corpus/typecheck/`)

## Templating: Text Output and Formatters

See doc/whatif/templating.md.

- [x] Accept templating — see doc/whatif/templating.md (State: Accepted — 2026-05-04)

### templating-phase1: emit and Multi-File Pipeline

**Depends on:** `io-phase1`

See doc/whatif/templating.md §Phase 1.

- [ ] Accept multiple `.llt` files in `tinct eval` CLI argument parser (`src/main.rs`)
- [ ] Thread `%` across file boundaries — each file's output becomes `%` for next (`src/eval.rs`)
- [ ] Document `emit` semantics and lazy evaluation interaction (`doc/11a-builtins.md`)
- [ ] Document multi-file pipeline CLI behavior (`doc/09-documents.md`)
- [ ] Corpus tests for `emit` builtin and multi-file pipeline (`tests/corpus/`)

### templating-phase2: Standard Formatters

**Depends on:** `templating-phase1`

See doc/whatif/templating.md §Phase 2.

- [ ] Create `stdlib/fmt/` directory with base formatter pattern
- [ ] Implement `stdlib/fmt/yaml.llt` — YAML 1.2 serializer using type predicates + recursion
- [ ] Implement `stdlib/fmt/toml.llt` — TOML serializer
- [ ] Implement `stdlib/fmt/json-pretty.llt` — indented JSON alternative to compact default
- [ ] Implement `stdlib/fmt/env.llt` — `KEY=VALUE` for `.env` files
- [ ] Implement `stdlib/fmt/csv.llt` — CSV from list-of-dicts
- [ ] Document standard formatters in `doc/11-stdlib.md`
- [ ] Integration tests: data program | formatter produces expected output (`tests/corpus/`)

### templating-phase3: String Interpolation

See doc/whatif/templating.md §Phase 3 and doc/whatif/string-interpolation.md.

- [ ] Add `i"..."` token to lexer — detect `i` prefix before `"` (`src/lexer.rs`)
- [ ] Parse `i"..."` as `InterpolatedString` AST node (`src/parser.rs`, `src/ast.rs`)
- [ ] Desugar `InterpolatedString` to `[str ...]` call in desugar pass (`src/desugar.rs`)
- [ ] Handle `$ident` simple interpolation and `${expr}` expression interpolation in parser
- [ ] Update formatter to preserve `i"..."` strings (idempotency) (`src/formatter.rs`)
- [ ] Corpus tests for string interpolation (`tests/corpus/`)
- [ ] Document string interpolation syntax (`doc/02-syntax.md`)

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
