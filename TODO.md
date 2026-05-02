# Implementation Roadmap

See DONE.md for the full history of completed sprints.

For future work beyond the active sprints below, see:
- `doc/whatif/index.md §Adopt Now` — features ready to implement (Type Predicates, String Interpolation Phase 1, Let Binding, Structural Contracts Phase 1, ADTs Phase 1, Source Snippets, Circular Dep Error Paths, Eval Semantics Verification Phase 1)
- `doc/whatif/index.md §Wait for Trigger` — features with complete designs pending a concrete trigger

## new-syntax: Unified Syntax Reform

Bare-word references, implied call, `$` as disambiguator, and `%`-named pipeline sections. See `doc/whatif/new-syntax.md` (Accepted 2026-05-01) and the updated chapters `doc/02-syntax.md` and `doc/09-documents.md`.

- [x] Design new-syntax — see doc/whatif/new-syntax.md §Design

### new-syntax-a: Phase 1 — `%` Pipeline + Named Sections

Non-breaking addition. See `doc/09-documents.md §DOC-PIPELINE` (updated formal semantics) and `doc/whatif/new-syntax.md §Phase 1`.

**Depends on:** `new-syntax-docs`

- [ ] **AST** (`src/ast.rs`): Add `name: Option<String>`, `output_type: Option<Spanned<Annotation>>`, `expects: Option<Spanned<Annotation>>` to `Document`. Update all `Document { expressions }` construction sites to add `name: None, output_type: None, expects: None`.
- [ ] **Lexer** (`src/lexer.rs`): No lexer changes required for Phase 1. `%` is already a valid `bare_word_char` (not in the exclusion list), so `%foo` already lexes as `Token::BareWord("%foo")` and the formatter already renders it as `%foo` (no sigil added). Add lexer unit tests to confirm `%defaults`, `%`, `%+` all lex as `BareWord` tokens with the `%` included in the string.
- [ ] **Parser** (`src/parser.rs`): Two changes. (1) **Atom parsing**: in the `BareWord` match arm for atom parsing, add a rule: if the bare word string starts with `%`, produce `Expr::VarRef(s.to_string())` instead of `Expr::Str(s)`. This makes `%defaults` and bare `%` in value position resolve as variable references, not strings. (2) **Section header helper**: implement `parse_section_header(tokens, i)` consuming tokens until `Newline`. Matches: optional `%name` — `Token::BareWord(s) if s.starts_with('%')`, section name = `s[1..]` (chars after `%`); optional `@Type` annotation — match `ImmediateAt` (emitted correctly since `BareWord` sets `LastSignificantToken::BareWord`, so `is_immediate_at_context()` returns true and `ImmediateAt` is emitted for `%name@Type` with no whitespace); optionally also accept `At` for robustness; optional `expects:` pragma — match `BareWord("expects")`, `Colon`, type annotation. `BareWord("%")` with nothing after `%` (empty name after strip) → parse error. Duplicate `%name` in same file → parse error. Populate `Document.name`, `output_type`, `expects`.
- [ ] **Evaluator** (`src/eval.rs`): In `eval_file_with_input()`, add `named: IndexMap<String, Rc<Thunk>>` accumulator (`Σ`). After each document: bind `"%"` = `prev_output` in `doc_env` (alongside existing `"$"` binding for `$$` backward compat). For all prior named sections, bind `format!("%{}", section_name)` in `doc_env` — e.g., a section named `"defaults"` (stored as `doc.name = Some("defaults")`) is bound as key `"%defaults"` so that `VarRef("%defaults")` resolves via LOOKUP. If `doc.name = Some(n)`, after evaluating the document insert `(n, result_thunk)` into `named`. Named section thunks stored raw (no materialization at `---` boundary). A section cannot reference its own name (not yet in `Σ`) — produces `UndefinedVariable`. Forward references to later sections also produce `UndefinedVariable`.
- [ ] **Type checker** (`src/typecheck.rs`): Rename pipeline binding from `"$"` to `"%"` in `typecheck_document`. Thread named-section type bindings through the sequential document loop. Validate `@Type` output annotation against post-body env (resolve after body inference). Validate `expects:` against incoming `%` type (resolve against pre-body env). Both emit `TypeError` (advisory).
- [ ] **Corpus tests**: Named sections `--- %defaults` / `--- %overrides` / `[call $merge %defaults %overrides]`. Anonymous `%` as alias for `$$`. `@Type` output annotation. `expects:` contract violation → type error. Bare `%` section name → parse error.

### new-syntax-b: Phase 2 — Core Syntax Migration (Breaking)

Single atomic change. All internal `.llt` files migrated in the same commit. See `doc/02-syntax.md §2.3` (updated), `doc/whatif/new-syntax.md §Phase 2`.

**Depends on:** `new-syntax-a`

