# Implementation Roadmap

See DONE.md for the full history of completed sprints.

For future work beyond the active sprints below, see:
- `doc/whatif/index.md §Adopt Now` — features ready to implement (String Interpolation Phase 1, Let Binding, Structural Contracts Phase 1, ADTs Phase 1, Source Snippets, Circular Dep Error Paths, Eval Semantics Verification Phase 1)
- `doc/whatif/index.md §Wait for Trigger` — features with complete designs pending a concrete trigger

## Access and Generator Pipeline

Pipe operator, integer dot access, `get` builtin, and generator stdlib. See `doc/whatif/access-pipeline.md`.

- [x] Accept access-pipeline — see doc/whatif/access-pipeline.md (State: Accepted — 2026-05-05)

### access-pipeline-phase2: Remove Bracket Access + Prelude Migration

**Depends on:** `access-pipeline-phase1`

See doc/whatif/access-pipeline.md §Phase 2. Breaking — all bracket/range access must migrate atomically with `stdlib/prelude.llt` refactor, otherwise prelude fails to load.

- [ ] Remove `Token::BracketAccess`, `Token::Range` (`..`) from lexer; remove whitespace-sensitive `[` detection (`src/lexer.rs`)
- [ ] Remove `StackFrame::BracketAccessKey` from parser; remove `Expr::BracketAccess`, `Expr::RangeAccess` from AST (`src/parser.rs`, `src/ast.rs`)
- [ ] Remove all `BracketAccess`/`RangeAccess` arms from: `desugar.rs`, `resolve.rs`, `typecheck.rs`, `eval.rs`, `eval_materialize.rs`, `eval_access.rs`, `formatter.rs`
- [ ] Add Seq-at-top-level error to CLI: if top-level is `Value::Seq` and `emitted = false`, return error; if `emitted = true`, force each element (`src/main.rs`)
- [ ] Refactor `stdlib/prelude.llt`: redefine `get` to use `builtin-get`; add `collect-kv` using `reduce`+`merge`; replace all dynamic `xs[k]` / `ks[i]` with `[get k xs]` / `[get i ks]`; replace literal integer access `left[0]` → `left.0`, `pairs[i][0]` → `[get i pairs].0` (~30+ occurrences) (`stdlib/prelude.llt`)
- [ ] Migrate all corpus tests using `dict[$key]`, `list[0]`, `seq[0..n]` to `|` / dot / `get` / `slice` equivalents (`tests/corpus/`)

### access-pipeline-stdlib: Migrate Existing Stdlib Files

**Depends on:** `access-pipeline-phase2`

Migrate all existing stdlib files written in old bracket-access syntax. Future stdlib files (`stdlib/io.llt`, `stdlib/net.llt`) are written fresh in new syntax and do not need migration.

- [ ] Migrate `stdlib/fmt/toml.llt` — 6 bracket-access occurrences (`stdlib/fmt/toml.llt`)
- [ ] Migrate `stdlib/fmt/csv.llt` — 4 bracket-access occurrences (`stdlib/fmt/csv.llt`)
- [ ] Migrate `stdlib/fmt/yaml.llt` — 3 bracket-access occurrences (`stdlib/fmt/yaml.llt`)
- [ ] Migrate `stdlib/fmt/json-pretty.llt` — 3 bracket-access occurrences (`stdlib/fmt/json-pretty.llt`)
- [ ] Migrate `stdlib/fmt/env.llt` — 1 bracket-access occurrence (`stdlib/fmt/env.llt`)
- [ ] Verify all stdlib corpus tests pass with new syntax (`tests/corpus/`)

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

### default-emit: Default Emit Program

When `tinct eval` finishes and `emit` was never called, run the result through
a pure-tinct JSON formatter instead of the current hardcoded Rust `value_to_json()`.
This makes the default output behavior user-observable and replaceable.

- [ ] Implement `stdlib/fmt/json.llt` — pure-tinct compact JSON serializer using
  type predicates (`int?`, `str?`, `dict?`, `seq?`, `null?`, `bool?`, `float?`)
  and `str` concatenation; handles nested dicts/seqs, string escaping, null (`[]`) (`stdlib/fmt/json.llt`)
