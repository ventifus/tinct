# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## CHR Unification

`chr-unification` accepted 2026-05-16 (commits 0886ef1, 7d15c36). See `doc/whatif/chr-unification.md` and `doc/feature/chr-unification.md`. Implementation order: chr-module-split → chr-normalization → chr-class-instance → chr-prelude.


### chr-class-instance: AST redesign and parser/typecheck support for [class] and [instance]

Redesigns `Expr::ClassDecl` and `Expr::InstanceDecl` for the two-bracket class body and match-arm instance syntax. New `[pattern [...]]` form reuses existing annotated-identifier machinery.

- [x] Extend `Expr::ClassDecl` in `src/ast.rs`: add `determines`, `resolver` fields; update all exhaustive match sites (`src/ast.rs` + ~8 files)
- [x] Update `StackFrame::ClassDecl` in `src/parser.rs`: structural_metadata second bracket; extract determines/resolver/kinds/superclasses; key extraction handles VarRef+Str (`src/parser.rs`)
- [x] Redesign `Expr::InstanceDecl` in `src/ast.rs`: arms Vec form; backward-compat legacy_arm_pattern; update all exhaustive match sites (`src/ast.rs` + ~8 files)
- [x] Update `StackFrame::InstanceDecl` in `src/parser.rs`: arms/pending_arm_pattern; pattern arm syntax; legacy support (`src/parser.rs`)
- [x] Add `Expr::PatternDecl { bindings }` to `src/ast.rs` + `StackFrame::PatternDecl`; colon-ahead rejection guard (`src/ast.rs`, `src/parser.rs`)
- [x] Implement ClassDecl typecheck: determines/resolver validation, coverage, consistency; 6-field probe isolation in patterns_overlap (`src/typecheck.rs`)
- [x] Implement InstanceDecl typecheck: disjointness, coverage, consistency, InstanceEnv registration, all-arms iteration, VarRef method keys (`src/typecheck.rs`, `src/eval.rs`)
- [x] Tests: class_fd_basic.llt-eval, instance_pattern_basic.llt-eval, instance_pattern_syntax.llt-eval; unit test for FD consistency violation (`tests/corpus/eval/typecheck/`, `src/lib.rs`)
- [x] Remove legacy instance syntax: removed `legacy_arm_pattern` field, `[instance [ClassName Type] ...]` now produces parse error (`src/parser.rs`, `tests/corpus/invalid/instance_legacy_syntax_rejected.llt-eval`)

### chr-prelude: Migrate arithmetic classes to prelude.llt and implement boundary guard elaboration

Moves the hardcoded arithmetic instance table out of Rust and into tinct itself. Completes the CHR cycle by adding the post-inference boundary guard elaboration pass.

- [x] Add iteration cap (100) to `process_deferred_equalities()` (`src/type_unify.rs`)
- [x] Add corpus test for `determines:` extraction round-trip (`tests/corpus/eval/typecheck/class_determines_roundtrip.llt-eval`)
- [ ] Fix consistency check to use unify-under-θ instead of structural equality (`types_equal`) — deferred, O(N²) performance risk for large prelude (`src/typecheck.rs:2400`)
- [x] Improve disjointness/consistency error spans: both arm spans included (`src/typecheck.rs`)
- [x] Coverage error message: uses param name from `params` list (`src/typecheck.rs`)
- [x] Add `instance_resolution_depth: u32` to `InferState`; guard `resolve_instance` call in `check_constraints_on_var` (limit 64, matching GHC `-freduction-depth` per Sulzmann et al. 2007 §3.2); **unblocks all remaining chr-prelude and unified-bindings-migrate work** (`src/type_unify.rs`, `src/type_infer.rs`)
- [x] Add `in_prelude_load: bool` flag to `InferState`; skip InstanceDecl method body inference during prelude load (`src/type_infer.rs`, `src/typecheck.rs`, `src/imports.rs`)
- [x] Wire boundary guards from typecheck to eval pipeline: `boundary_guards` on EvalContext, `set_boundary_guards()` method; wired in `eval_source_with_config`, `eval_source_with_cap_net`, `run_eval` (`src/eval.rs`, `src/lib.rs`, `src/main.rs`)
- [x] Remove backward-compat legacy instance parsing — `legacy_arm_pattern` field removed, old syntax now produces parse error; 7 test files converted (`src/parser.rs`)
- [x] Write resolver functions (AddResult/SubResult/MulResult/DivResult) in `--- stage: type` section + arithmetic class declarations with `[determines: [...] resolver: ...]` + migrate 27 instances to `[instance ClassName [pattern [...]]: [...]]` syntax + 16 new arithmetic instances (`stdlib/prelude.llt`)
- [x] NormCtxt resolver_cache pre-populated (16 entries); `improve_functional_dependency` has `fd_depth` guard with `MAX_FD_DEPTH=16` (`src/type_normalize.rs`, `src/type_unify.rs`)
- [x] `boundary_guards: Vec<(Span, Type)>` added to InferState; collected at CALL-MONO and CALL-POLY boundaries (`src/type_infer.rs`, `src/typecheck.rs`)
- [x] Wire boundary guards to eval: create guarded thunks from `state.boundary_guards`; eval-side `ThunkState::Guarded` with BlameLabel (`src/eval.rs`)
- [x] Tests: full arithmetic FD + boundary guard tests (blocked on resolver activation) — boundary guard tests added (4 unit tests; FD tests remain blocked)

