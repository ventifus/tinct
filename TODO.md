# Implementation Roadmap

See DONE.md for the full history of completed sprints.

For future work beyond the active sprints below, see:
- `doc/whatif/index.md §Adopt Now` — features ready to implement (String Interpolation Phase 1, Let Binding, Structural Contracts Phase 1, ADTs Phase 1, Source Snippets, Circular Dep Error Paths, Eval Semantics Verification Phase 1)
- `doc/whatif/index.md §Wait for Trigger` — features with complete designs pending a concrete trigger

## I/O and Capabilities

See doc/whatif/io.md.

- [x] Accept io — see doc/whatif/io.md (State: Accepted — 2026-05-04)







## Language Design Research

- [x] Research null semantics — see doc/whatif/null-semantics.md
- [x] Research tinct-hosted formatter — see doc/whatif/tinct-hosted-formatter.md and doc/whatif/ast-schema.md
- [x] Research macro-rewrite — see doc/whatif/macro-rewrite.md
- [x] Research parse-stage macros — see doc/whatif/parse-stage-macros.md
- [ ] Research Boolean-Algebraic Subtyping (BAS) as alternative foundation for D2 algebraic subtyping — Chau & Parreaux (POPL 2026) proves BAS encodes extensible records without row variables (one new term form, one typing rule, no subtyping changes). Complete soundness proofs. May supersede Marques et al. (2024) which lacks proofs. Write whatif evaluating BAS vs Rémy row variables for tinct's record model. Post-typing-cluster. Paper: doi:10.1145/3776689, preprint: https://lptk.github.io/files/boolean-algebraic-subtyping.pdf

## Template-Polarity Research

- [x] Research template-polarity embedding — see doc/whatif/template-polarity.md

## Type System Correctness and Performance

### type-annotation-fixes: Fix @Fn Unification and Cross-Kind Collision Detection

- [x] Change `resolve_type_name("Fn")` to return `Type::Any` instead of `Function { params:[], ret:Any, variadic:true }` — current encoding cannot unify with any concrete function type, producing false type errors for ~50 prelude functions annotated with `@Fn` [Major — computer-scientist, type-theorist]
- [x] Fix cross-kind collision detection: same name used as both type var and row var in one annotation scope should error rather than silently creating two independent bindings; implement unified `KindEnv` or single keyed map [Critical — type-theorist]
- [x] Fix comment syntax errors in `src/types.rs:1791,1800,1808` — single `/` should be `//` in the `generalize()` function [Major — type-theorist]
- [x] Document bare `@Fn` vs `Fn@T` distinction in `doc/06-type-inference.md:296`; document `@Dict` fresh-row-var-per-site behavior in `doc/05-type-annotations.md` [Minor — computer-scientist, type-theorist]
- [x] Fix `doc/02-syntax.md` EBNF: `named_arg_key` should be `identifier` only (remove `escaped_ref |`); add note that `$key: val` works in dicts but not in call named args [Critical — grammar-architect]
- [x] Remove stale bracket access examples from `doc/02-syntax.md` (lines ~106, 196, 380-381, 583-584 showing old `a[0]` syntax alongside new `[get key data]`) [Critical — grammar-architect]
- [x] Update `doc/11-stdlib.md` stale builtin count: change "59 Rust-native builtins" to "76 Rust-native builtins" [Major — stdlib-author]
- [x] Update `doc/10-errors.md` Part 8 implementation correspondence table: replace stale line numbers with function-name anchors [Major — integration-verifier]
- [x] Clarify `Expr::Pipe` lifecycle in `doc/15-ast.md`: present in post-parse AST, eliminated by desugar before type-check/eval [Major — grammar-architect]

## Test Coverage

### test-coverage-gaps: Missing Corpus and Unit Tests