- [ ] Wire in CLI: when `emitted == false` after evaluation, run the result through
  `stdlib/fmt/json.llt` as an implicit final pipeline stage (same mechanism as
  multi-file pipeline) rather than calling `value_to_json()` directly (`src/main.rs`)
- [ ] Keep `value_to_json()` for LSP, tests, and the REPL — only the CLI default
  output path switches to the tinct formatter (`src/main.rs`)
- [ ] Corpus/CLI tests: verify default output of `tinct eval simple.llt` matches
  previous JSON output for Int, Float, String, Bool, Dict, Seq, Null (`tests/cli_tests.rs`)
- [ ] Update `doc/09-documents.md` and `doc/12-tooling.md` to document that the
  default output is produced by `stdlib/fmt/json.llt`

## Language Design Research

- [x] Research null semantics — see doc/whatif/null-semantics.md
- [x] Research tinct-hosted formatter — see doc/whatif/tinct-hosted-formatter.md and doc/whatif/ast-schema.md
- [x] Research macro-rewrite — see doc/whatif/macro-rewrite.md
- [x] Research parse-stage macros — see doc/whatif/parse-stage-macros.md

## Template-Polarity Research

- [x] Research template-polarity embedding — see doc/whatif/template-polarity.md

## Type System Correctness and Performance

### type-checker-fixes: Type Checker Correctness and Allocation Optimizations

- [ ] `Seq@ElemType` in type annotations: add `"Seq"` to `resolve_type_name` (bare `@Seq` → `Type::Seq(Any)`); add `name == "Seq"` arm to `resolve_annotated` resolving the annotation as the element type → `Type::Seq(Box::new(elem))` (mirrors the `name == "Fn"` arm); add `"Null"` to `resolve_type_name` → `Type::Record(Row::Empty)` (NOT a new `Type::Null` variant — see doc/whatif/null-semantics.md) so void return types can be expressed as `fn@Null`; add corpus tests for `[fn@Seq@String [h@Handle] ...]` and `[fn@Null [s@String] ...]` (`src/typecheck.rs`)
- [ ] Update void-returning builtin type signatures: change return type from `Type::Any` to `Type::Record(Row::Empty)` for `emit`, `write`, `write-atomic`, `revoke-cap`, `mkdir`, `delete` in `src/types.rs`; add comment `// Null — Type::Record(Row::Empty), see doc/whatif/null-semantics.md`
- [ ] Add `Null` to the type conventions table in `doc/05-type-annotations.md`; update `doc/11a-builtins.md` void-returning builtin signatures to show `fn@Null`
- [ ] Fix Guarded error decoration bypass: apply `decorate` to inner-error path at `eval.rs:1625` and `eval_materialize.rs:1220` so TypeAssert failures carry materialization context — currently the only ThunkState branch that skips decoration (`src/eval.rs`, `src/eval_materialize.rs`) [Major — computer-scientist]
- [ ] Thread `row_ann_mapping` through `resolve_type_assert` and `resolve_annotated` so named row variables (e.g., `...r`) in TypeAssert annotations are scoped correctly rather than creating fresh anonymous row vars — match the pattern used in `infer_fn:2033-2038` (`src/typecheck.rs`) [Major — computer-scientist]
- [ ] unify_rows: add `RowTail::Empty` fast-path before key iteration to avoid 5 collection allocations (2 HashSets, Vec, 2 HashMaps) for the common closed-row case (`src/types.rs:1167`) [Critical — performance-expert]
- [ ] resolve_row: add `if row.fields.is_empty()` guard to return resolved row directly without cloning for the common case of bound row vars with no extra fields (`src/types.rs:826`) [Critical — performance-expert]
- [ ] lower_row_var_levels: fuse two separate update loops into a single `type_vars.iter().chain(&row_vars)` pass; saves 2 loop iterations per call — called twice in Case 4 (`src/types.rs:913`) [Critical — performance-expert]
- [ ] Substitution::apply(): add concrete-type fast-path (Int/Float/Bool/Str/Number/Any/Never) before `visited_types`/`visited_rows` HashSet allocation; saves 2 allocations per apply on primitives — common in dict field unification (`src/types.rs:542`) [Major — performance-expert]
- [ ] instantiate_at_level: fuse `collect_type_vars` + `collect_row_vars` into a single `collect_all_vars(&mut type_vars, &mut row_vars)` tree walk to save 1 full tree walk + 1 BTreeSet allocation per CALL-POLY invocation (`src/types.rs:1600`) [Major — performance-expert]
- [ ] eval_dict: replace `contains_key + insert` with single `insert()` checking returned `Option` to save N hash lookups per dict construction (`src/eval.rs:961`) [Minor — performance-expert]
- [ ] GuardedValidate error path: reuse owned `field_path` Box instead of clone+rebox (`eval_materialize.rs:1216`) [Minor — computer-scientist]
- [ ] CALL-POLY: remove redundant double-application of substitution to return type — after merge into `state.subst`, use `state.subst.apply(inst_ret)` directly (`src/typecheck.rs:1966`) [Minor — computer-scientist]
- [ ] infer_fn: add early-exit guard when no params have annotations to skip HashMap allocation for every annotation-free lambda (`src/typecheck.rs`) [Minor — performance-expert]