---

## Unified Binding Declarations

`unified-bindings` accepted 2026-05-17. See `doc/whatif/unified-bindings.md` and `doc/02-syntax.md` §6, §9. Implementation order: unified-bindings-ast → unified-bindings-typecheck → unified-bindings-migrate.

- [x] Design unified bindings — see doc/whatif/unified-bindings.md

### unified-bindings-ast: Lexer, AST, and parser for [let ...], [case ...], and ... placeholder

Add `Token::Let`, `Token::Case`, `Expr::LetDecl`, `Expr::CaseArm`, `Expr::Placeholder` and parser support. Both old and new binding syntax accepted during this phase (old syntax deprecated but functional to avoid breaking everything at once). **Spec chapters:** `doc/02-syntax.md §6, §9`, `doc/whatif/unified-bindings.md §src/lexer.rs, §src/ast.rs, §src/parser.rs`.

- [x] Add `Token::Let` and `Token::Case` keywords to `src/lexer.rs`; reserved keyword denylist (`src/lexer.rs`)
- [x] Add `Expr::LetDecl { bindings }`, `Expr::CaseArm { pattern, body }`, `Expr::Placeholder` to `src/ast.rs`; updated all exhaustive match sites (~15 files) (`src/ast.rs` + all consumers)
- [x] Add `StackFrame::LetDecl` and `StackFrame::CaseDecl` to `src/parser.rs`; push_value + CloseBracket handlers (`src/parser.rs`)
- [x] Parse `Expr::Placeholder`: bare `...` (not followed by identifier) outside Dict context → `Expr::Placeholder` (`src/parser.rs`)
- [x] Colon-ahead disambiguation for `let:` and `case:` via peek_next_horizontal (`src/parser.rs`)
- [x] Fn/ClassDecl/TypeAlias/InstanceDecl/Match accept `Expr::LetDecl`/`Expr::CaseArm` (old syntax still works) (`src/parser.rs`)
- [x] ErrorKind::Unimplemented added for Placeholder eval; all new AST nodes have Display/typecheck/eval stubs (`src/error.rs`, `src/eval.rs`, `src/typecheck.rs`)

### unified-bindings-typecheck: Type checker and evaluator for binding declarations, case arms, and placeholders

**Depends on:** `unified-bindings-ast`

- [x] LetDecl binding extraction: fn params already handled by parser; extract_pattern_types updated to accept both PatternDecl and LetDecl (`src/typecheck.rs`)
- [x] typecheck_case_arm: BAS narrowing (scrutinee_ty ∩ annotation_type); LetDecl/exact-value patterns; Type::Unknown ∩ T = T in normalize_intersection (`src/typecheck.rs`, `src/type_def.rs`)
- [x] LetDecl validity check: TypeError "not valid in expression position" (`src/typecheck.rs`)
- [x] Expr::Placeholder → Type::Unknown with gradual typing doc comment (`src/typecheck.rs`)
- [x] eval_case_arm, eval_let_pattern, nullary variant values_equal; CaseArm routing in Match frame; Placeholder → EvalError::unimplemented (`src/eval.rs`, `src/parser.rs`)
- [x] Tests: case_arm_basic, let_decl_fn_param, let_decl_wildcard, placeholder_unimplemented corpus tests; 9 new type checker unit tests (`tests/corpus/eval/`, `src/typecheck.rs`)