- [ ] Add interpolated string corpus tests: `tests/corpus/valid/literals/interpolated_strings.llt-eval` (basic i"Hello $name", i"$$escaped", variable boundaries) and `tests/corpus/eval/builtins/interpolated_string_eval.llt-eval` (desugaring to `str` calls) [Critical — test-crafter]
- [ ] Add row polymorphism corpus tests: anonymous rest (`...`), named rest (`...r`), rest in function signatures, rest with field constraints in `tests/corpus/eval/type_system/row_*.llt-eval` [Critical — test-crafter]
- [ ] Add deeply chained access corpus tests: 5+ level chains mixing dot access (identifier and integer keys) and `get` builtin calls, and mid-chain error cases (`tests/corpus/eval/access/deeply_chained_mixed.llt-eval`) [Critical — test-crafter]
- [ ] Add pipeline section metadata corpus tests for `--- %name@Type` (output type annotation) and `--- expects: Type` (input contract) in `tests/corpus/eval/pipeline/` [Critical — test-crafter]
- [ ] Add annotation bracket restriction invalid tests: `x@[call f]`, `x@[fn [a] a]`, `x@[@Type e]` must all be parse errors (`tests/corpus/invalid/syntax_errors/annotation_special_form.llt-eval`) [Critical — test-crafter]
- [ ] Add static constraint corpus test: verify rest entry position `[a ... b]` is valid syntax (`tests/corpus/valid/edge_cases/rest_entry_positions.llt-eval`) [Major — test-crafter]
- [ ] Add Unicode identifier corpus tests (`tests/corpus/valid/literals/unicode_identifiers.llt-eval`) and escape sequence rejection tests for `\x41`, `A` (`tests/corpus/invalid/syntax_errors/unsupported_escape.llt-eval`) [Major — test-crafter]
- [ ] Add MAX_PARSE_DEPTH boundary tests: 256-nested-bracket file (valid) and 257 (error) in `tests/corpus/` [Major — test-crafter]
- [ ] Add CRLF formatter roundtrip unit test in `src/formatter.rs`: parse CRLF input, format, assert line endings preserved [Major — test-crafter]
- [ ] Add builtin limit enforcement tests for MAX_COLLECT_SIZE and MAX_STRING_SIZE in `tests/corpus/eval/errors/` [Minor — test-crafter]
- [ ] Add unit test module in `src/builtins.rs` for `builtin_each`, `builtin_each_key`, `builtin_each_kv`, `builtin_get` covering empty dict, multi-entry, offset-based recursion, and type errors [Critical — test-crafter]
- [ ] Add error corpus tests for `each`, `each-key`, `each-kv` called on non-Dict values in `tests/corpus/eval/errors/` [Critical — test-crafter]
- [ ] Add pipe operator precedence and associativity corpus tests: `x | f.g`, `[f x | g]`, `[a: x | f]`, `a | b | c` left-associativity in `tests/corpus/eval/access/` [Major — test-crafter]
- [ ] Add JSON formatter error corpus tests: Seq serialization error, Function/Builtin error in `tests/corpus/eval/errors/` [Major — test-crafter]
- [ ] Add pipe + each integration corpus tests: `dict | each | collect`, `d | each-key | map str`, three-stage `each-kv | filter | collect-kv` in `tests/corpus/eval/cross_feature/` [Major — test-crafter]

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
- [ ] `fn name_at_offset(expr: &Expr, span: Span, offset: usize) -> Option<String>`: recursive span-containment walk returning `VarRef.name.clone()` for the innermost `Expr::VarRef` at `offset`; recurses into Dict entries, Call func+args+named_args, Fn body, DotAccess, Pipe, TypeAlias, TypeAssert; returns `None` for literals, Error, Rest, Annotated, Fn params (`src/lsp/analysis.rs`)
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

- [ ] Default subcommand: if `argv[1]` is not a known subcommand (`eval`, `fmt`, `literate`, `hash`) and does not start with `-`, treat it as `tinct eval <argv[1]> [remaining args]`; enables `#!/usr/bin/env tinct` shebangs and `tinct my-script.llt` shorthand; `#!` shebang lines are already valid tinct (parsed as comments, ignored); no file-existence probing — pass the argument through to `tinct eval` which will produce a normal "file not found" error if needed (`src/main.rs`)
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

## Metaprogramming: AST-as-Data, Quasiquoting, Macros, Formatter