### prelude-type-annotations: Full Type Annotations for stdlib/prelude.llt

Add complete type annotations (parameter types and return types) to all functions in `stdlib/prelude.llt`. Currently most prelude functions have no annotations, relying entirely on inference.

- [ ] Add type annotations to all 82 public prelude functions: parameter types, return types, and where relevant rest-row constraints (`stdlib/prelude.llt`)
- [ ] Verify annotations don't narrow inference — no regressions on existing corpus tests
- [ ] Add reference section to `doc/11-stdlib.md` documenting prelude function type signatures

## Test Coverage

### test-coverage-gaps: Missing Corpus and Unit Tests

- [ ] Add interpolated string corpus tests: `tests/corpus/valid/literals/interpolated_strings.llt-eval` (basic i"Hello $name", i"$$escaped", variable boundaries) and `tests/corpus/eval/builtins/interpolated_string_eval.llt-eval` (desugaring to `str` calls) [Critical — test-crafter]
- [ ] Add row polymorphism corpus tests: anonymous rest (`...`), named rest (`...r`), rest in function signatures, rest with field constraints in `tests/corpus/eval/type_system/row_*.llt-eval` [Critical — test-crafter]
- [ ] Add deeply chained access corpus tests: 5+ level chains mixing dot/bracket access and mid-chain error cases (`tests/corpus/eval/access/deeply_chained_mixed.llt-eval`) [Critical — test-crafter]
- [ ] Add pipeline section metadata corpus tests for `--- %name@Type` (output type annotation) and `--- expects: Type` (input contract) in `tests/corpus/eval/pipeline/` [Critical — test-crafter]
- [ ] Add annotation bracket restriction invalid tests: `x@[call f]`, `x@[fn [a] a]`, `x@[@Type e]` must all be parse errors (`tests/corpus/invalid/syntax_errors/annotation_special_form.llt-eval`) [Critical — test-crafter]
- [ ] Add static constraint corpus test: verify rest entry position `[a ... b]` is valid syntax (`tests/corpus/valid/edge_cases/rest_entry_positions.llt-eval`) [Major — test-crafter]
- [ ] Add Unicode identifier corpus tests (`tests/corpus/valid/literals/unicode_identifiers.llt-eval`) and escape sequence rejection tests for `\x41`, `A` (`tests/corpus/invalid/syntax_errors/unsupported_escape.llt-eval`) [Major — test-crafter]
- [ ] Add MAX_PARSE_DEPTH boundary tests: 256-nested-bracket file (valid) and 257 (error) in `tests/corpus/` [Major — test-crafter]
- [ ] Add CRLF formatter roundtrip unit test in `src/formatter.rs`: parse CRLF input, format, assert line endings preserved [Major — test-crafter]
- [ ] Add builtin limit enforcement tests for MAX_COLLECT_SIZE and MAX_STRING_SIZE in `tests/corpus/eval/errors/` [Minor — test-crafter]

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