### unified-bindings-migrate: Migrate all existing code to [let ...] and [case ...] syntax

**Depends on:** `unified-bindings-typecheck`

- [x] Migrate ~242 fn declarations in `stdlib/prelude.llt` to `[fn [let params] body]` (`stdlib/prelude.llt`)
- [x] Migrate class/type/instance declarations in `stdlib/prelude.llt` (`stdlib/prelude.llt`)
- [x] Migrate all corpus test files and doc examples (`tests/corpus/`, `doc/`)
- [x] Remove old param-list parsing paths (old syntax becomes a parse error) (`src/parser.rs`)
- [x] Verify `just test` passes with all migrations applied (`tests/`)
- [x] Run `just doc` for the first time and commit the annotated output — populates `=== out`/`=== warn`/`=== info` sections inside each ```tinct block, making the docs self-verifying living documentation; add `just doc-verify` to CI (exits 1 if any annotated block's actual output diverges from its `===` sections) (`justfile`, CI config, `doc/*.md`)

---

## Codebase Health

### unknown-elimination: Replace remaining `Type::Unknown` builtin signatures with precise types

First-pass audit complete (2026-05-16). The following categories of Unknown remain and require future work:

**Category B — TypeVar polymorphism required (HKT or multi-arity):**
- `map`, `filter`, `reduce`: target `∀f a b. Mappable f => (a→b)→f a→f b`. Requires higher-kinded types (Type::App) not yet representable in TypeScheme. See comment `// TODO(unknown-elimination)` in each signature.
- `each`, `each-key`, `each-kv`: return element type requires HKT over input collection type.
- `builtin-collect`: `Seq(Unknown)` param; return Dict erases element type anyway — low priority.

**Category A — Record return types (closed Record schema needed):**
- `revocable`: returns `{cap: DirCap, revoke: Fn()->Null}` — expressible once Rust builtin signatures support closed Record return types.
- `recv-datagram`: returns `{data: Bytes, addr: Str, port: Int}`.
- `tls-peer-cert`: returns `{subject: Str, issuer: Str, sans: Seq(Str), ...}`.
- `icmp-ping`: returns `{rtt_ms: Int, success: Bool}`.
- `http-request`: returns `{status: Int, headers: Map(Str,Str), body: Bytes}`.
- `list-dir`: returns `Seq({name: Str, kind: Str, size: Int, ...})`.
- `stat`: returns `{name: Str, kind: Str, size: Int, ...}`.
- `timestamp-parts`: returns `{year: Int, month: Int, day: Int, hour: Int, minute: Int, second: Int}`.
- `timestamp-in-tz`: returns the above plus `offset-seconds: Int, tz-name: Str`.
- `builtin-first`/`builtin-last`: return type depends on input type (Dict element, Str char, Int byte).

**Category A — Genuinely unknown (no precise type possible without language feature):**
- `from-json`: requires schema-directed parsing; return is `Unknown` by design.
- `include`: included file type not knowable without parsing the included file at type-check time.
- `builtin-get`/`get?`: special-cased by `check_get` dispatcher; label-polymorphic scheme (`HasField l d a`) was attempted but reportedly caused inference to hang on prelude.llt (informal O(N²) analysis: ~35 `get` calls × HasField constraints × substitution merge loop); unproven whether this was a true performance issue or a unification bug — worth re-investigating once chr-class-instance lands a better HasField implementation.
- `map`/`filter`/`reduce` seq/init params: HKT required.
- `builtin-join` seq param: `stringify()` accepts any element type.
- `builtin-concat` return: merge shape not inferrable statically.
- Transport variant constants (`Tcp`, `Udp`, etc.): requires `Type::Variant`.
- `connect` transport param: requires `Type::Variant` for dispatch.
- `Map` unparameterized constructor: `Unknown` K/V until user supplies type args.

**Tasks:**
- [ ] Implement `Type::Variant` and replace Transport constant `Unknown` registrations (`src/type_env.rs`, `src/types.rs`)
- [x] Add closed-Record return type for `revocable`, `icmp-ping`, `recv-datagram`, `stat`, `timestamp-parts`, `timestamp-in-tz`, `timestamp-in-tz`, `tls-peer-cert`, `http-request` (`src/type_env.rs`)
- [x] Add precise `Seq({...})` return for `list-dir` — `Seq({name: Str, kind: Str, size: Int})` (`src/type_env.rs`)
- [ ] Implement HKT (`Type::App`) to express `map`/`filter`/`reduce`/`each` precisely — see `chr-unification` sprint for the type-application machinery
- [ ] After above: add `from-json` option for schema-directed typed parse returning a specific Record type

---

## Test Infrastructure

### corpus-consolidation: Consolidate corpus tests into fewer, more comprehensive test cases

**Depends on:** `literate-flags` — that sprint adds `=== info` as a new section label
(for `log`/stdout output) alongside the existing `=== out`/`=== warn`/`=== error`.
Corpus consolidation must use `=== info` for any tests that exercise `log` or other
stdout-producing builtins, so the test infrastructure is consistent with `tinct literate`.

The corpus test suite has grown to hundreds of fine-grained single-feature tests. The goal is to reduce the total number while increasing coverage density per test — each consolidated test should exercise multiple related features together (e.g., a single `arithmetic_mixed_types.llt-eval` that covers Int+Int, Int+Float, Float+Int, Float+Float and their type annotations, rather than 4 separate files). This reduces the serial test execution time (currently 700+ seconds for the full corpus).

**Strategy:** Merge tests within the same subdirectory that share the same builtin or feature area. Keep negative/error tests separate (one file per distinct error code is fine). Target: reduce corpus file count by 30-40%. Use `=== info` for expected log/stdout output in consolidated tests.

- [x] Audit `tests/corpus/eval/builtins/` — merge arithmetic variants, string operation variants, and type-predicate variants into composite tests; use `=== info` for any `log` output (`tests/corpus/eval/builtins/`)
- [x] Audit `tests/corpus/eval/typecheck/` — merge related positive typecheck tests into 1-3 comprehensive files per feature area (`tests/corpus/eval/typecheck/`)
- [x] Audit `tests/corpus/eval/stdlib/` — merge related prelude function tests; use `=== info` for `log` output (`tests/corpus/eval/stdlib/`)
- [x] Verify `just test` passes after consolidation; update any CI time baselines (`tests/`)

---

## Codebase Cleanup

### remove-emitted-flag: Remove EvalContext.emitted and make emit additive

`EvalContext.emitted` was added before the tinct-native output formatter (`-o` flag /
`json.llt`). It currently serves two purposes:

1. **JSON suppression** — when `emit` is called, final JSON serialization is skipped.
   This is the obsolete behaviour: now that `-o` handles output formatting through the
   tinct pipeline, `emit` can be additive (write to stdout without suppressing the
   final result).

2. **Seq drain gating** — when the final value is a `Seq` and `emitted=true`, the Seq
   is drained to completion to fire all emit side-effects in generator elements. This
   behaviour should be preserved by a different mechanism (always drain Seq if it is the
   final value, regardless of emit calls).

After this change, `emit` is purely additive: calling it one or more times writes strings
to stdout AND the final expression is still serialized. This makes `emit` usable as the
logging/print primitive and unblocks `stdlib/log.llt`.

- [x] Remove `emitted: Cell<bool>` from `EvalContext` (`src/eval.rs`, `src/lib.rs`)
- [x] Remove all `eval_ctx.emitted.get()` / `emitted.set(true)` sites; update
  `run_eval`, `run_literate_eval`, `run_literate_weave` accordingly (`src/main.rs`)
- [x] Preserve Seq drain: always drain a Seq final value to completion (remove the
  `emitted.get()` gate, make drain unconditional) (`src/main.rs`)
- [x] Update `doc/09-documents.md` §Interaction with `emit` — remove language about
  JSON suppression (`doc/09-documents.md`)
- [x] Update corpus tests that rely on emit-suppression behaviour (`tests/corpus/`)
- [x] Verify `just test` passes (`tests/`)

---

## VS Code Extension

### vscode-corpus-grammar: Colorize corpus/literate format in source and markdown preview

**Depends on:** `literate-flags` — defines the `=== info` section label and the
corpus-in-markdown format. This sprint wires the grammar support for both `.llt-eval`
files and tinct code blocks in markdown that contain `===` sections.

Two contexts need colorization:
1. **`.llt-eval` files** (corpus tests) — currently unregistered; get tinct grammar up to
   the first `===` line, then section-specific coloring for markers and their content.
2. **Tinct code blocks in markdown** containing `===` sections — the current
   `tinct.markdown-injection.json` embeds plain `source.llt` for the whole block, missing
   the section structure introduced by literate-flags.

**Grammar design** (`integrations/vscode/syntaxes/tinct.corpus.tmLanguage.json` — new):

State machine: tinct-code-mode → `=== section` marker → output-mode → next marker.

Scope names (map to standard theme colors):
```
=== out   line: markup.heading.output.corpus.llt        # neutral / heading color
=== warn  line: keyword.control.warning.corpus.llt      # yellow in most themes
=== error line: invalid.illegal.corpus.llt              # red
=== info  line: keyword.control.info.corpus.llt         # blue/green
output content: string.unquoted.output.corpus.llt       # dimmed, distinct from code
tinct code portion: embedded source.llt (unchanged)
```

- [x] Create `integrations/vscode/syntaxes/tinct.corpus.tmLanguage.json` — TextMate
  grammar with state machine: tinct code (embed `source.llt`) until first `^===` line;
  `=== (out|warn|error|info)` header with section-specific scope; content after header
  with `string.unquoted.output.corpus.llt` scope until next `===` or end-of-block
  (`integrations/vscode/syntaxes/`)
- [x] Register `.llt-eval` file type in `integrations/vscode/package.json` with the new
  corpus grammar; associate with `llt-eval` language id (`integrations/vscode/package.json`)
- [x] Update `integrations/vscode/syntaxes/tinct.markdown-injection.json` — switch the
  content grammar from plain `source.llt` to `source.llt.corpus` (the new grammar) so
  that tinct markdown blocks with `===` sections are colored correctly in both the editor
  and markdown preview (`integrations/vscode/syntaxes/tinct.markdown-injection.json`)
- [x] Verify colorization in: `.llt-eval` file open, markdown source with `===` blocks,
  markdown preview pane with `===` blocks (`integrations/vscode/`)
- [x] Add `vsce package` to `just ext` and confirm the `.vsix` includes the new grammar
  files (`justfile`, `integrations/vscode/`)

### vscode-markdown-lsp: LSP hover/completion/diagnostics inside ```tinct blocks in markdown

**Depends on:** `literate-flags` — adds `=== info` label and corpus-in-markdown format.

Implementation: **Option B — server-side markdown support** in `tinct lsp`. This is the
correct approach because `tinct lsp` and `tinct literate` are siblings in the same binary,
sharing `src/literate.rs`. The LSP server reuses `extract_code_blocks` directly rather
than duplicating position-mapping logic in TypeScript.

In VS Code, multiple LSP servers for the same file type are additive, not exclusive —
the built-in markdown support (link completion, preview, header folding) is unaffected.
`tinct lsp` returns `null` for positions outside tinct blocks (files with no tinct content
cost a few microseconds). The extension simply adds `.md` to its `DocumentSelector`.

The `===` section markers are handled naturally: the server already parses blocks by
splitting on `===` (via literate mode), so the code portion is extracted correctly before
any LSP analysis.

**Shared infrastructure first** — before adding LSP support, factor the literate
machinery out of `src/main.rs` into `src/literate.rs` as a proper library API. Both
`tinct literate` (CLI) and `tinct lsp` consume this API; neither reimplements it.

Core types and functions to expose from `src/literate.rs`:

```rust
pub struct LiterateBlock {
    pub code: String,               // tinct source (before first === line)
    pub expectations: Expectations, // === out/warn/info/error sections
    pub md_start: usize,            // byte offset of ``` fence in markdown
    pub md_code_start: usize,       // byte offset of first code line
    pub md_code_end: usize,         // byte offset of closing ``` fence
}

pub fn extract_blocks(markdown: &str) -> Vec<LiterateBlock>;

// Position mapping — shared by CLI (weave span annotation) and LSP (hover/diagnostics)
pub fn md_offset_to_block(blocks: &[LiterateBlock], md_offset: usize)
    -> Option<(usize, usize)>;  // (block_index, block_relative_offset)
pub fn block_span_to_md(blocks: &[LiterateBlock], block_idx: usize, span: Span) -> Span;
```

Evaluation stays in `src/main.rs` (CLI path). LSP analysis stays in `src/lsp/`. Only the
block extraction and position mapping live in `src/literate.rs` — the shared kernel.

- [x] Refactor `src/literate.rs`: define `LiterateBlock`, `extract_blocks`,
  `md_offset_to_block`, `block_span_to_md`; move block extraction logic out of
  `run_literate_weave`/`run_literate_eval` in `src/main.rs` (`src/literate.rs`,
  `src/main.rs`)
- [x] Add `===` section parsing to `LiterateBlock.expectations` — reuse
  `TestExpectations` or factor a shared `Expectations` type; both corpus tests and
  literate mode use the same parser (`src/literate.rs`, `tests/corpus_tests.rs`)
- [x] Extend `tinct lsp` to accept `textDocument/didOpen`/`didChange` for `.md` files;
  call `literate::extract_blocks` and cache `Vec<LiterateBlock>` per document
  (`src/lsp/`, `src/literate.rs`)
- [x] Handle `textDocument/hover` for `.md`: `md_offset_to_block` → run existing hover
  analysis on `block.code` at block-relative position → `block_span_to_md` for response
  span (`src/lsp/analysis.rs`, `src/literate.rs`)
- [x] Handle `textDocument/publishDiagnostics` for `.md`: collect diagnostics from block
  typecheck, `block_span_to_md` each span to markdown coordinates (`src/lsp/`)
- [x] Add `{ language: 'markdown' }` to the extension's `DocumentSelector`; no other
  extension changes needed (`integrations/vscode/package.json`)
- [x] Handle completion for `.md` (lower priority); preview hover requires webview bridge
  (lower priority) (`src/lsp/`, `integrations/vscode/`)

---

## Prelude Annotation Modernization

### prelude-triple-quote: Migrate prelude doc: strings to triple-quoted form

`"""..."""` is fully implemented (lexer `Token::TripleQuotedString`, parser desugars to `[unindent "..."]`). The `doc:` strings in `stdlib/prelude.llt` currently use `\n` escape sequences in regular double-quoted strings. Replace with `"""` for readability.

- [x] Replace all `doc: "...\n\n..."` multi-line strings in `stdlib/prelude.llt` with `doc: """..."""` triple-quoted form; use natural indentation for Example: and Note: sections (`stdlib/prelude.llt`)
- [x] Verify `just test-lib` passes; doc string content unchanged (`stdlib/prelude.llt`)

---

## tinct literate Hardening

### literate-flags: --strict, -i/--in-place, --errors-in-doc, fixed clock, and capability flags

Four improvements to `tinct literate` to make `just doc` robust and safe.

**`--strict` mode** — mirror `tinct run --strict`: type errors are fatal (exit 1), rich
parse error diagnostics via `format_parse_error`. Currently literate mode uses basic error
formatting and type errors are non-fatal. Needed for `git diff --exit-code` CI gate to
catch type regressions in doc examples, not just parse failures.

- [x] Add `--strict` flag to `tinct literate` subcommand; wire to the same `strict: bool`
  path used by `run_eval` (`src/main.rs`)
- [x] Use `format_parse_error` for parse errors in literate strict mode (`src/main.rs`,
  `src/parser.rs`)
- [x] Update `just doc` justfile target to pass `--strict` (`justfile`)

**`-i` / `--in-place`** — write weaved output back to the source file atomically instead
of stdout. Replaces the fragile `> tmp && mv` shell idiom in `just doc`.

- [x] Add `-i` / `--in-place` flag to `tinct literate weave`; write to `.tmp` then rename
  (`src/main.rs`)
- [x] Update `just doc` justfile target to use `tinct literate weave --strict -i doc/*.md`
  (`justfile`)

**Corpus-test format inside code blocks** — use the same `=== out` / `=== warn` /
`=== error` / `=== info` section format as corpus test files (`.llt-eval`), embedded
directly inside the tinct code block. The literate parser splits on `===` markers exactly
as the corpus test runner does, reusing `TestExpectations` parsing logic. Blocks without
`===` sections have no expectations and are ignored by `--verify`.

````markdown
```tinct
[log "hello"]
[+ 1 2]
=== warn
[T003] undefined variable: x
=== info
hello
=== out
3
```
````

The `===` sections are visible in rendered markdown — readers see code and expected output
inline, without needing HTML rendering to surface results. The `=== info` section is new
(not in corpus tests) and captures `log`/stdout output.

**Weave (`--in-place`):** evaluate the code portion (everything before the first `===`
line); update/insert `=== out`, `=== warn`, `=== info` sections with actual results.
Replaces the previous `<!-- tinct-result: ... -->` HTML comment approach.

**`--verify`:** compare actual output against the expected `===` sections. Blocks without
`===` sections pass vacuously (allows incremental annotation). Exits 0 if all annotated
blocks match, exits 1 with a diff-style report if any mismatch.

- [x] Extend `literate::extract_code_blocks` (or add a new extractor) to split each block
  on `===` markers, returning `(code: &str, expectations: TestExpectations)` — reuse
  `TestExpectations` from `tests/corpus_tests.rs` or factor into `src/literate.rs`
  (`src/literate.rs`, `tests/corpus_tests.rs`)
- [x] Update `run_literate_weave --in-place` to evaluate code portion and update/insert
  `=== out`, `=== warn`, `=== info` sections in-place; remove the old
  `<!-- tinct-result: ... -->` HTML comment output (`src/main.rs`)
- [x] Add `--verify` flag to `tinct literate weave`; compare actual against expected
  sections; exit 1 with diff-style details on mismatch; blocks without `===` pass
  vacuously (`src/main.rs`)
- [x] Add `=== info` as a new section label alongside `=== out`/`=== warn`/`=== error`
  in the corpus test parsing infrastructure (`tests/corpus_tests.rs`, `src/literate.rs`)
- [x] Update `doc/09-documents.md` §Weave Mode with the new block format and `--verify`
  flag; update §Corpus Test Format to note `=== info` (`doc/09-documents.md`,
  `doc/12-tooling.md`)

**Error handling in weave** — embedding errors in the doc is the **default** behaviour:
on block evaluation error, write the error to the block's `=== error` section, continue
to the next block, and return exit 0. This ensures `just doc` always produces complete
output even when examples break.

`--fail-on-errors` opts into CI-style behaviour: any evaluation error is fatal (exit 1).
Orthogonal to `--strict` (typecheck phase) and `--verify` (comparison phase). `just doc-verify`
uses both `--strict` and `--fail-on-errors`.

- [x] Make embedding errors the default in `run_literate_weave`; on block evaluation error,
  write error text to `=== error` section and continue; return exit 0 (`src/main.rs`)
- [x] Add `--fail-on-errors` flag; when set, any evaluation error exits 1 immediately
  instead of embedding (`src/main.rs`)
- [x] Update `just doc-verify` in justfile to use `--strict --fail-on-errors --verify`
  (`justfile`)

**Fixed ClockCap from file mtime** — `%clock` is always available in literate programs,
set to a fixed ClockCap derived from the source markdown file's mtime. This makes weave
output deterministic: the same file always produces the same output regardless of when
`just doc` runs, which is essential for `git diff --exit-code` to be stable.

- [x] In `tinct literate`, read the source file's mtime at startup; inject it as a fixed
  ClockCap (`--cap-clock-fixed`) rather than the live system clock (`src/main.rs`)

**Capability flags and `--no-pwd`** — `--cap-fs` and `--cap-net` work identically to
`tinct run`: not specified = no DirCap/NetCap injected. `%libdir` is always available.
`tinct literate` additionally always runs with `--no-pwd`: no implicit DirCap for the
current working directory is granted. Code blocks cannot access CWD-relative files;
all filesystem access must be through explicit `--cap-fs` grants. This prevents a
markdown file being processed from accidentally reading or writing local files.

- [x] Hard-code `--no-pwd` and `--no-env` into `tinct literate`: never inject an implicit
  CWD-based DirCap, and `env-var` always returns `Err` (no environment variable access)
  regardless of flags (`src/main.rs`)
- [x] Expose `--cap-fs name=path:mode` and `--cap-net name=host:port` on `tinct literate`
  subcommand; wire to the same cap-grant machinery as `tinct run` (`src/main.rs`)
- [x] Document in `doc/12-tooling.md` §Literate Mode: all flags, fixed-clock semantics,
  `--no-pwd` always-on, and `--errors-in-doc` use case (`doc/12-tooling.md`)

---