AST dict schema, quasiquoting, procedural macros, and tinct-hosted formatter. See `doc/whatif/plans/macros-cluster.md` for the full cluster plan, dependency graph, and decision gates.

- [x] Accept macros cluster — see doc/whatif/plans/macros-cluster.md (State: Accepted — 2026-05-05); covers ast-schema.md, quasiquoting.md, macros.md, tinct-hosted-formatter.md

### ast-dict-core: AST Dict Schema + `ast_to_dict` Minimal Mode

See doc/15-ast.md §AST Dict Schema. No dependencies.

- [ ] New `src/ast_dict.rs` with `AstToDictOpts` struct; `ast_to_dict_expr` covering all `Expr` variants with `type:` string discriminator and `span:` on every node; stub arms for `Quote`/`DefMacro` (later sprints) (`src/ast_dict.rs`)
- [ ] `ast_to_dict` wrapping `File → Document → expressions` hierarchy; root carries `schema-version: 1` (`src/ast_dict.rs`)
- [ ] Helpers: `annotation_to_dict`, `entry_to_dict`, `param_to_dict`, `span_to_dict`; `[]` for absent optional fields (`src/ast_dict.rs`)
- [ ] Tests: every `Expr` variant round-trips through `ast_to_dict_expr`; schema-version present; span on every node; type discriminator correct per variant (`tests/`)

### formatter-compact: Compact Formatter Modes in Tinct

See doc/12-tooling.md §Compact Formatter Modes. **Depends on:** `ast-dict-core`. **Supersedes:** `fmt-oneline` sprint (Rust implementation) — once this lands, remove the Rust compact formatter code.

- [ ] `stdlib/formatter/compact.llt`: `format-node` dispatch via `cond` chains (no `[match]` yet); section headers as `[str "; " ...]`; dicts as `[key: value ...]` space-separated (`stdlib/formatter/compact.llt`)
- [ ] CLI: `tinct fmt --oneline` / `--nospaces` / `--minimize` calls `ast_to_dict(None, None)` then evaluates `compact.llt` with AST dict as `%`; Rust formatter retained for `tinct fmt` (no flag) and LSP (`src/main.rs`)
- [ ] Tests: every `Expr` variant round-trips through compact formatter; output is re-parseable; idempotent; `--nospaces` and `--oneline` correct (`tests/`)

### quote: `[quote expr]` Special Form

See doc/02-syntax.md §Quasiquoting, doc/08-evaluation.md §Quote Semantics. **Depends on:** `ast-dict-core`.

- [ ] `quote` added to keyword denylist; `Expr::Quote(Box<Spanned<Expr>>)` AST variant (`src/lexer.rs`, `src/parser.rs`, `src/ast.rs`)
- [ ] Evaluator: `Expr::Quote` → `ast_to_dict_expr(inner, AstToDictOpts::minimal())` → return `Value::Dict`; no `unquote` handling yet (opaque Phase 2) (`src/eval.rs`)
- [ ] Type checker: `Quote → Dict`; formatter: `[quote ...]` round-trip; handle new variant in `eval_deep.rs`, `eval_materialize.rs`, `lsp/analysis.rs`, `lsp/document.rs` (`src/typecheck.rs`, `src/formatter.rs`, etc.)
- [ ] Tests: `[quote 42]` → literal dict; `[quote config.host]` → dot-access dict; `[type-of [quote x]]` → `"dict"` (`tests/corpus/eval/`)

### ast-dict-source: AST Dict Source Info + Comments

See doc/15-ast.md §AST Dict Schema. No blocking dependencies (extends `ast-dict-core`).

- [ ] `bare: true` on string literals when source char at token start ≠ `"` via `AstToDictOpts.source: Option<&str>` (`src/ast_dict.rs`)
- [ ] `leading-comments:`, `trailing-comment:`, `blank-before:` on `Entry` and `Document` nodes via `AstToDictOpts.comments` (`src/ast_dict.rs`)
- [ ] Tests: `bare: true` for bare-word strings; comment embedding; `blank-before: true`; both-`None` mode unchanged (`tests/`)

