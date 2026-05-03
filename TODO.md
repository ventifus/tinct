# Implementation Roadmap

See DONE.md for the full history of completed sprints.

For future work beyond the active sprints below, see:
- `doc/whatif/index.md §Adopt Now` — features ready to implement (Type Predicates, String Interpolation Phase 1, Let Binding, Structural Contracts Phase 1, ADTs Phase 1, Source Snippets, Circular Dep Error Paths, Eval Semantics Verification Phase 1)
- `doc/whatif/index.md §Wait for Trigger` — features with complete designs pending a concrete trigger

## new-syntax: Unified Syntax Reform

Bare-word references, implied call, `$` as disambiguator, and `%`-named pipeline sections. See `doc/whatif/new-syntax.md` (Accepted 2026-05-01) and the updated chapters `doc/02-syntax.md` and `doc/09-documents.md`.

- [x] Design new-syntax — see doc/whatif/new-syntax.md §Design

### new-syntax-c: Phase 2b — Polish and Completeness

See `doc/whatif/new-syntax.md §Phased Adoption` and `doc/02-syntax.md §6 Complete Grammar`.

**Depends on:** `new-syntax-b`

- [ ] **tree-sitter-llt** (`tree-sitter-llt/grammar.js`): Update grammar for `identifier` rule (bare word → reference), `escaped_ref` (`$word`), implied call in bracket forms, `%`/`%name` pipeline identifiers, `--- %name@Type expects: Type` section headers.
- [ ] **Corpus tests — implied call**: Nested `[f [g x] y]`, zero-arg `[clock]`, single-arg `[negate n]`, `[f]` is call not data, `[$f]` is data not call.
- [ ] **Corpus tests — EscapedRef data sequences**: `stages: [$parse transform format]` (only head needs `$`). Data sequences with `%` references.
- [ ] **Corpus tests — named pipeline sections**: Multi-input `[merge %defaults %overrides]`, type-annotated outputs (`%config@Config`), `expects:` contract violations at section boundaries, forward reference to unnamed section → `UndefinedVariable`.
- [ ] **Error message tests**: `UndefinedVariable` for unquoted string → "Did you mean to quote?" suggestion fires. No `$name` references remain in error output.
- [ ] **Doc updates**: Verify `doc/02-syntax.md §6 Complete Grammar` ebnf rules match implementation. Add `call_implied` production to §6 `bracket_expr` rule (currently omits `[f x y]` implied-call form). Add Priority 2b (Identifier+ImmediateAt → Dict, fixes `[Foo@String]`) to §3.2 priority table. Fix `src/typecheck.rs:2495-2497` comment about `Fn@RetType` classification (Priority 2b routes it to Dict, not implied Call). Update `doc/09-documents.md` DOC-PIPELINE implementation correspondence table with exact `eval.rs` line numbers for named-section Σ accumulation. Update `$include` cross-references that still use old `$$` notation in formal spec section. Update `doc/02-syntax.md §2.5` to document that `%name` uses `lex_percent_word()` and gets dot-access treatment (`last_significant_token = VarRef`), unlike plain bare words where `a.b` tokenizes as a single `BareWord("a.b")`. Update `doc/09-documents.md` formal DOC-PIPELINE rule to note transitional `$` binding alongside `%`. Fix `output_type` annotation resolution to use `result_env` (post-body type aliases visible) instead of `env` in `src/typecheck.rs:529`.
- [ ] **`$$` removal**: Remove `"$"` binding from pipeline env if still present after `new-syntax-b`. Confirm no corpus test or stdlib references remain. Also update `doc/whatif/structural-contracts.md` and `doc/whatif/index.md` which still use `$$` in proposal examples (lines ~25, 26, 39, 87, 120).

## Cycle Findings — C116

### cycle-findings-c116: C116 Codebase Health

Findings from 9-agent review after cycle #116 analysis phase.