### lsp-goto-definition: Go To Definition

Intra-document "go to definition" — F12 in VS Code and equivalents. Given a cursor on a variable reference, returns the span of the dict entry key that defines that name in the current file. Builtins and `$include`-introduced names return no result (cross-file resolution is a future concern; `no_fs=true` makes it non-trivial).

- [ ] `fn key_name(key_expr: &Expr) -> Option<&str>`: `Expr::Str(s)` → `s.as_str()`, `Expr::Annotated { name, .. }` → `name.as_str()`, all other key forms → `None` (computed/int keys are not static definition targets) (`src/lsp/analysis.rs`)
- [ ] `fn name_at_offset(expr: &Expr, span: Span, offset: usize) -> Option<String>`: recursive span-containment walk returning `VarRef.name.clone()` for the innermost `Expr::VarRef` at `offset`; recurses into Dict entries, Call func+args+named_args, Fn body, DotAccess, BracketAccess, RangeAccess, Pipe, TypeAlias, TypeAssert; returns `None` for literals, Error, Rest, Annotated, Fn params (`src/lsp/analysis.rs`)
- [ ] `fn find_key_definition(expr: &Expr, span: Span, name: &str) -> Option<Span>`: recursive walk; on `Expr::Dict(entries)` matches `key_name(key) == name` and returns key span; recurses into entry values, call args, fn bodies, access targets to find definitions in nested dicts; depth-first, first match wins (`src/lsp/analysis.rs`)
- [ ] `pub fn definition_at(doc: &DocumentState, offset: usize) -> Option<Span>`: requires `doc.ast.is_ok()`; walks documents → expressions with `name_at_offset` to find the name at cursor; then walks documents → expressions with `find_key_definition`; returns definition span or `None` (`src/lsp/analysis.rs`)
- [ ] Add `definition_provider: Some(lsp_types::OneOf::Left(true))` to `ServerCapabilities` in `run_lsp()` (`src/lsp/server.rs`)
- [ ] `GotoDefinition::METHOD` handler in `handle_request`: deserialize `GotoDefinitionParams`, convert position to offset, call `definition_at`, respond with `GotoDefinitionResponse::Scalar(Location { uri, range })` or serialized `null`; error handling mirrors `HoverRequest` arm (`src/lsp/server.rs`)
- [ ] Unit tests: `test_definition_at_simple` (`[x: 42  y: $x]`, cursor on `$x` → key `x` span); `test_definition_at_mutually_recursive` (`[a: $b  b: $a]`); `test_definition_at_annotated_key` (`[x@Int: 1  y: $x]` → `x@Int` key span); `test_definition_at_no_match` (`$undefined` → `None`); `test_definition_at_nested_dict` (cursor on name defined in inner dict); `test_definition_at_parse_error` (parse failure → `None`) (`src/lsp/analysis.rs`)

### lsp-include-prelude: Prelude Awareness in LSP

**Depends on:** `lsp-goto-definition`

Eliminates false "undefined variable" diagnostics and missing hover types for all prelude-defined functions (`map`, `filter`, `identity`, etc.). At LSP startup, parse the embedded prelude source, type-check it to extract a `TypeMap`, and build a `name → key-span` index. Hover gains accurate types for prelude names; go-to-definition navigates to the on-disk `stdlib/prelude.llt`.