### unquote: `[unquote]` and `[unquote-splice]`

See doc/02-syntax.md §Quasiquoting, doc/08-evaluation.md §Quote Semantics. **Depends on:** `quote`.

- [ ] `unquote` and `unquote-splice` added to denylist; `Expr::Unquote`, `Expr::UnquoteSplice` AST variants (`src/lexer.rs`, `src/parser.rs`, `src/ast.rs`)
- [ ] Parser: nesting depth tracker; `unquote` outside `quote` is parse error; `[unquote-splice ...]` at top level of `[quote ...]` (not in list position) is parse error per Bawden (1999) (`src/parser.rs`)
- [ ] Evaluator: walk quoted AST for `Unquote`/`UnquoteSplice` subexpressions; evaluate and splice results (`src/eval.rs`)
- [ ] Tests: `[quote [+ [unquote x] 1]]` with `x: 42`; splice into args; `unquote` outside quote = error; top-level splice = error; nested depth preserved (`tests/corpus/eval/`)

### dict-to-ast: `dict_to_ast` + `eval-ast` Builtin

See doc/15-ast.md §dict-to-ast, doc/11a-builtins.md. **Depends on:** `ast-dict-core`.

- [ ] `dict_to_ast(v: &Value) -> Result<Expr, AstError>` — validate `type:` key; reconstruct `Expr`; unknown fields ignored; `span:` optional; `AstError` with `field_path` (`src/ast_dict.rs`)
- [ ] `eval-ast` builtin: `Dict -> Any` — calls `dict_to_ast`, evaluates in current environment; obeys capability model (`src/builtins.rs`)
- [ ] Tests: every `type:` value round-trips; missing `type:` → error; `eval-ast` executes constructed call node (`tests/`)

### defmacro: `[defmacro]` + Expansion Loop

See doc/08-evaluation.md §Macro Expansion Pipeline. **Depends on:** `quote`, `dict-to-ast`.

- [ ] `defmacro` added to denylist; `Expr::DefMacro` AST variant; parser: `[defmacro name [params] body]` (`src/lexer.rs`, `src/parser.rs`, `src/ast.rs`)
- [ ] New `src/expand.rs`: `expand_macros` top-down walk with `MacroEnv`; quotes args via `ast_to_dict_expr`, calls macro, `dict_to_ast` result, replaces node, re-expands (`src/expand.rs`)
- [ ] `DefMacro` handling: evaluate body in fresh `EvalContext` (inherits `EvalConfig`); register in `MacroEnv`; remove from AST after registration (`src/expand.rs`)
- [ ] Termination: depth limit 100 + 100k node-count cap; `HashSet<(file_id, byte_offset)>` for in-progress tracking; `SyntheticId(u64)` for generated nodes (`src/expand.rs`)
- [ ] `gensym: [] -> Str` builtin — `:gensym:N` names with forbidden-char prefix (`src/builtins.rs`)
- [ ] Namespace: macros cannot shadow registered Rust builtins — error at registration time (`src/expand.rs`)
- [ ] Pipeline update in `src/main.rs` **and** `src/lsp/document.rs`: insert `expand_macros` between parse and desugar (`src/main.rs`, `src/lsp/document.rs`)
- [ ] Handle new variants in `eval_deep.rs`, `eval_materialize.rs`, `lsp/analysis.rs` (`src/`)
- [ ] Tests: `[defmacro my-when ...]` expands; `gensym` unique; depth limit hit; node-count cap; `DefMacro` absent post-expansion; `[defmacro str ...]` rejected; LSP diagnostics correct (`tests/`)

### formatter-full: Full Tinct Formatter

See doc/12-tooling.md §Tinct-Hosted Formatter. **Depends on:** `ast-dict-source`, typing-cluster `let-binding`, typing-cluster `pattern-matching-basic`.