- [ ] **Major — eval_stack push/pop asymmetry on DepthExceeded** (`src/eval_materialize.rs:391-413, 567-593`): For Unevaluated and PendingBuiltin thunks, `eval_stack.push()` is called BEFORE the depth check. On the DepthExceeded path, the thunk state is restored (no InProgress sentinel left) but the `eval_stack` entry is never popped — the Memoize continuation that does the pop is never pushed. Fix: move `eval_stack.push()` to after the `depth > MAX_EVAL_DEPTH` check (matching the PendingCall pattern which checks depth before taking). Leaked entries cause misleading cycle-path chains in subsequent `CircularDependency` errors. [computer-scientist]
- [ ] **Major — 9 error codes have zero corpus coverage** (`tests/corpus/eval/errors/`): E050 (IncludeNotAvailable), E051 (IncludeIoError), E052 (IncludeCycle), E053 (IncludeParseFailed), E054 (IncludeFileTooLarge), E055 (IncludeHashMismatch), E056 (IncludeHashRequired), E057 (IncludePathNotAllowed), E062 (JsonRange). All documented in doc/10-errors.md §9.2 but have no corpus regression tests. [test-crafter]
- [ ] **Major — Row.fields uses IndexMap instead of HashMap** (`src/types.rs:38`): Row field order is semantically irrelevant at the type level (Rémy commutativity). Change `Row.fields: IndexMap<String, Type>` → `HashMap<String, Type>`. IndexMap is ~20% slower for lookup in the `unify_rows` hot path. (This is distinct from runtime `Value::Dict` which correctly uses IndexMap for ordered semantics.) [performance-expert]
- [ ] **Major — No corpus tests for resource limits** (`tests/corpus/`): (a) No test for MAX_PARSE_DEPTH=256 — add `tests/corpus/invalid/syntax_errors/parse_depth_exceeded.llt-eval` with 257 nested brackets. (b) No test for MAX_COLLECT_SIZE=1M — add `tests/corpus/eval/errors/collect_size_exceeded.llt-eval` creating >1M element sequence and collecting it. [test-crafter]
- [ ] **Minor — doc/08-evaluation.md Cont variant names diverge from implementation** (`doc/08-evaluation.md:1107-1141`): Spec lists `Cont::PendingCallForceFunc` and `Cont::PendingBuiltinForceResult` but implementation uses `Cont::PendingCallDispatch` and `Cont::BuiltinForceArg`. Update spec table to reflect implemented variant names. [computer-scientist]
- [ ] **Minor — Access chain span propagation gap** (`src/eval.rs:5746`): Multi-step access chain errors (e.g., `a.b.c.d`) don't propagate `mat_span` through each step. The outer access expression span is lost when the chain descends. Per doc/10-errors.md Part 3 (DECORATE rule), each access step should attach its span as materialization context. Track as `access-chain-spans` fix-later. [integration-verifier]
- [ ] **Minor — Span::origin() frame filtering missing from error display** (`src/error.rs`): The stdlib filter removes frames with `-impl`/`-step`/`-check` suffixes, but not frames with `Span::origin()` (0:0-0:0 synthetic spans from stdlib thunks). These synthetic frames can pollute user-facing stack traces. Add explicit filter for `Span::origin()` alongside the label suffix filter in `EvalError::Display`. [integration-verifier]
- [ ] **Minor — Missing Seq guards in 4 stdlib functions** (`stdlib/prelude.llt:465,496,517,613`): `sort-by`, `take-while`, `drop-while`, `flatten` crash on Seq input instead of providing a clear error. Add `$seq?` guard with message "func: expected Dict, got Seq — collect the Seq first" following the pattern already used in `any?`/`all?`/`partition`. [stdlib-author]
- [ ] **Minor — Stale builtin counts in docs** (`doc/11-stdlib.md:255`, `doc/11a-builtins.md:246,255`): doc/11-stdlib.md says "51 Rust-native builtins" (should be 46). doc/11a-builtins.md says "51 + 12 = 63 registered names" (should be 46 + 12 = 58). doc/11a-builtins.md "Evaluation: 5" (should be 4, `until` is general not evaluation-control). [stdlib-author]
- [ ] **Minor — Corpus README severely outdated** (`tests/corpus/README.md`): Shows only 4 directories but 26 are enforced by `test_corpus_structure()`. Rewrite documenting all 26 directories, format conventions (`===` delimiter, `ERROR:` prefix, `# no_fs` directive), and test harness behavior. [test-crafter]
- [ ] **Minor — Missing sequence constructor corpus tests** (`tests/corpus/eval/builtins/`): doc/08-evaluation.md §Testing Requirements mandates tests for: `repeat_depth_limit.llt-eval`, `iterate_malformed_tail.llt-eval`, `unfold_depth_limit.llt-eval`. Add these 3 tests. [test-crafter]
- [ ] **Minor — doc/12-tooling.md §Sandboxing "ASPIRATIONAL" label stale** (`doc/12-tooling.md:60`): All sandbox features are now implemented (--no-fs, --timeout, --allow-path, --require-integrity, Landlock, seccomp-bpf, rlimit). Remove "ASPIRATIONAL — NOT YET IMPLEMENTED" label and update to enumerate implemented features. [security-expert]
