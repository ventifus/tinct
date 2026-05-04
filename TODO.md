# Implementation Roadmap

See DONE.md for the full history of completed sprints.

For future work beyond the active sprints below, see:
- `doc/whatif/index.md §Adopt Now` — features ready to implement (Type Predicates, String Interpolation Phase 1, Let Binding, Structural Contracts Phase 1, ADTs Phase 1, Source Snippets, Circular Dep Error Paths, Eval Semantics Verification Phase 1)
- `doc/whatif/index.md §Wait for Trigger` — features with complete designs pending a concrete trigger

## Cycle Findings — C116

### cycle-findings-c116: C116 Codebase Health

Findings from 9-agent review after cycle #116 analysis phase.

- [ ] **Major — eval_stack push/pop asymmetry on DepthExceeded** (`src/eval_materialize.rs:391-413, 567-593`): For Unevaluated and PendingBuiltin thunks, `eval_stack.push()` is called BEFORE the depth check. On the DepthExceeded path, the thunk state is restored (no InProgress sentinel left) but the `eval_stack` entry is never popped — the Memoize continuation that does the pop is never pushed. Fix: move `eval_stack.push()` to after the `depth > MAX_EVAL_DEPTH` check (matching the PendingCall pattern which checks depth before taking). Leaked entries cause misleading cycle-path chains in subsequent `CircularDependency` errors. [computer-scientist]
- [ ] **Major — 9 error codes have zero corpus coverage** (`tests/corpus/eval/errors/`): E050 (IncludeNotAvailable), E051 (IncludeIoError), E052 (IncludeCycle), E053 (IncludeParseFailed), E054 (IncludeFileTooLarge), E055 (IncludeHashMismatch), E056 (IncludeHashRequired), E057 (IncludePathNotAllowed), E062 (JsonRange). All documented in doc/10-errors.md §9.2 but have no corpus regression tests. [test-crafter]
- [x] **Major — Row.fields uses IndexMap instead of HashMap** (`src/types.rs:38`): `Row.fields` already uses `HashMap<String, Type>` — finding resolved. [performance-expert]
- [ ] **Major — No corpus tests for resource limits** (`tests/corpus/`): (a) No test for MAX_PARSE_DEPTH=256 — add `tests/corpus/invalid/syntax_errors/parse_depth_exceeded.llt-eval` with 257 nested brackets. (b) No test for MAX_COLLECT_SIZE=1M — add `tests/corpus/eval/errors/collect_size_exceeded.llt-eval` creating >1M element sequence and collecting it. [test-crafter]
- [ ] **Minor — doc/08-evaluation.md Cont variant names diverge from implementation** (`doc/08-evaluation.md:1107-1141`): Spec lists `Cont::PendingCallForceFunc` and `Cont::PendingBuiltinForceResult` but implementation uses `Cont::PendingCallDispatch` and `Cont::BuiltinForceArg`. Update spec table to reflect implemented variant names. [computer-scientist]
- [ ] **Minor — Access chain span propagation gap** (`src/eval.rs:5746`): Multi-step access chain errors (e.g., `a.b.c.d`) don't propagate `mat_span` through each step. The outer access expression span is lost when the chain descends. Per doc/10-errors.md Part 3 (DECORATE rule), each access step should attach its span as materialization context. Track as `access-chain-spans` fix-later. [integration-verifier]
- [ ] **Minor — Span::origin() frame filtering missing from error display** (`src/error.rs`): The stdlib filter removes frames with `-impl`/`-step`/`-check` suffixes, but not frames with `Span::origin()` (0:0-0:0 synthetic spans from stdlib thunks). These synthetic frames can pollute user-facing stack traces. Add explicit filter for `Span::origin()` alongside the label suffix filter in `EvalError::Display`. [integration-verifier]
- [x] **Minor — Missing Seq guards in 4 stdlib functions** (`stdlib/prelude.llt:465,496,517,613`): `sort-by`, `take-while`, `drop-while`, `flatten` all have `$seq?` guards added with "expected Dict, got Seq — collect the Seq first" message. [stdlib-author]
- [ ] **Minor — Stale builtin counts in docs** (`doc/11-stdlib.md:255`, `doc/11a-builtins.md:246,255`): doc/11-stdlib.md says "51 Rust-native builtins" (should be 46). doc/11a-builtins.md says "51 + 12 = 63 registered names" (should be 46 + 12 = 58). doc/11a-builtins.md "Evaluation: 5" (should be 4, `until` is general not evaluation-control). [stdlib-author]
- [ ] **Minor — Corpus README incomplete** (`tests/corpus/README.md`): Shows ~22 directories but is missing 5 enforced by `test_corpus_structure()`: `eval/cross_feature`, `eval/regressions`, `eval/pipeline`, `invalid/pipeline`, `valid/parser_mechanisms`. Also lists `eval/letrec` and `eval/documents` which are not in the enforced set. Add all 25 required dirs and document format conventions (`===` delimiter, `ERROR:` prefix, `# no_fs` directive). [test-crafter]
- [ ] **Minor — Missing sequence constructor corpus tests** (`tests/corpus/eval/builtins/`): doc/08-evaluation.md §Testing Requirements mandates tests for: `repeat_depth_limit.llt-eval`, `iterate_malformed_tail.llt-eval`, `unfold_depth_limit.llt-eval`. Add these 3 tests. [test-crafter]
- [ ] **Minor — doc/12-tooling.md §Sandboxing "ASPIRATIONAL" label stale** (`doc/12-tooling.md:60`): All sandbox features are now implemented (--no-fs, --timeout, --allow-path, --require-integrity, Landlock, seccomp-bpf, rlimit). Remove "ASPIRATIONAL — NOT YET IMPLEMENTED" label and update to enumerate implemented features. [security-expert]