- [ ] Add `str-repeat: Str -> Int -> Str` to `stdlib/prelude.llt` (pure-tinct one-liner using `$reduce` over `$range`) (`stdlib/prelude.llt`)
- [ ] Add `str-length: Str -> Int` Rust builtin (`src/builtins.rs`)
- [ ] `stdlib/formatter/format.llt`: `format-node` dispatch via `[match ...]`; `fits-inline?` via `[str-length [render-inline node]]`; comment/blank-line preservation; evaluated with prelude loaded (`stdlib/formatter/format.llt`)
- [ ] `src/main.rs`: `tinct fmt` (no flag) evaluates `format.llt`; Rust formatter retained as `format_source_rust()` for LSP (`src/main.rs`)
- [ ] Tests: existing formatter corpus passes; idempotent; comments preserved; blank lines preserved; re-parseable (`tests/`)

### macro-hygiene: Scope Sets + Dual-Span Error Reporting

See doc/08-evaluation.md §Macro Hygiene. **Depends on:** `defmacro`.

- [ ] `ScopeId(u32)` type; `ScopeMap` threaded through expander; each invocation gets fresh scope; bindings carry definition-site scope; call-site variables carry caller scope (`src/expand.rs`)
- [ ] Name resolution: same name + different `ScopeId` = distinct (simplified biggest-subset rule, sufficient for non-recursive macros) (`src/expand.rs`)
- [ ] Dual-span side map: `HashMap<NodeKey, (String, Span, usize)>` — `(macro_name, call_site_span, expansion_rule_index)` for honest tags per Pombrio & Krishnamurthi (2015) (`src/expand.rs`)
- [ ] Error formatter: shows "in expansion of `<name>` at line N" with provenance chains for nested expansions (`src/`)
- [ ] Tests: macro binding `x` does not capture caller's `x`; error shows call site; nested provenance chain; existing macros still work (`tests/`)

### macro-integration: Include Ordering, `_` Port, Formatter Config

See doc/08-evaluation.md §Macro Expansion Pipeline. **Depends on:** `macro-hygiene`, `unquote`, `formatter-full`.

- [ ] Include ordering: `expand_macros` runs on statically-included files first; macro definitions registered in `MacroEnv` before includer expansion; static-path-only constraint documented as Flatt (2002) phase separation consequence; `IncludeContext` cache bypass or `(EvalResult, MacroEnv)` tuples (`src/expand.rs`)
- [ ] Port `_` desugaring: replace `desugar_underscore()` Rust pass with `[defmacro desugar-underscore ...]`; remove Rust pass atomically; all existing underscore corpus tests pass unchanged (`src/desugar.rs`, `src/expand.rs`)
- [ ] Formatter config: `max-width:` and `max-entries:` named params; `tinct fmt --width 100 --max-entries 6`; `tinct fmt --formatter path/to/custom.llt` (`src/main.rs`, `stdlib/formatter/format.llt`)
- [ ] Tests: included file's macros available; `_` macro matches prior Rust output; `--formatter` override works; `--width 100` changes layout (`tests/`)

## Macros Cluster: Theoretical Gaps

Gaps between the macros-cluster plan (`doc/whatif/plans/macros-cluster.md`) and the theoretical requirements established by the cited papers. Track here; resolve during the relevant sprint.