- [ ] **AST** (`src/ast.rs`): Add `implied: bool` to the `Call` AST node (or `Expr::Call` variant). `Expr::Str` remains for quoted strings only; `Expr::VarRef` now covers all value-position bare words.
- [ ] **Lexer** (`src/lexer.rs`): Rename `Token::BareWord` → `Token::Identifier`, `Token::VarRef` → `Token::EscapedRef`. Update all match arms atomically. Add `Identifier` to `is_access_context()` and `is_bracket_access_context()` so `name.field` and `name[0]` produce access chains (consistent with bare-word-as-reference semantics). Update `LastSignificantToken` accordingly.
- [ ] **Parser** (`src/parser.rs`): Implement head-position priority table in frame classification. **Critical**: the colon check for keyed-head detection (Priority 4 — dict vs implied call) must use `peek_next_horizontal` (not `peek_next_significant` which skips newlines) — consistent with existing keyword disambiguation. `[name\n: val]` must not classify as a dict entry; it should be a zero-arg call. Full priority table: keywords (call/fn/type, horizontal colon check) → special form; `Identifier` followed by horizontal `:` → Dict (use `peek_next_horizontal`); `Identifier` (not keyword, no horizontal colon) → `Call` with `implied: true`; `EscapedRef` in head → Dict (data sequence); literal in head → Dict. Add `func: Option<Spanned<Expr>>` to `StackFrame::Call`; set it to the head `Identifier`'s `VarRef` expr at frame-push time for implied calls (leaves `None` for explicit `[call ...]`). When closing a Call frame, use `func` if set, else extract function from `args[0]` as today. Atoms: `Identifier(s)` → `Expr::VarRef(s)` (was `Expr::Str`); `EscapedRef(s)` → `Expr::VarRef(s)` (renamed token, same resolution). Update keyword detection to `Token::Identifier` (atomic, covers all three keyword arms simultaneously).
- [ ] **Type checker** (`src/typecheck.rs`): Remove `BareWord/Identifier → String` inference arm. `Expr::Str` (quoted) still infers `Type::Str`. No other type inference changes.
- [ ] **Evaluator** (`src/eval.rs`): Stop binding `"$"` (for `$$`) in pipeline env — bind only `"%"` and `"%name"`. `VarRef` resolution unchanged; just applied to more nodes.
- [ ] **Formatter** (`src/formatter.rs`): `Identifier(s)` → render as `s` (no sigil). `EscapedRef(s)` → render as `$s`. `%foo` and `%` are `Identifier("%foo")` / `Identifier("%")` after Phase 2 rename — `Identifier(s)` → `s` renders them correctly as `%foo` / `%` with no extra sigil.
- [ ] **Error messages** (`src/error.rs`): Update error text — references shown as `name` not `$name`; calls shown as `[f ...]` not `[call $f ...]`. Add "Did you mean to quote this as a string? Use `\"name\"`" suggestion for `UndefinedVariable` errors where the name looks like an intended string literal (heuristic: all lowercase/alphanumeric, not a known builtin, not `%`-prefixed).
- [ ] **File migration**: Mechanically transform all `.llt` files: `stdlib/prelude.llt`, `tests/corpus/**/*.llt`. Rules: `$var` → `var`; `[call $f x y]` → `[f x y]`; unquoted bare string values → quoted (`host: localhost` → `host: "localhost"`); `$$` → `%`; `$$foo` → `%foo`. Verify: `cargo test` passes in full after migration.

### new-syntax-c: Phase 2b — Polish and Completeness

See `doc/whatif/new-syntax.md §Phased Adoption` and `doc/02-syntax.md §6 Complete Grammar`.

**Depends on:** `new-syntax-b`

- [ ] **tree-sitter-llt** (`tree-sitter-llt/grammar.js`): Update grammar for `identifier` rule (bare word → reference), `escaped_ref` (`$word`), implied call in bracket forms, `%`/`%name` pipeline identifiers, `--- %name@Type expects: Type` section headers.
- [ ] **Corpus tests — implied call**: Nested `[f [g x] y]`, zero-arg `[clock]`, single-arg `[negate n]`, `[f]` is call not data, `[$f]` is data not call.
- [ ] **Corpus tests — EscapedRef data sequences**: `stages: [$parse transform format]` (only head needs `$`). Data sequences with `%` references.
- [ ] **Corpus tests — named pipeline sections**: Multi-input `[merge %defaults %overrides]`, type-annotated outputs (`%config@Config`), `expects:` contract violations at section boundaries, forward reference to unnamed section → `UndefinedVariable`.
- [ ] **Error message tests**: `UndefinedVariable` for unquoted string → "Did you mean to quote?" suggestion fires. No `$name` references remain in error output.
- [ ] **Doc updates**: Verify `doc/02-syntax.md §6 Complete Grammar` ebnf rules match implementation. Update `doc/09-documents.md` DOC-PIPELINE implementation correspondence table with exact `eval.rs` line numbers for named-section Σ accumulation. Update `$include` cross-references that still use old `$$` notation in formal spec section.
- [ ] **`$$` removal**: Remove `"$"` binding from pipeline env if still present after `new-syntax-b`. Confirm no corpus test or stdlib references remain.