- [ ] `fn find_stdlib_prelude_path() -> Option<PathBuf>`: tries (1) `current_exe()` grandparent + `stdlib/prelude.llt` (dev layout: `target/debug/tinct` → project root), then (2) `current_exe()` parent + `../share/tinct/stdlib/prelude.llt` (install layout); mirrors `libdir_path` resolution in `src/main.rs` (`src/lsp/document.rs`)
- [ ] `pub struct PreludeIndex`: `path: Option<PathBuf>`, `name_to_key_span: HashMap<String, Span>`, `type_map: TypeMap`; constructed once in `DocumentStore::new()` (`src/lsp/document.rs`)
- [ ] `fn build_prelude_index() -> PreludeIndex`: parses `include_str!("../../stdlib/prelude.llt")`, runs `desugar_file` + `resolve_file`, calls `typecheck_file_with_types` for `TypeMap`, walks top-level `Expr::Dict` entries collecting `Expr::Str(s)` key names → spans; logs warning but returns empty index on parse failure (`src/lsp/document.rs`)
- [ ] Add `prelude_index: PreludeIndex` to `DocumentStore`; initialize in `DocumentStore::new()` (`src/lsp/document.rs`)
- [ ] Seed `TypeEnv` with `Type::Any` for each prelude name before per-document type inference, suppressing false "undefined variable" errors; expose as `TypeEnv::with_builtins_and_prelude(names: &[&str])` or thread `PreludeIndex` into `DocumentState::new()` (`src/typecheck.rs`, `src/lsp/document.rs`)
- [ ] Extend `hover_at` to fall back to `prelude_index.type_map` when the document `type_map` has no entry: look up name in `prelude_index.name_to_key_span`, retrieve type from `prelude_index.type_map`; signature gains `prelude_index: &PreludeIndex` param; update call site in `server.rs` (`src/lsp/analysis.rs`, `src/lsp/server.rs`)
- [ ] Extend `definition_at` to accept `store: &DocumentStore`: after `find_key_definition` returns `None`, look up name in `prelude_index.name_to_key_span`; if found, return `Some((Url::from_file_path(path)?, span))`; update return type to `Option<(Url, Span)>`; update `GotoDefinition` handler in `server.rs` (`src/lsp/analysis.rs`, `src/lsp/server.rs`)
- [ ] Unit tests: `test_prelude_index_non_empty` (index contains `"map"`, `"filter"`, `"identity"`); `test_hover_prelude_name` (hover on `$map` returns non-empty string without `<error>`); `test_definition_at_prelude_name` (definition on `$identity` returns `Some` when prelude path resolves); `test_no_false_undefined_for_prelude` (`[call $map [fn [x] x] [1 2 3]]` → zero type errors) (`src/lsp/analysis.rs`, `src/lsp/document.rs`)

### lsp-workspace-index: Cross-File Include Resolution

**Depends on:** `lsp-include-prelude`

Teaches the LSP to resolve `$include "path"` at the document level using plain filesystem reads (bypassing `no_fs=true` eval-time blocking). When a document's AST contains `Call { func: VarRef("include"), args: [Str(path)] }`, the LSP reads and parses that file independently, maintaining a `DocumentState` for it in an include graph. Cross-file names become available to `definition_at` and hover.

- [ ] `fn collect_include_paths(file: &File) -> Vec<(String, Span)>`: walks all `Expr::Call` nodes; matches `func` as `VarRef { name: "include" }` and first positional arg as `Expr::Str(path)`; returns `(path.clone(), call_span)` pairs; ignores non-literal paths (`src/lsp/analysis.rs`)
- [ ] `pub struct IncludeGraph`: `HashMap<Url, IncludeNode>` where `IncludeNode` holds `state: DocumentState`, `includes: Vec<Url>`, `included_by: Vec<Url>`; stored on `DocumentStore` (`src/lsp/document.rs`)
- [ ] `fn resolve_include_url(base_url: &Url, path: &str) -> Option<Url>`: resolves `path` relative to `base_url`'s parent directory via `PathBuf::join`; returns `None` for non-`file://` base URLs or resolution failures (`src/lsp/document.rs`)
- [ ] `fn index_file(url: Url, graph: &mut IncludeGraph, stdlib_env, eval_ctx, depth: usize)`: reads file with `std::fs::read_to_string` (plain OS read, not eval); creates `DocumentState::new`; calls `collect_include_paths`; adds forward/reverse edges; recurses for unseen URLs; depth cap at 16 to handle circular includes (`src/lsp/document.rs`)
- [ ] `fn invalidate_dependents(changed_url: &Url, graph, stdlib_env, eval_ctx)`: follows `included_by` reverse edges breadth-first; re-runs `index_file` for each dependent; stops at already-reindexed URLs (handles diamond deps) (`src/lsp/document.rs`)
- [ ] Extend `DocumentStore::update_document`: call `collect_include_paths` on fresh AST, call `index_file` for new include URLs, remove stale edges, call `invalidate_dependents` (`src/lsp/document.rs`)
- [ ] Extend `definition_at` to search direct includes after prelude lookup fails: for each URL in the document's `IncludeNode::includes`, call `find_key_definition` on its `DocumentState`; return `Some((included_url, span))` on first match (`src/lsp/analysis.rs`)
- [ ] Extend `hover_at` to fall back to direct includes' `type_map` after prelude miss (`src/lsp/analysis.rs`)
- [ ] Unit tests: `test_collect_include_paths_literal` (`[call $include "foo.llt"]` → `[("foo.llt", _)]`); `test_collect_include_paths_non_literal` (`[call $include $path]` → `[]`); `test_resolve_include_url`; `test_circular_include_depth_cap` (A includes B includes A — no stack overflow); `test_definition_at_cross_file` (name defined in included file returns included file URL + key span) (`src/lsp/document.rs`, `src/lsp/analysis.rs`)