- [x] M3c unquote-splice top-level error: specify that `[unquote-splice expr]` at the top level of a `[quote ...]` (not inside a list/call args) is a parse error — Bawden (1999) Appendix A rejects `tag-comma-atsign?` at top level of `qq-expand`. (`doc/whatif/quasiquoting.md`, `doc/whatif/plans/macros-cluster.md` M3c) [Minor, computer-scientist train]
- [x] M5a scope sets: document the biggest-subset binding resolution rule from Flatt (2016) §3.1 — the current M5a description says "distinct bindings with the same name but different ScopeIds do not capture each other" which is a simplification that may not handle recursive macros or nested macro definitions correctly. Add a note that the full scope-set model uses subset-based resolution, and that tinct's initial implementation is a simplification sufficient for non-recursive macros. (`doc/whatif/plans/macros-cluster.md` M5a, `doc/whatif/macros.md`) [Minor, computer-scientist train]
- [x] M5a honest tags for Abstraction: Pombrio & Krishnamurthi (2015) Theorem 2 (Abstraction) requires "honest tags" — the expansion side map must record accurate before/after patterns, not just the call-site span. Note this requirement in the dual-span tracking design so that error provenance chains are faithful to the actual expansion. (`doc/whatif/plans/macros-cluster.md` M5a) [Minor, computer-scientist train]
- [x] M4b blackhole detection for synthetic nodes: the plan uses `HashSet<(file_id, byte_offset)>` to track in-progress call sites, but macro-generating macros can produce NEW call sites with no source position. Specify how synthetic nodes (from `dict_to_ast` with absent `span:`) are tracked — e.g., assign synthetic node IDs or use the parent expansion's call site. (`doc/whatif/plans/macros-cluster.md` M4b task 11) [Minor, computer-scientist train]
- [x] M5b dynamic include limitation as phase constraint: frame the static-paths-only limitation for `$include` macro ordering as a formal consequence of Flatt's (2002) phase separation model — compile-time imports must be resolved before expansion begins; dynamic paths cannot participate in phase separation. (`doc/whatif/plans/macros-cluster.md` §5 Cross-Cutting Concerns) [Minor, computer-scientist train]

## Typing Cluster: Theoretical Gaps

Gaps between the typing-cluster plan (`doc/whatif/plans/typing-cluster.md`) and the theoretical requirements established by the cited papers. Track here; resolve during the relevant sprint.

- [x] C5 divergent values in coverage analysis: the C5 exhaustiveness sprint now implements the full Maranget (2007) usefulness algorithm with lazy ⊥-as-constructor extension, yielding the Karachalias et al. (2015) three-way partition (Covered/Divergent/Uncovered). Coverage algorithm in Rust (`src/coverage.rs`), exposed as `check-coverage` builtin. Inaccessible-RHS warnings from divergent-useful detection. (`doc/whatif/plans/typing-cluster.md` C5) [Minor, computer-scientist train]
- [ ] D2 Marques et al. soundness unproven: the D2 algebraic subtyping sprint cites Marques et al. (2024) as a "direct implementation template" for row variables under algebraic subtyping. However, the paper explicitly states soundness and completeness proofs "do not have yet done" (§1). Decision: proceed with Marques et al. (Path A), accepting the risk. Risk is documented in typing-cluster.md D2 §Formal model and §Risk. Alternative identified for future research: Chau & Parreaux (POPL 2026) BAS — see §Language Design Research. (`doc/whatif/plans/typing-cluster.md` D2) [Major, computer-scientist train]
- [x] D4 Greenman et al. venue correction: the §References section lists "ICFP '19" for Greenman, Felleisen & Dimoulas (2019). The correct venue is OOPSLA '19 (Proc. ACM Program. Lang. 3, OOPSLA, Article 122, doi:10.1145/3360548). Fixed in typing-cluster.md, gradual-typing.md, and doc/17-references.md. (`doc/whatif/plans/typing-cluster.md` §References) [Nit, computer-scientist train]
- [x] Occurrence typing tasks missing: narrowing added as proposal #12 to typing-cluster plan with two sprints: B5a `narrowing-basic` (8 tasks: `if` as type-level special form, `Narrowing` enum, `extract_narrowings`, environment forking, branch type join, conjunction, type map, tests) and B5b `narrowing-predicates` (5 tasks: `int?`/`str?`/etc. direct narrowing, `num?` supertype, `cond` narrowing, tests). Wired into dependency graph, implementation calendar (weeks 7-8), and cross-cutting concerns (§5 items 6-7). (`doc/whatif/plans/typing-cluster.md` B5a, B5b) [Minor, computer-scientist train]
- [x] B4 constraint duplication during instantiation: proof obligation now stated in typing-cluster.md §5 Cross-Cutting Concerns item 4 — each fresh variable carries exactly the constraints from the generalized scheme (Jones 1995, §8.3), not accumulated constraints from the current inference state. (`doc/whatif/plans/typing-cluster.md` B4) [Minor, computer-scientist train]
