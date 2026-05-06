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

- [x] Add interpolated string corpus tests: `tests/corpus/valid/literals/interpolated_strings.llt-eval` (basic i"Hello $name", i"$$escaped", variable boundaries) and `tests/corpus/eval/builtins/interpolated_string_eval.llt-eval` (desugaring to `str` calls) [Critical — test-crafter]
- [x] Add row polymorphism corpus tests: anonymous rest (`...`), named rest (`...r`), rest in function signatures, rest with field constraints in `tests/corpus/eval/type_system/row_*.llt-eval` [Critical — test-crafter]
- [x] Add deeply chained access corpus tests: 5+ level chains mixing dot access (identifier and integer keys) and `get` builtin calls, and mid-chain error cases (`tests/corpus/eval/access/deeply_chained_mixed.llt-eval`) [Critical — test-crafter]
- [x] Add pipeline section metadata corpus tests for `--- %name@Type` (output type annotation) and `--- expects: Type` (input contract) in `tests/corpus/eval/pipeline/` [Critical — test-crafter]
- [x] Add annotation bracket restriction invalid tests: `x@[call f]`, `x@[fn [a] a]`, `x@[@Type e]` must all be parse errors (`tests/corpus/invalid/syntax_errors/annotation_special_form.llt-eval`) [Critical — test-crafter]
- [x] Add static constraint corpus test: verify rest entry position `[a ... b]` is valid syntax (`tests/corpus/valid/edge_cases/rest_entry_positions.llt-eval`) [Major — test-crafter]
- [x] Add Unicode identifier corpus tests (`tests/corpus/valid/literals/unicode_identifiers.llt-eval`) and escape sequence rejection tests for `\x41`, `A` (`tests/corpus/invalid/syntax_errors/unsupported_escape.llt-eval`) [Major — test-crafter]
- [x] Add MAX_PARSE_DEPTH boundary tests: 256-nested-bracket file (valid) and 257 (error) in `tests/corpus/` [Major — test-crafter]
- [x] Add CRLF formatter roundtrip unit test in `src/formatter.rs`: parse CRLF input, format, assert line endings preserved [Major — test-crafter]
- [x] Add builtin limit enforcement tests for MAX_COLLECT_SIZE and MAX_STRING_SIZE in `tests/corpus/eval/errors/` [Minor — test-crafter]
- [x] Add unit test module in `src/builtins.rs` for `builtin_each`, `builtin_each_key`, `builtin_each_kv`, `builtin_get` covering empty dict, multi-entry, offset-based recursion, and type errors [Critical — test-crafter]
- [x] Add error corpus tests for `each`, `each-key`, `each-kv` called on non-Dict values in `tests/corpus/eval/errors/` [Critical — test-crafter]
- [x] Add pipe operator precedence and associativity corpus tests: `x | f.g`, `[f x | g]`, `[a: x | f]`, `a | b | c` left-associativity in `tests/corpus/eval/access/` [Major — test-crafter]
- [x] Add JSON formatter error corpus tests: Seq serialization error, Function/Builtin error in `tests/corpus/eval/errors/` [Major — test-crafter]
- [x] Add pipe + each integration corpus tests: `dict | each | collect`, `d | each-key | map str`, three-stage `each-kv | filter | collect-kv` in `tests/corpus/eval/cross_feature/` [Major — test-crafter]

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

## Metaprogramming: AST-as-Data, Quasiquoting, Macros, Formatter

AST dict schema, quasiquoting, procedural macros, and tinct-hosted formatter. See `doc/whatif/plans/macros-cluster.md` for the full cluster plan, dependency graph, and decision gates.

- [x] Accept macros cluster — see doc/whatif/plans/macros-cluster.md (State: Accepted — 2026-05-05); covers ast-schema.md, quasiquoting.md, macros.md, tinct-hosted-formatter.md

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