### lsp-doc-annotations: `doc:` Annotation Hover

**Depends on:** `lsp-include-prelude`

Wire up `doc:` annotation metadata to LSP hover. When a param or function is annotated with `doc: "..."`, the hover tooltip shows the doc string below the type signature. The type checker already ignores `doc:` — this sprint extracts it separately and threads it through to the LSP layer. See `doc/05-type-annotations.md` §`@` Property Annotations.

- [ ] `pub type DocMap = HashMap<String, String>`: `name → doc string`; extract from `Annotation::PropertyDict` entries where key is `"doc"` and value is `Expr::Str`; build alongside `TypeMap` in `typecheck_file_with_types`; return as `(TypeMap, DocMap)` tuple (`src/typecheck.rs`)
- [ ] Add `doc_map: DocMap` field to `DocumentState`; populate from `typecheck_file_with_types` result; initialize to empty `HashMap` on type-check failure (`src/lsp/document.rs`)
- [ ] Extend `hover_at`: after formatting the type string, look up name in `doc_map`; if present, append `"\n\n"` + doc string to the hover markdown so VS Code renders it as a second paragraph (`src/lsp/analysis.rs`)
- [ ] Unit tests: `test_hover_shows_doc` (`[fn [x@[type: String doc: "the name"]] $x]`, hover on `$x` → hover contains `"the name"`); `test_hover_no_doc` (param without `doc:` → hover contains type only, no extra newline); `test_hover_doc_and_default` (`x@[type: Number default: 0 doc: "count"]` → hover shows both type and doc) (`src/lsp/analysis.rs`)

## CLI: Inline Expressions and JSON Streaming

### eval-cli: Inline Expressions, Input/Output Formatters

Enable `tinct eval -i json -e 'expr' -o raw < input.json` as a jq-style
JSON processor. `-e` evaluates an inline expression; `-i <name>` prepends
`stdlib/in/<name>.llt` as an input stage (suppressing auto JSON detection);
`-o <name>` appends `stdlib/fmt/<name>.llt` as an output stage. Together
these form a symmetric pipeline: input formatter → expression → output formatter.
See `doc/12-tooling.md`.

Stdin auto-detection (`read_stdin_json()`) is preserved when `-i` is absent
for backward compatibility. When `-i` is present, auto-detection is suppressed
and the input program reads from the `stdin` Handle directly.

No hard dependency on `access-pipeline` — dot access works today. Pipe
expressions (`|`) work automatically in `-e` strings once `access-pipeline-phase1`
lands.

