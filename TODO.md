# Implementation Roadmap

See DONE.md for the full history of completed sprints.

For future work beyond the active sprints below, see:
- `doc/whatif/index.md §Adopt Now` — features ready to implement (Type Predicates, String Interpolation Phase 1, Let Binding, Structural Contracts Phase 1, ADTs Phase 1, Source Snippets, Circular Dep Error Paths, Eval Semantics Verification Phase 1)
- `doc/whatif/index.md §Wait for Trigger` — features with complete designs pending a concrete trigger

## new-syntax: Unified Syntax Reform

Bare-word references, implied call, `$` as disambiguator, and `%`-named pipeline sections. See `doc/whatif/new-syntax.md` (Accepted 2026-05-01) and the updated chapters `doc/02-syntax.md` and `doc/09-documents.md`.

- [x] Design new-syntax — see doc/whatif/new-syntax.md §Design

### new-syntax-b: Phase 2 — Core Syntax Migration (Breaking)

Single atomic change. All internal `.llt` files migrated in the same commit. See `doc/02-syntax.md §2.3` (updated), `doc/whatif/new-syntax.md §Phase 2`.

**Depends on:** `new-syntax-a`

- [ ] **Bug** (`src/parser.rs`): `[fn [x ...rest default: 10] body]` is rejected with "parameter after variadic parameter". The grammar currently disallows any parameter (including named params with defaults) after a `...rest` variadic. If named-after-variadic support is desired, add a parser rule allowing named params after the variadic.
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
- [ ] **Doc updates**: Verify `doc/02-syntax.md §6 Complete Grammar` ebnf rules match implementation. Update `doc/09-documents.md` DOC-PIPELINE implementation correspondence table with exact `eval.rs` line numbers for named-section Σ accumulation. Update `$include` cross-references that still use old `$$` notation in formal spec section. Update `doc/02-syntax.md §2.5` to document that `%name` uses `lex_percent_word()` and gets dot-access treatment (`last_significant_token = VarRef`), unlike plain bare words where `a.b` tokenizes as a single `BareWord("a.b")`. Update `doc/09-documents.md` formal DOC-PIPELINE rule to note transitional `$` binding alongside `%`. Fix `output_type` annotation resolution to use `result_env` (post-body type aliases visible) instead of `env` in `src/typecheck.rs:529`.
- [ ] **`$$` removal**: Remove `"$"` binding from pipeline env if still present after `new-syntax-b`. Confirm no corpus test or stdlib references remain.