- [ ] Section header loop: add `Token::Semicolon` as a break condition alongside `Token::Newline` at `src/parser.rs:2350` — currently `;` hits the catch-all "unexpected token in section header" error, making `--- %name@Type; [expr]` fail; every other Newline-break in the parser already includes Semicolon (`peek_next_significant_token`, `skip_whitespace_tokens`, main loop all use `Newline | Semicolon`) (`src/parser.rs`)
- [ ] Rename `stdlib/fmt/` → `stdlib/out/` via `git mv stdlib/fmt stdlib/out`; update all references in `doc/`, `TODO.md`, and `doc/whatif/` from `stdlib/fmt/` to `stdlib/out/` (`stdlib/`, `doc/`, `TODO.md`)
- [ ] Add `-e <expr>` / `--expr <expr>` flag to `tinct eval`: repeatable — each occurrence inserts an inline tinct expression as a pipeline stage at that position in the command line, interleaved with file arguments in order; `tinct eval -i json -e '%.x' transform.llt -e '[+ % 1]' -o raw` runs four stages in sequence; each `-e` expression receives `%` from the previous stage exactly as a file would; `---` is valid inside a single `-e` string to create multiple stages within it (the lexer already recognizes `---` anywhere, not just at line start); `;` is whitespace-equivalent and compresses multi-line syntax but does not create pipeline stages (`src/main.rs`)
- [ ] Add `-i <format>` / `--input <format>` flag: resolves `<format>` to `stdlib/in/<format>.llt` via `libdir_path`; prepends that file as the first pipeline stage; suppresses `read_stdin_json()` auto-detection so the input program reads from the `stdin` Handle directly; error if named file does not exist (`src/main.rs`)
- [ ] Add `-o <format>` / `--output <format>` flag: resolves `<format>` to `stdlib/out/<format>.llt` via `libdir_path`; appends that file as the final pipeline stage — exactly equivalent to appending the file to the CLI file list; error if named file does not exist (`src/main.rs`)
- [ ] Implement `stdlib/in/json.llt`: reads `[slurp stdin]` and passes through `[from-json]`; produces the parsed tinct value as `%` for the next stage (`stdlib/in/json.llt`)
- [ ] Implement `stdlib/out/raw.llt`: if `%` is a String emit it unquoted; if `%` is a Seq emit each element on its own line via `each` + `emit`; otherwise `[error "raw formatter: expected String or Seq, got " [type-of %]]` (`stdlib/out/raw.llt`)
- [ ] CLI tests: `tinct eval -e '%.x' <<< '{"x":42}'` → `42` (auto-detect); `tinct eval -i json -e '%.x' <<< '{"x":42}'` → `42` (explicit); `tinct eval -i json -e '%.msg' -o raw <<< '{"msg":"hello"}'` → `hello` (no quotes); `tinct eval -e '[x: 1]' -e '[merge % [y: 2]]'` → `{"x":1,"y":2}` (chained `-e`); `-i` + `-o` together; unknown format name errors clearly; `-e` parse error reports inline source and stage index (`tests/cli_tests.rs`)
- [ ] Update `doc/12-tooling.md`: document `-e`, `-i`/`--input` and `stdlib/in/` convention, `-o`/`--output` and `stdlib/out/` convention, stdin dual-use (auto-detect vs Handle), and the symmetric pipeline model; add jq-comparison example: `tinct eval -i json -o raw -e '%.response' < mcp.json`

### fmt-oneline: Single-Line Formatter Mode

**Depends on:** `eval-cli` (the `Token::Semicolon` header-break fix must land first so `--oneline` output is re-parseable)

Add `tinct fmt --oneline` that produces a single-line, re-parseable representation of any tinct program. Useful for constructing `-e` strings programmatically, diffing, and log-safe embedding of tinct expressions.

The formatter already builds output as a `String` via `self.output`. The changes are localized to the section-separator block and document formatting — the expression formatting already compresses naturally when not given line-breaking hints.

- [ ] Add `--oneline` flag to `tinct fmt` subcommand; thread an `oneline: bool` field through `Formatter` struct (`src/main.rs`, `src/formatter.rs`)
- [ ] In `format_file` document separator block (`src/formatter.rs:40-72`): when `oneline`, replace the leading `\n\n` with ` ` (or nothing if first doc), emit `---` + header metadata as normal, then emit `; ` instead of `\n\n` after the header — making `[doc1] --- %name@Type; [doc2]` the single-line form
- [ ] Strip comments in `--oneline` mode: comments (`Token::Comment`) cannot survive without newlines; skip all `leading_comments` emission in the formatter when `oneline` is set (`src/formatter.rs`)
- [ ] Suppress `ensure_trailing_newline()` when `--oneline` (single-line output has no trailing newline) (`src/formatter.rs`)
- [ ] In `format_document` and sub-formatters: replace any remaining `\n` emitted within expressions with single spaces when `oneline` is set — audit all `self.output.push('\n')` call sites in `src/formatter.rs` (`src/formatter.rs`)
- [ ] Add `--nospaces` flag: remove inter-token spaces except where required for unambiguous tokenization; rule — insert a single space between consecutive tokens only when the preceding token's last character AND the following token's first character are both bare-word characters (alphanumeric, `-`, `_`, `?`, `!`, `/`, `%`, `~`); without this guard `---%name` would lex as a single bare word rather than `DocSeparator` + `%name` identifier (`src/formatter.rs`)
- [ ] Add `--minimize` flag: shorthand that sets both `oneline = true` and `nospaces = true`; equivalent to `tinct fmt --oneline --nospaces`; useful for producing maximally compact one-line output for embedding in shell scripts or `-e` strings (`src/main.rs`, `src/formatter.rs`)
- [ ] Round-trip tests: `tinct fmt --oneline file.llt | tinct eval -` equals `tinct eval file.llt`; `tinct fmt --nospaces file.llt | tinct eval file.llt` equals original; `tinct fmt --minimize file.llt | tinct eval -` equals original; section headers with `%name@Type expects: T` survive all modes; comments stripped in `--oneline`/`--minimize`; all three modes are idempotent (`src/formatter.rs`, `tests/`)
- [ ] Update `doc/12-tooling.md`: document `--oneline`, `--nospaces`, `--minimize`; note comments stripped in oneline/minimize; explain the bare-word-adjacency rule for `--nospaces`; give section-header `;` example

## Macros Cluster: Theoretical Gaps

Gaps between the macros-cluster plan (`doc/whatif/plans/macros-cluster.md`) and the theoretical requirements established by the cited papers. Track here; resolve during the relevant sprint.

- [ ] M3c unquote-splice top-level error: specify that `[unquote-splice expr]` at the top level of a `[quote ...]` (not inside a list/call args) is a parse error — Bawden (1999) Appendix A rejects `tag-comma-atsign?` at top level of `qq-expand`. (`doc/whatif/quasiquoting.md`, `doc/whatif/plans/macros-cluster.md` M3c) [Minor, computer-scientist train]
- [ ] M5a scope sets: document the biggest-subset binding resolution rule from Flatt (2016) §3.1 — the current M5a description says "distinct bindings with the same name but different ScopeIds do not capture each other" which is a simplification that may not handle recursive macros or nested macro definitions correctly. Add a note that the full scope-set model uses subset-based resolution, and that tinct's initial implementation is a simplification sufficient for non-recursive macros. (`doc/whatif/plans/macros-cluster.md` M5a, `doc/whatif/macros.md`) [Minor, computer-scientist train]
- [ ] M5a honest tags for Abstraction: Pombrio & Krishnamurthi (2015) Theorem 2 (Abstraction) requires "honest tags" — the expansion side map must record accurate before/after patterns, not just the call-site span. Note this requirement in the dual-span tracking design so that error provenance chains are faithful to the actual expansion. (`doc/whatif/plans/macros-cluster.md` M5a) [Minor, computer-scientist train]
- [ ] M4b blackhole detection for synthetic nodes: the plan uses `HashSet<(file_id, byte_offset)>` to track in-progress call sites, but macro-generating macros can produce NEW call sites with no source position. Specify how synthetic nodes (from `dict_to_ast` with absent `span:`) are tracked — e.g., assign synthetic node IDs or use the parent expansion's call site. (`doc/whatif/plans/macros-cluster.md` M4b task 11) [Minor, computer-scientist train]
- [ ] M5b dynamic include limitation as phase constraint: frame the static-paths-only limitation for `$include` macro ordering as a formal consequence of Flatt's (2002) phase separation model — compile-time imports must be resolved before expansion begins; dynamic paths cannot participate in phase separation. (`doc/whatif/plans/macros-cluster.md` §5 Cross-Cutting Concerns) [Minor, computer-scientist train]
