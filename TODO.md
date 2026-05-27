# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Known Bugs + Nits

### nit-fixes: Prelude bugs, docgen T003, macro-ast compat (7 tasks)

Combines: docgen-return-type-t003 (1), prelude-named-section-bug (2), prelude-pretty-array-multiarg-if (2), macro-ast-expression-compat (2).

**docgen T003:** `return-type-text` in scripts/docgen.llt produces advisory T003 from unconstrained `try-str` return type.
- [x] Add `@String` type annotation to `try-str`'s return so the checker infers concrete String rather than `String | _` (`scripts/docgen.llt`)

**prelude named-section bug:** `eval-document-pipeline` uses `[[str "%" n]: result]` (invalid computed-key syntax). Fix: use `builder-set` for named section dict construction.
- [x] Fix `eval-document-pipeline` in stdlib/prelude.llt to use builder-set for named section dict construction
- [x] Add corpus test that uses `---\n%name-of-doc` and verifies `%name-of-doc` binding works in subsequent docs

**prelude pretty-array multi-arg if:** `to-json-pretty-array-from-dict` calls `[if ...]` with 6 args (only 3 allowed). Fix: extract body into helper function.
- [x] Fix `to-json-pretty-array-from-dict` in stdlib/prelude.llt
- [x] Add corpus test for pretty-printing array-like dicts (integer keys 0..n)

**macro-ast-expression-compat:** Pre-existing test failures from runtime-v2 (fn macro alias, pattern match duplicate handling).
- [x] `test_syntax_llt_fn_single_param` / `test_syntax_llt_fn_macro_triggered`: fn macro alias produces Fn but body VarRefs aren't resolved. Investigate resolution pass interaction with macro-expanded expressions.
- [x] `test_pm3_match_expr_duplicate_dict_field_errors`: verify last-binding-wins semantics work correctly after linearity check removal

### letrec-self-ref-silent: `[x: x]` in letrec dicts silently cycles via try-or, producing wrong results

In a letrec dict `[name: name  doc: doc  sig: sig]`, the value expressions `name` and `doc` resolve to the dict's OWN "name" and "doc" keys (not the outer-scope variables), creating circular dependencies. When forced, cycle detection or depth limiting produces an error. If a surrounding `try-or` (e.g., in `has?` via `get-or`) catches this error, it silently returns the fallback instead of the actual value.

This was the root cause of `scripts/docgen.llt` producing empty documentation: `has? record "name"` returned false for all records because forcing the "name" thunk cycled.

**Workaround (applied in docgen.llt):** Use an intermediate fn call to break the letrec: `[call [fn@Dict [let n d s] [name: n  doc: d  sig: s]] name kdoc sig-val]`. The fn params `n`, `d`, `s` don't conflict with the dict keys.

**Proper fix:** Add a diagnostic at resolve or typecheck time warning when a letrec dict entry `[k: expr]` has `expr` being a bare VarRef with the same name as key `k`. This is almost always a bug.

- [ ] Add T002/T003 diagnostic: warn when a dict entry's value is `VarRef(name)` and the entry's key is also `name` — likely letrec self-reference
- [ ] Alternatively: evaluate dict value expressions in PARENT scope when the value is a bare VarRef matching its own key
- **Files:** `src/resolve.rs` or `src/typecheck.rs`

### tco-restorestate: Change RestoreState to CoreExpr-based restore (prerequisite for TCO)

`RestoreState::PendingCall` and `RestoreState::PendingBuiltin` currently hold `Vec<Arc<Thunk>>` arg references. This prevents tail-call optimization: when `builtin-if` returns a branch thunk, the Memoize for the if-call holds that thunk in its RestoreState → `Arc::strong_count == 2` → TCO eligibility check fails. Fix: store `(original_call: Arc<Spanned<CoreExpr>>, env)` instead, re-evaluating from CoreExpr on DepthExceeded retry. Already-materialized sub-thunks hit the `try_get_materialized()` fast path — no redundant work.

The `CoreExpr::Call` node is available in `eval_core_expr_pub` when it matches on the Call arm and calls `eval_call_core` — but it currently drops `expr` and only passes the pieces. Thread it through:

- [x] Add `original_call: Arc<Spanned<CoreExpr>>` to `UnevaluatedState::Call` and `Thunk::new_pending_call` (`src/value.rs`)
- [x] Pass `Arc::clone(expr)` from `eval_core_expr_pub`'s Call arm to `eval_call_core`; update `eval_call_core` signature and `new_pending_call` call (`src/eval.rs:1514`, `src/eval_call.rs:51`)
- [x] `take_pending_call()` returns `original_call` alongside existing fields; add `original_call` to `PendingCallDispatchData` (`src/eval_materialize.rs`)
- [x] In `apply_cont(PendingCallDispatch)`: build `RestoreState::CoreExpr { expr: original_call, env: caller_env }` instead of `RestoreState::PendingCall { func, args, ... }` / `RestoreState::PendingBuiltin { def, args, ... }` for both Function and Builtin dispatch paths (`src/eval_materialize.rs`)
- [x] Update `RestoreState::restore()` for the new `CoreExpr` variant: re-evaluate `expr` in `env` via `eval_call_core` to reconstruct a fresh PendingCall thunk, then restore that state into the thunk (`src/eval_materialize.rs`)
- [x] Corpus test: DepthExceeded from inside a recursive `[if ...]` branch retries correctly and produces the right result
- **Files:** `src/value.rs`, `src/eval.rs`, `src/eval_call.rs`, `src/eval_materialize.rs`

### tco-proper: True O(1) tail-call optimization in the CEK machine

**Depends on:** `tco-restorestate` (eliminates arg-thunk references from RestoreState so branch thunks reach `strong_count == 1`)

No `CoreExpr::Call` changes. No lowering pass changes. Use `Arc::strong_count(thunk) == 1` as the sole runtime TCO eligibility check — if nobody else holds the thunk, memoization is unnecessary and skipping it is safe.

**How it achieves O(1):** `apply_cont(PendingCallDispatch { tail_hint: true }, Function)` returns `Action::EvalCore { body_expr, new_env }` — no intermediate body thunk is created, so no `Memoize(T_body)` is pushed. `EvalCore` evaluates the body inline; if it produces a fresh call thunk (e.g. `[if ...]` with count==1), that gets TCO'd too. The chain propagates: `T_if_call` (count==1) → TCO → `builtin-if` returns `T_recursive` (count==1, after `tco-restorestate`) → TCO → `EvalCore` again. Stack stays flat throughout the recursion.

**Race condition:** `Arc::strong_count()` and `take_pending_call()` are both synchronous (no `.await` between them). In tokio's `LocalSet` (cooperative, single-threaded), no other task can run between two synchronous operations — the count is stable. Document as a comment; no code fix needed.

- [x] Add `tail_hint: bool` to `PendingCallDispatchData` (`src/eval_materialize.rs`)
- [x] In `force_step`, Call branch: when `Arc::strong_count(thunk) == 1`, push `PendingCallDispatch` with `tail_hint: true`, skip `eval_stack` push (guard drops without disarm), return `Materialize { func_thunk }` — add comment explaining cooperative scheduling invariant (`src/eval_materialize.rs`)
- [x] Add `invoke_function_tco(ctx: &CallContext) -> EvalResult<(Arc<Spanned<CoreExpr>>, Arc<RwLock<Environment>>)>` — same as `invoke_function` but skips `Thunk::new_unevaluated_core`, returning `(body, call_env)` directly (`src/eval_call.rs`)
- [x] In `apply_cont(PendingCallDispatch { tail_hint: true })` for `Value::Function`: call `invoke_function_tco` → `(body_expr, new_env)`; return `Action::EvalCore { body_expr, env: new_env }` — NO `Memoize` pushed, inherited guard drops (`src/eval_materialize.rs`)
- [x] In `apply_cont(PendingCallDispatch { tail_hint: true })` for `Value::Builtin`: call builtin normally → `result_thunk`; return `Action::Materialize { thunk: result_thunk }` — NO `Memoize` pushed, inherited guard drops. `result_thunk` will itself be TCO-eligible on next `force_step` if count==1 (`src/eval_materialize.rs`)
- [x] Corpus test: `[fn [let n] [if [= n 0] 0 [f [- n 1]]]]` with 10,000+ iterations completes without `DepthExceeded`
- [x] Corpus test: `loop-select` survives 10,000+ iterations
- [x] Corpus test: mutually recursive `f(n) = g(n-1)` / `g(n) = f(n-1)` with 10,000+ iterations
- **Files:** `src/eval_call.rs`, `src/eval_materialize.rs`

### macro-ast-expression-compat ✅ Partially DONE — 5 of 8 tests fixed. Remaining 2 tasks merged into nit-fixes above.

### bare-include-scope ✅ DONE

Bare `[include ...]` and other Call expressions returning dicts now promote bindings into scope for subsequent expressions, matching dict literal semantics. Named form `[lib: [include ...]]` remains for namespaced access.

**Implemented:**
- [x] Change 1: `src/builtins_meta.rs` — `builtin_eval` loop now uses mutable `current_env`, creates child env for intermediate Dict/Overlay results
- [x] Change 2: `src/eval_pipeline.rs` — `eval_surface_document` adds `else` branch to promote Call-result dicts into scope
- [x] Corpus test: `bare_include_scope.llt-eval` — bare `[include %libdir "strings.llt"]` followed by `[str-at 0 "hello"]`
- [x] Corpus test: `bare_call_scope.llt-eval` — intermediate Call returning dict makes bindings available
- [x] Corpus test: `bare_nondict_skip.llt-eval` — non-dict intermediates silently skip (no error)

**Deferred to follow-up sprint `extract-eval-document-exprs`:**
- Change 3: Extract `eval_document_exprs` shared function to eliminate duplication between `builtin_eval` and `eval_surface_document` (tracked as TODO comments in both files)
- Change 4: Update `doc/09-documents.md` §SEQ-SCOPE to document dynamic binding semantics and tier-2 fallback behavior

### extract-eval-document-exprs: Deduplicate scope-chaining logic

**Rationale:** `builtin_eval` (src/builtins_meta.rs:1758-1808) and `eval_surface_document` (src/eval_pipeline.rs:171-287) both implement identical scope-chaining semantics: materialize intermediate expressions, promote Dict/Overlay bindings into child envs, return last expression lazily. This logic should be extracted into a shared function.

**Tasks:**
- [ ] Add `pub(crate) async fn eval_document_exprs(expr_nodes: &[Arc<SurfaceNode>], env: Arc<RwLock<Environment>>, ctx: &Arc<EvalContext>, res: &Arc<ResolutionTable>, types: &Arc<TypeAnnotationTable>) -> EvalResult<Arc<Thunk>>` in `eval_pipeline.rs`
- [ ] Loop: lower → eval → materialize → if Dict/Overlay flatten and create child env with all `Key::String` entries (strictly materialized) → chain; last expression returned lazily
- [ ] `eval_surface_document` delegates its expression loop to `eval_document_exprs` (caps validation block stays in `eval_surface_document`)
- [ ] `builtin_eval` extracts `SurfaceNode` from each `Value::Expression`, builds initial env, then delegates to `eval_document_exprs`
- [ ] Verify `builtin_eval` callers handle lazy return (currently returns freshly-materialized value; unified path returns thunk)
- [ ] Update `doc/09-documents.md` §SEQ-SCOPE: document dynamic binding semantics (runtime Dict/Overlay promotion creates child env not modeled by resolver; tier-2 fallback in eval.rs rescues level-drifted Var lookups)
- [ ] Remove TODO comments from `src/builtins_meta.rs:1754` and `src/eval_pipeline.rs:174`

**Files:** `src/builtins_meta.rs`, `src/eval_pipeline.rs`, `doc/09-documents.md`

---

## Profiling and Call Tracing (`doc/whatif/profiling.md`)

Span-level profiling with dual attribution (materialization-context and creation-context), stall breakdown (I/O, network, channel, timer), and Perfetto trace output. Collection via `--profile spans.json`; analysis via `scripts/profile/` tinct programs against the span file. See `doc/12-tooling.md §Profiling`.

### profiling-review: Post-implementation review

**Whatif:** `profiling`
**Depends on:** `profiling-scripts`

- [ ] Run `/review-whatif profiling` — verify all sprints complete, implementation matches spec, docs consistent; address findings before closing

---

## JSON in Tinct — Remove serde_json from Rust

Goal: all JSON handling in tinct stdlib; `serde_json` removed from `Cargo.toml`.

**Sprint order:** json-no-stdin → json-delete-to-json → json-describe-tinct → json-pretty-indent → json-native-from-json → json-remove-serde-dep

### json-remove-serde-dep: Final serde_json cleanup after all JSON code moved to tinct

**Blocked on:** lib.rs (JsonVisitor), profiling.rs, lsp/server.rs still use serde_json directly.
**Depends on:** Migrating those 3 files off serde_json first (separate work).

- [ ] Remove dead error variants E041/E061/E062 (`JsonDepthExceeded`, `JsonParse`, `JsonRange`) from `src/error.rs` — no production callers after `builtin_from_json` deleted
- [ ] Remove stale E041/E061/E062 help text from `src/main.rs:4317,4445,4454`
- [ ] Fix `from-json` row misplaced in Rust builtins table in `doc/11-stdlib.md:247`; add `codecs/json.llt` to optional stdlib modules table
- [ ] Add `\uXXXX` surrogate pair handling to `json-parse-string-body` in `stdlib/codecs/json.llt` (U+D800–U+DFFF rejected with clean error)
- [ ] Remove vestigial `[include %libdir "strings.llt"]` from `stdlib/codecs/json.llt:16` (strings not used in single-dict version)
- [ ] Verify zero remaining `serde_json` references in `src/` (`src/`)
- [ ] Remove `serde_json = "1.0"` from `Cargo.toml`

### json-serde-removal ✅ DONE — Steps 1–3 complete (2026-05-26). See DONE.md.

## Typecheck–Runtime Unification (`doc/whatif/typecheck-runtime-unification.md`)

Unify the static type-checking path and runtime type-checking path so they derive from a single source of truth. Implementation sequence: 2 → 1 → 3 (see whatif for rationale).

- [x] Accept `doc/whatif/typecheck-runtime-unification.md` — Accepted 2026-05-25

### failed-bindings-error: Component 1 independent — failed_bindings → Type::Error

**Whatif:** `typecheck-runtime-unification`
**Spec chapters:** `doc/06-type-inference.md §Error Propagation`

The `failed_bindings → Type::Error` change is independent of Component 2 and can ship first. Fixes the E099 cascade bug where Unknown-typed entries create CoreExpr::Error nodes for reachable variables.

- [x] Change `failed_bindings` entries from `Type::Unknown` to `Type::Error` at 3 sites (`src/typecheck_dict.rs:413,592,608`)
- [x] Add `lower.rs` Type::Error guard: when `TypeAnnotationTable.get(&id) == Some(Type::Error)`, emit `CoreExpr::RuntimeTypeCheck` instead of `CoreExpr::TypeAssert { resolved_type: Type::Error }` (`src/lower.rs:159-164`)
- [x] Verify `unify(Error, T) = Ok(())` no-op behavior is preserved — no spurious cascade errors (`src/type_unify.rs:1777-1781`)
- [x] Verify `is_subtype(Error, X) = false` bidirectional rejection is preserved (`src/type_def.rs:396-399`)
- [x] Tests: corpus tests for E099 cascade fix — dict entry with T003'd dependency produces `Type::Error`, not `Unknown`; downstream uses produce `undefined_variable` error with `failed_bindings` note, not E099 runtime crash (`tests/corpus/typecheck/`)
- [x] Tests: verify T010 no longer fires for `failed_bindings` entries (they're Error, not Unknown) (`tests/corpus/typecheck/`)

### consistent-subtype: Component 3 — unified runtime type check

**Whatif:** `typecheck-runtime-unification`
**Spec chapters:** `doc/07-type-extensions.md §Consistent Subtyping`, `doc/08-evaluation.md §TypeAssert Runtime Validation`
**Depends on:** `failed-bindings-error`

Implement the AGT consistent subtyping relation and ground_type_of; replace value_matches_type with the unified path.

- [x] Implement `is_consistent_subtype(sub, sup) -> bool` in `src/type_def.rs` — AGT `~<:` relation per whatif sketch: Unknown/TypeVar guards, then structural recursion for Seq/Map/Record/Function/Union/Intersection, with `is_subtype` fallthrough for remaining cases (`src/type_def.rs`)
- [x] Implement `ground_type_of(v: &Value) -> Type` in `src/eval.rs` — per whatif sketch: primitives → concrete type, Dict → `Record(extract_row)`, Overlay → closed empty record, Seq → `Seq(Unknown)`, Function → erased params/ret, capability types → `Unknown`, Decimal/BigInt → `Unknown`, Builder → `Top`, catch-all → `Top` (`src/eval.rs`)
- [x] Implement `extract_row(map: &IndexMap<Key, ThunkId>) -> Row` in `src/eval.rs` — key-only extraction, all field types `Unknown`, `Key::Int` entries skipped (`src/eval.rs`)
- [x] Replace `value_matches_type` body with `is_consistent_subtype(ground_type_of(v), T)` — single-line delegation, no fast-path bypass (`src/eval.rs:572-668`)
- [x] Update `lower.rs` Type::Error guard for post-Component-3: emit `CoreExpr::TypeAssert { resolved_type: Type::Unknown }` instead of `CoreExpr::RuntimeTypeCheck` (Unknown passes via consistent subtyping) (`src/lower.rs`)
- [x] Tests: corpus tests for `is_consistent_subtype` — `Seq(Unknown) ~<: Seq(Int)` passes, `Record({a: Unknown}) ~<: Record({a: Int})` passes, `Int ~<: Str` fails, `Record({}) ~<: Record({a: Int})` fails (missing field), `Function([Unknown], Unknown) ~<: Function([Int], String)` passes (`tests/corpus/typecheck/`)
- [x] Tests: `ground_type_of` unit tests for each Value variant — verify correct Type mapping and no thunk forcing (`src/eval.rs`)
- [x] Tests: `extract_row` unit tests — empty dict, string-keyed dict, mixed int/string keys, verify no ThunkId access (`src/eval.rs`)
- [x] Tests: end-to-end TypeAssert — `[@Int 42]` passes, `[@Seq[Int] [seq 1 2 3]]` passes (tag-only), `[@[a: Int] {a: 1}]` passes (field presence), `[@String 42]` fails (`tests/corpus/eval/`)
- [x] Doc: add §Consistent Subtyping to `doc/07-type-extensions.md` — `is_consistent_subtype` definition, AGT Proposition 22, Seq/Dict element erasure caveat (`doc/07-type-extensions.md`)
- [x] Doc: update §TypeAssert Runtime Validation in `doc/08-evaluation.md` — `value_matches_type = is_consistent_subtype(ground_type_of(v), T)`, no fast-path, no dual-path (`doc/08-evaluation.md`)

### pipeline-expects-restructure: Pipeline expects: contract restructure

**Whatif:** `typecheck-runtime-unification`
**Spec chapters:** `doc/09-documents.md §Pipeline Contracts`
**Depends on:** `consistent-subtype`

Restructure pipeline `expects:` contracts to use resolved types instead of RuntimeTypeCheck string comparison.

- [x] Add `resolved_type: Option<Type>` field to `CoreExpr::RuntimeTypeCheck` in `src/ast.rs` (`src/ast.rs:903-907`)
- [x] Add `state.expects_resolved: HashMap<DocumentId, Type>` side table to typecheck state (`src/typecheck.rs`)
- [x] In typecheck `expects:` handler: instead of discarding the resolved type after advisory check, store it in `state.expects_resolved` (`src/typecheck.rs:307-341`)
- [x] Thread `expects_resolved` from typecheck output to `eval_surface_file_with_input` → `wrap_with_nominal_validation` (`src/eval_pipeline.rs`)
- [x] Update `wrap_with_nominal_validation` signature to accept `resolved_type: Option<Type>` and populate `RuntimeTypeCheck::resolved_type` (`src/eval_pipeline.rs:35-76`)
- [x] At force time: when `resolved_type` is `Some(ty)`, call `value_matches_type(v, ty)` instead of string comparison (`src/eval_materialize.rs:2595-2694`)
- [x] Handle eval-time macros producing TypeAssert nodes: either run typecheck on expanded output or restrict expansion to not produce TypeAssert without resolved types (`src/builtins_meta.rs`)
- [x] Tests: pipeline `expects:` contract with resolved type — verify structural type checking replaces nominal string comparison (`tests/corpus/eval/pipeline/`)

### runtime-typecheck-deletion: Delete RuntimeTypeCheck and cleanup

**Whatif:** `typecheck-runtime-unification`
**Spec chapters:** `doc/16-architecture.md §CoreExpr`
**Depends on:** `pipeline-expects-restructure`

Delete RuntimeTypeCheck entirely and remove all special-case code smells identified during review.

- [x] Delete `CoreExpr::RuntimeTypeCheck` variant from `src/ast.rs` (`src/ast.rs:903-907`)
- [x] Delete RuntimeTypeCheck string comparison fallback path — 105 lines (`src/eval_materialize.rs:2710-2814`)
- [x] Delete `type_name()` method from Value (no longer used for type checking) (`src/value.rs:771`) — verify no other callers remain; if used for error messages, keep but document as error-display-only
- [x] Convert all `RuntimeTypeCheck` construction sites to `CoreExpr::TypeAssert { resolved_type }` (`src/lower.rs`, `src/eval_pipeline.rs`)
- [x] Remove Handle validation always-true special case (32-line TODO block) — now handled by `ground_type_of → Type::Unknown` (`src/eval.rs:594-625`)
- [x] Remove TypeVar always-true special case — now handled by `is_consistent_subtype` TypeVar guard (`src/eval.rs:589`)
- [x] Remove Record always-true special case — now handled by `is_consistent_subtype` Record arm (`src/eval.rs:590`)
- [x] Remove Type::Error debug_assert — now handled by `is_consistent_subtype` Error guard (`src/eval.rs:663-666`)
- [x] Delete old `value_matches_type` match arms that are now dead code (the body is already `is_consistent_subtype(ground_type_of(v), T)` after consistent-subtype sprint) (`src/eval.rs`)
- [x] Delete `check_open` special case from typecheck.rs — 115 lines, replaced by typeclass instances (`src/typecheck.rs:3340-3455`)
- [x] Delete `check_tls_layer` special case from typecheck.rs — 42 lines, replaced by row polymorphism (`src/typecheck.rs:3812-3854`)
- [x] Verify `check_get` is already removed (handled by other Claude's sprint) — if not, delete it (`src/typecheck.rs:3875-3948`)
- [x] Tests: verify all existing TypeAssert corpus tests still pass after deletion (`tests/corpus/`)
- [x] Tests: verify no remaining `RuntimeTypeCheck` references in codebase (`src/`)
- [x] Doc: update `doc/16-architecture.md` — remove `RuntimeTypeCheck` from CoreExpr variant documentation (`doc/16-architecture.md`)

### typecheck-runtime-unification-review: Post-implementation review

**Whatif:** `typecheck-runtime-unification`
**Depends on:** `runtime-typecheck-deletion`

- [x] Run `/review-whatif typecheck-runtime-unification` — verify all sprints are complete, implementation matches spec (no stubs or de-scoped features), and main docs are consistent; address any findings before closing

## Research / Design Items

- [ ] Write `doc/whatif/filterable.md` proposal for `Filterable f` class (`doc/whatif/filterable.md`)
- [ ] Accept `doc/whatif/schema-directed-from-json.md` via `/rnd` and create implementation sprint (`doc/whatif/schema-directed-from-json.md`)
- [ ] (grammar-doc-polish): `ClassDecl.superclasses` is silently dropped in `surface_convert.rs:1740` (`superclasses: _`). Add `superclasses: [[class-name var-name] ...]` key to the ClassDecl dict schema. Format is directly `Vec<(String, String)>` — serialize as a Seq of 2-element Seqs. (`src/surface_convert.rs:1740`)

---

## Codebase Health Audit Findings (2026-05-24) — Sixth Full Panel Review

### doc-08-rv2-stale-evaluator: doc/08 Iterative Evaluator section stale after runtime-v2 [Critical]

**computer-scientist C1 + grammar-architect M4-M6.** `doc/08-evaluation.md` §Iterative Evaluator (lines 1377-1470) describes a machine that no longer exists:
- Line 1399: `Action::Eval { expr: Rc<Spanned<Expr>>, ... }` — deleted; replaced by `Action::EvalCore { expr: Arc<Spanned<CoreExpr>>, ... }`
- Line 1432: `eval_step(expr, env, ...)` — deleted; current is `eval_core_expr_pub()`
- Line 1467: "~18-20 Cont variants" — actual count is 6 (`Memoize`, `PendingCallDispatch`, `GuardedValidate`, `BuiltinForceArg`, `DotAccessForce`, `TypeAssertCheck`)
- Line 1007: Sequential routing references deleted `eval_recursive`
- Line 1461: "`deep_materialize` in `eval_deep.rs`" — `eval_deep.rs` deleted; moved to `eval_materialize.rs`
- Line 1419: compile-time assertion cited at "line 252" — actual `src/eval_materialize.rs:349`

- [x] Rewrite Action enum listing → EvalCore/Continue/Materialize; Cont enum → 6 variants; run() loop → eval_core_expr_pub() (lines 1399, 1430-1440, 1474)
- [x] Fix line 1007 Sequential routing note (no eval_recursive; references cek-match-sequential-rust-stack)
- [x] Fix line 1468 deep_materialize → eval_materialize.rs (eval_deep.rs deleted)
- [x] Fix line 1419 assertion line number → src/eval_materialize.rs:349

**Round 2 staleness (2026-05-25, computer-scientist):** The fixes above partially applied but the doc has drifted again:
- [x] Line 1408-1419: Cont enum listing shows 6 variants — actual is **11**: add SequentialStep, ForceAndBind, MatchDispatch, MatchGuardCheck, PredicateCheck
- [x] Line 1477: "6 variants" count claim → update to "11 variants"
- [x] Lines 1247-1272: §Deep Materialization section describes `deep_materialize` as an active function — **deleted entirely** (no references in src/). Replace with §Output Serialization describing `visit_value` visitor pattern in `src/lib.rs:657`
- [x] Line 1471: "deep_materialize: Implemented as a separate recursive function in eval_materialize.rs" — deleted; replace with note about visit_value
- [x] Line 1597: Recursive call table row "deep_materialize() → ... Cont::DeepEntries / Cont::DeepSeqTail" — neither implemented. Remove row.
- [x] Line 1467: References `Cont::DictBuildValue` and `Cont::BindArgDefault` — do not exist. Remove.
- [x] Line 1475: References `Cont::CallForceFunc` — does not exist (actual: PendingCallDispatch). Fix.
- [x] Line 1596: References `Cont::PendingCallForceFunc → Cont::PendingCallForceResult` — neither exists. Fix.
- [x] Line 1422: Compile-time assertion cited at "src/eval_materialize.rs:349" — actual line is **443**. Fix.
- [x] Line 429: References "MAX_COLLECT_SIZE in deep_materialize" — deep_materialize deleted. Fix.
- [x] Lines 1488-1494: FnAnnotation struct shows return_ann, constraints, source_span — actual struct (value.rs:29-34) has only doc and source_file. Update pseudocode.
- **File:** `doc/08-evaluation.md`

### fn-annotation-callability: @Fn bare annotation resolves to Type::Unknown [Major]

**type-theorist M3.** The `"Fn"` arm in `resolve_type_name` (`src/typecheck_annot.rs:1777-1788`) returns `Type::Unknown` to avoid false positives for ~50 prelude functions. Consequence: `[@Fn 42]` passes both static checking (via Unknown compatibility) and runtime checking (no TypeAssert fires). The correct encoding is `Type::Function { params: vec![], ret: Box::new(Type::Top), variadic: true }` — subsumes any callable under width subtyping.

- [ ] NEEDS_DESIGN: The correct encoding `Type::Function { params: vec![], ret: Top, variadic: true }` was tried but **unification fails** — tinct's unifier requires exact param count match, so a zero-param variadic function cannot unify with any concrete function type (`[fn [x] x]` has 1 param). The design question is: add a subtyping rule for zero-param variadic as top of the function lattice, OR enumerate and fix the ~50 prelude `@Fn` annotations individually? Requires `/rnd`.
- **Files:** `src/typecheck_annot.rs:1777-1788`, `src/type_unify.rs` (unification), `stdlib/prelude.llt`

### eval-error-test-health: Eval health items, error source file, test coverage gaps (13 tasks)

Combines: error-source-file (5), eval-health-321 (4), test-coverage-326 (4).

**Error source file in `(defined at ...)` messages:** `[E030] duplicate key: raise (defined at 2132:1-2132:66)` shows only `line:col` with no filename. `EvalError` has no `source_file` field.
- [x] Add `source_file: Option<Arc<str>>` field to `EvalError` (`src/error.rs`)
- [x] Update `EvalError::duplicate_key` to accept `source_file: Option<&str>`; set from `ctx.config.source_file.as_deref()` in `eval_dict.rs:201`
- [x] Update `EvalError::Display` to prefix span with `"<file>:"` when source file is known (`src/error.rs:1858`)
- [x] Audit other high-frequency `EvalError` constructors that run inside `eval_dict_core`/`eval_core_expr` and populate source file there
- [x] Add corpus test: `tests/corpus/errors/e030_source_file.llt` — verifies filename appears in `[E030]` output

**Placeholder/InProgress ambiguity (from eval-health-321):** Placeholder thunks indistinguishable from InProgress at storage level.
- [x] Add explicit Placeholder detection in `src/eval.rs:1796-1799` — issue structured `PlaceholderForced` error rather than panic or silent failure
- [x] Document the three-state ambiguity in `src/value.rs:1764` comment

**AstNodeField restore (from eval-health-321):**
- [x] Determine if `UnevaluatedState::AstNodeField` can raise non-cacheable errors; if yes, add `RestoreState::AstNodeField` variant; if no, document invariant explicitly

**PipelineBlame dead code (from eval-health-321):**
- [x] Implement PipelineBlame instantiation for `%@Type` pipeline validation (`src/error.rs:72-86`), OR delete PipelineBlame if feature is no longer planned

**CEK edge case tests (from test-coverage-326):**
- [x] Add 5 unit tests for continuation stack depth, GuardedValidate, RestoreState edge cases (`src/eval_materialize.rs`)
- [x] Add `tests/corpus/eval/errors/continuation_stack_exceeded.llt-eval` corpus test

**Error code coverage (from test-coverage-326):**
- [x] Fix or delete `tests/corpus/eval/errors/include_path_not_allowed.llt-eval` — update expected error code
- [x] Add corpus tests for E055/E056 when include functionality supports hash validation

### string-handle: Wrap a String as a readable Handle for uniform codec APIs

**Motivation:** All codecs and input programs should accept a `Handle` as their entry point — this enables lazy/streaming processing. But many callers have strings (e.g., inline data, test fixtures, small configs). Rather than duplicating every codec entry point, provide a single `string-handle` primitive that wraps a `String` as a `Handle[Readable]` backed by `std::io::Cursor<Vec<u8>>` (which implements `BufRead`). Then codecs can auto-dispatch: if given a String, wrap it; if given a Handle, stream it.

**Design:**
```llt
# Rust primitive (~20 lines):
string-handle: String → Handle[Readable]   # wraps Cursor<Vec<u8>> as BufRead

# Codec entry point (auto-dispatch):
from-json: [fn [let input]
  [if [str? input]
    [from-json [string-handle input]]
    [from-json-stream input]]]   # Handle path: lazy Seq<Value> for arrays

# Caller convenience:
[from-json "[1,2,3]"]           # String → auto-wrapped
[from-json [open cap "data.json" Readable]]  # Handle → lazy streaming
```

This applies to ALL codecs (`from-json`, `parse-toml-lite`, `parse-csv`, etc.) and all CLI input formatters.

**`string-handle` returns a Handle with `Readable` cap, no `Binary` cap** — compatible with `builtin-read-line` and `builtin-read-chunk` from `lazy-file-io`.

- [x] Add `builtin_string_handle` to `src/builtins_io.rs`: take String arg; create `Box::new(std::io::Cursor::new(s.into_bytes()))` as BufRead; wrap in `Value::Handle` with `{"Readable": {}}` caps; return Handle
- [x] Register `builtin!("string-handle", builtin_string_handle, [Strictness::Seq])` in `src/builtins.rs`
- [x] Add `"string-handle"` type entry in `src/type_env.rs`: `Str → Handle[Readable]`
- [x] Expose `string-handle` in prelude (public, not builtin-prefixed — it's a user-facing convenience)
- [ ] Update `from-json` dispatch in `stdlib/codecs/json.llt` to auto-wrap String via `string-handle` (deferred to from-json-streaming sprint)
- [ ] Update `parse-toml-lite` dispatch in `stdlib/codecs/toml-lite.llt` to auto-wrap String (deferred to future codec unification sprint)
- [ ] Update any other codec entry points similarly (deferred to future codec unification sprint)
- [x] Corpus test: `[string-handle "..."]` creates valid Handle readable by `builtin-read-line`
- **Files:** `src/builtins_io.rs`, `src/builtins.rs`, `src/type_env.rs`, `stdlib/prelude.llt`, `stdlib/codecs/json.llt`, `stdlib/codecs/toml-lite.llt`

**Depends on:** `lazy-file-io` (provides `builtin-read-line` / `builtin-read-chunk` that work with the resulting Handle)

### lazy-file-io: Replace `slurp`/`lines` Rust builtins with `builtin-read-line`/`builtin-read-chunk` primitives

**Decision:** No `slurp` at all — not even as LLT. Guide programmers toward lazy I/O; use `collect` only where a parser genuinely needs full text (making the materialization cost explicit). Work piecewise wherever possible. No backwards compatibility.

**Rust primitives (the only read builtins):**
- `builtin-read-line: Handle → String | []` — `BufRead::read_line()`, strips `\n`/`\r\n`, `[]` on EOF
- `builtin-read-chunk: Handle × Int → Bytes | []` — `Read::read()`, `[]` on EOF

**LLT prelude (private dict, `builtin-*` stable aliases):**
```llt
lines:  [fn [let h]   [let l [builtin-read-line h]]    [builtin-if [builtin-eq l []] [] [builtin-seq l [lines h]]]]
chunks: [fn [let h n] [let c [builtin-read-chunk h n]] [builtin-if [builtin-eq c []] [] [builtin-seq c [chunks h n]]]]
```
No `slurp`. Where full text is unavoidably needed (parsers), callers write `[join "\n" [collect [lines h]]]` explicitly — making the eagerness visible.

**Naming conflict:** `stdlib/prelude.llt:754` defines `lines: [fn [let s@String] [split "\n" s]]` (String → Seq). Deleted; use `[split "\n" s]` inline.

**Site-by-site analysis:** Piecewise where possible; explicit `collect` where not.

#### Rust (src/builtins_io.rs, src/builtins.rs, src/type_env.rs, src/typecheck.rs)

- [x] Add `builtin_read_line` — `expect_one_arg`; extract Handle; reject Binary cap; `BufRead::read_line()`; strip `\n`/`\r\n`; return String or `[]` on EOF
- [x] Add `builtin_read_chunk` — 2 pre-materialized args (Handle, Int); `Read::read(&mut buf[..n])`; return Bytes or `[]` on EOF; error on non-positive n
- [x] Delete `builtin_slurp` from `src/builtins_io.rs`
- [x] Delete `builtin_lines` + `builtin_lines_step` from `src/builtins_io.rs`
- [x] Update `src/builtins_io.rs` module doc comment
- [x] Remove `builtin_slurp`, `builtin_lines` imports and `"slurp"`, `"lines"` registrations from `src/builtins.rs`; add `builtin!("builtin-read-line", ..., [Strictness::Seq])` and `builtin!("builtin-read-chunk", ..., [Strictness::Seq, Strictness::Seq], 2)`
- [x] Update `src/builtins.rs` test assertions (remove `"slurp"`, add `"builtin-read-line"` / `"builtin-read-chunk"`)
- [x] Remove `"slurp"` and `"lines"` type entries from `src/type_env.rs`; add `"builtin-read-line"` (`Handle[Readable] → Str | []`) and `"builtin-read-chunk"` (`Handle[Readable] × Int → Bytes | []`)
- [x] Remove `check_slurp` function and `if name == "slurp"` dispatch from `src/typecheck.rs`

#### stdlib/prelude.llt

- [x] Delete string-splitting `lines` at line 754 — use `[split "\n" s]` inline
- [x] Add `lines` and `chunks` LLT definitions (before `fixed-clock`); no `slurp`
- [x] `include` (line ~2790): `[slurp [open cap path Readable]]` → `[join "\n" [collect [lines [open cap path Readable]]]]` — `load` needs full String; `collect` is explicit
- [x] `cli-pipeline` (line ~2818): same replacement as include

#### stdlib/cli/in/ndjson.llt — piecewise ✓

- [x] `[split "\n" [slurp %stdin]]` → `[lines %stdin]` — no collect, truly lazy

#### stdlib/cli/in/json.llt — streaming possible via from-json Handle overload (see from-json-streaming sprint)

- [x] For now: `[from-json [slurp %stdin]]` → `[from-json [join "\n" [collect [lines %stdin]]]]` — explicit collect; revisit when `from-json` gains Handle support (see `from-json-streaming` sprint below)

#### stdlib/cli/in/toml-lite.llt — piecewise possible

- [x] `[call %.parse-toml-lite [slurp %stdin]]` → `[call %.parse-toml-lite [lines %stdin]]` — requires changing `parse-toml-lite` in `stdlib/codecs/toml-lite.llt` to accept `Seq<String>` directly (it already collects and filters internally; skip the `[split "\n" text]` step when a Seq is passed)

#### stdlib/io.llt

- [x] Delete `read-file` (line 9) — it is `slurp` by another name; callers use `read-lines` or write `[join "\n" [collect [lines ...]]]` explicitly if they need a String
- [x] `read-lines` (line 15): `[lines [open cap path Readable]]` — works as-is with new LLT `lines`; update comment (no 10 MB limit, truly lazy)
- [x] `copy` (line 72): `[slurp [open cap src Readable Text]]` → stream piecewise: `[close [reduce [fn [let h line] [write-line h line]] [raw-create cap dst] [lines [open cap src Readable]]]]` — no collect, line-at-a-time

#### stdlib/net.llt — collect unavoidable (HTTP parser needs full response bytes)

- [ ] Line 73: `[bytes-str [slurp [write-handle ...]]]` → `[bytes-str [bytes-concat [collect [chunks [write-handle [connect cap Tcp url.host url.port] [str-bytes [build-http-request "GET" url.path url.host]]] 65536]]]]` — HTTP response must be fully buffered for header parsing; `collect` is explicit

#### stdlib/codecs/toml-lite.llt — accept Seq<String> to enable lazy toml input

- [x] Modify `parse-toml-lite` to accept either `String` or `Seq<String>`: if given a Seq, use it directly (skip `[split "\n" text]`); if given a String, split first. This enables the CLI codec to pass `[lines %stdin]` without collecting.

#### samples/versions.llt — collect unavoidable (toml parser needs full text)

- [x] Lines 16-17: `[slurp [open %cwd "Cargo.toml" Readable]]` → `[join "\n" [collect [lines [open %cwd "Cargo.toml" Readable]]]]` — passed to `parse-toml-lite` which needs full text (or update to use Seq form once toml-lite.llt is updated)

#### samples/tls_test2.llt — collect unavoidable (HTTP parser needs full response)

- [ ] Line 2: `[slurp [write-handle [tls-connect ...]]]` → `[bytes-str [bytes-concat [collect [chunks [write-handle ...] 65536]]]]`

#### scripts/docgen.llt — collect unavoidable (`load` needs full String)

- [x] Line 228: `[slurp [open %libdir full-path Readable]]` → `[join "\n" [collect [lines [open %libdir full-path Readable]]]]`

#### Tests

- [x] Corpus test: `ndjson` codec with multi-MB synthetic NDJSON processes all lines lazily (no E043, no collect)
- [x] Corpus test: `copy` streams correctly (source larger than any buffer)
- [x] Unit tests: `builtin-read-line` EOF → `[]`, newline strip, Binary cap error; `builtin-read-chunk` EOF → `[]`, partial read

**Files:** `src/builtins_io.rs`, `src/builtins.rs`, `src/type_env.rs`, `src/typecheck.rs`, `stdlib/prelude.llt`, `stdlib/cli/in/ndjson.llt`, `stdlib/cli/in/json.llt`, `stdlib/cli/in/toml-lite.llt`, `stdlib/codecs/toml-lite.llt`, `stdlib/io.llt`, `stdlib/net.llt`, `samples/versions.llt`, `samples/tls_test2.llt`, `scripts/docgen.llt`

### from-json-streaming: `from-json` on a Handle returns a lazy `Seq<Value>` for JSON arrays

**Depends on:** `json-native-from-json` (Rust `builtin_from_json` deleted; `codecs/json.llt` is sole implementation), `lazy-file-io` (`builtin-read-chunk` available)

**Motivation:** When stdin is a JSON array, `from-json` should return a lazy `Seq<Value>` — parsing one element per thunk force — rather than requiring the whole document in memory. This eliminates the `collect` in `stdlib/cli/in/json.llt`.

**Implementation is entirely in LLT (`stdlib/codecs/json.llt`), not Rust.** serde_json is being removed (`json-remove-serde-dep`); no Rust streaming parser is available. The tinct-native recursive-descent parser in `codecs/json.llt` must be extended to:
1. Accept a `Handle` argument (in addition to `String`)
2. When given a Handle and the top-level token is `[`: read and parse one JSON value at a time using `builtin-read-chunk` for buffered input; after each element, return `[seq element [from-json-array-step handle remaining-buf]]` — a lazy Seq where the tail thunk continues parsing from the handle's current position
3. When given a Handle and the top-level token is `{`: slurp the object fully (objects have no natural element boundary) — `[from-json [join "\n" [collect [lines handle]]]]`
4. When given a `String`: existing behavior unchanged

The key challenge: the LLT parser needs a mutable "read buffer" threaded through the lazy Seq tail thunks. The handle's position is naturally stateful (each read advances it), so the tail thunk just needs to know where in the partially-read buffer the next element starts. Design requires a `from-json-array-step: Handle × String → Seq<Value>` internal function that takes the handle and any unconsumed buffer from the previous chunk read.

**`load` / LLT parser:** Not worth streaming — LLT source files are code (small, not data), and the parser builds a full AST in memory regardless.

- [x] Design `from-json-array-step` in `codecs/json.llt`: takes Handle + leftover-buffer String; reads next chunk; finds next complete JSON value boundary; returns `[seq parsed-value [from-json-array-step handle remaining]]` or `[]` at `]`
- [x] Extend `from-json` entry point in `codecs/json.llt` to dispatch on `Handle` vs `String` argument
- [x] Update `check_from_json` type signature: add `Handle → Seq<Any>` overload alongside `String → Any` (`src/typecheck.rs` or `src/type_env.rs`)
- [x] Update `stdlib/cli/in/json.llt`: `[from-json [join "\n" [collect [lines %stdin]]]]` → `[from-json %stdin]`
- [x] Corpus test: large JSON array via stdin — first element forced before last is parsed
- **Files:** `stdlib/codecs/json.llt`, `src/typecheck.rs` (type signature only), `stdlib/cli/in/json.llt`

### type-system-cleanup: Fix type inference gaps, T013 diagnostics, and type system health (14 tasks)

Combines: t013-unknown-constraint-discharge (3), t013-instantiate-scheme-source-names (3), type-system-health-321 (8).

**T013 constraint provenance — show origin builtin, constraint class, and argument span:** The TypeVar name (`_t3`, or even `a` from the scheme) is invisible in user code and useless. The message should instead identify *which call* introduced the constraint, *which constraint class*, and *which argument* is the unconstrained one, pointing directly at the argument's source span. Target output:
```
warning[T013]: argument to `str` has unconstrained type — Showable constraint will be silently dropped
 --> trace.llt:11:14
  |
  11 |         [str "span-" s.id]
  |                      ^^^^ type of this argument is not statically known
```

- [x] Add `origin_name: Option<Arc<str>>` and `origin_span: Option<Span>` to `Constraint::Class` in `src/type_class.rs` — carries which function/builtin created the constraint and at which argument span
- [ ] Add `origin_name` and `origin_span` parameters to `instantiate_scheme` in `src/type_env.rs`; populate them on all new `Constraint::Class` entries created during instantiation
- [ ] Thread origin at VarRef call site (`src/typecheck.rs:1886`): pass `name` (the function name, e.g. `"str"`) and `node.span`; also thread at `check_call_with_scheme` (`src/typecheck.rs:4680`) and dot-access field scheme instantiation (`src/typecheck.rs:4513`)
- [ ] Track the argument-level span: when `instantiate_scheme` is called during argument type-checking, the per-argument span is available; store it on the constraint as `origin_span`
- [ ] In `emit_ambiguous_constraint_diagnostics` (`src/type_env.rs`): when `origin_name` and `origin_span` are set, emit message `"argument to '{name}' has unconstrained type — {class} constraint will be silently dropped"` with a secondary span at `origin_span` pointing to the specific argument; drop TypeVar name from message entirely
- [ ] Update `format_var_name` fallback (`src/type_env.rs:386`): when origin info is available, show only origin; when not, show the scheme's quantified name (e.g. `'a'`) without `(internal: _tN)` suffix
- [ ] Corpus test: T013 message cites the origin function and points to the argument span

**merge/first/last return types (from type-system-health-321):** `merge` typed as `(a, b) → Unknown`; `builtin-first`/`builtin-last` return Unknown.
- [x] Fix `merge` return type in `src/type_env.rs:2869` — change from Unknown to `Appendable a => (a, a) → a` or add fundep constraint
- [x] Fix `builtin-first` and `builtin-last` return types at `src/type_env.rs:3343,3353` — currently Unknown; should return fresh TypeVar

**Variant type wiring (from type-system-health-321):** `Variant` returns Unknown despite `Type::NominalVariant` existing.
- [x] Wire `Variant` builtin signature to construct `Type::NominalVariant` based on tag and payload (`src/type_env.rs:1892`)

**Handle capability types (from type-system-health-321):** Multiple I/O builtins return `Handle(Box::new(Type::Unknown))`.
- [ ] Audit all Handle-returning builtins at `src/type_env.rs:2127,2281,2446,2481,2492` — update to use precise capability rows

**collect_all_vars_vec wildcard (from type-system-health-321):** `_ => {}` would miss new compound Type variants.
- [x] Replace `_ => {}` in `collect_all_vars_vec` with exhaustive leaf enumeration (`src/type_def.rs:1320`)

**DOT-VAR field fallback (from type-system-health-321):** Absent field fallback uses Unknown instead of fresh TypeVar.
- [ ] Change `row.fields.get(field_name).cloned().unwrap_or(Type::Unknown)` to use `state.fresh_type_var()` at `src/type_unify.rs:580,584,595,597`

**Unknown boundary leakage (from type-system-health-321):**
- [ ] Add lint or documentation for Unknown-typed top-level bindings at document boundaries (`doc/05-type-annotations.md` §Gradual Typing Boundaries)

**is_subtype depth guard (from type-system-health-321):**
- [x] Add `MAX_SUBTYPE_DEPTH` guard to `is_subtype` analogous to `MAX_CONSTRAINT_DEPTH=256` (`src/type_def.rs`)

---

## Unified Binding Declarations — Remaining Work

### unified-bindings-structural-tests: Implement structural test patterns in [let ...] (name: Constructor)

**Whatif:** `unified-bindings`
**Depends on:** `unified-bindings-typecheck` (in DONE.md)

The core structural test feature from `doc/whatif/unified-bindings.md` — `[let v: Ok]` binding patterns in `[case ...]` arms — is NOT implemented. The parser's `StackFrame::LetDecl` colon handler routes to "named param with default" semantics, not constructor-test semantics. `src/typecheck.rs:5538-5539` explicitly says "structural test form is future work; the parser does not yet support colon inside [let ...] to express the constructor." This is a divergence: `doc/02-syntax.md §9` documents `[let v: Ok]` as a working feature.

Spec: `doc/whatif/unified-bindings.md §src/parser.rs`, `§src/typecheck.rs`, `§src/eval.rs`.

- [ ] Extend `StackFrame::LetDecl` Colon handler: when last binding is `VarRef` or `Annotated`, set `pending_key` for structural-test; next token (uppercase identifier = constructor name) closes the structural-test entry and pushes a structural-test binding node (`src/parser.rs` — `StackFrame::LetDecl` Colon arm)
- [ ] Extend nested bracket inside `[let ...]` to always produce sub-LetDecl for multi-payload: `[let [a b]: Pair]` pushes `StackFrame::LetDecl` for the inner bracket (`src/parser.rs`)
- [ ] Remove stub comment at `src/typecheck.rs:5536-5539`; implement constructor payload lookup: for each `name: Constructor` binding, look up `Constructor` in `TypeEnv` as a function type scheme and extract domain type as payload type; bind `name` to that type (`src/typecheck.rs` — `typecheck_case_arm`)
- [ ] Implement soft-skip eval for structural tests: when `[let v: Constructor]` pattern is in a case arm, materialize scrutinee, check tag against constructor name, extract payload and bind; return `None` on tag mismatch (arm skip) (`src/eval.rs`)
- [ ] Add dead-arm warning when `payload_type(Constructor) ∩ annotation_type = Never` (e.g., `[let v@String: Ok]` where Ok payload is Int) (`src/typecheck.rs`)
- [x] Tests: `case_structural_ok_err.llt-eval` (basic Ok/Err patterns); `case_structural_nested.llt-eval` (`[let [a b]: Pair]`); `case_structural_typed_payload.llt-eval` (`[let v@Int: Ok]`); `case_structural_mismatch_skips.llt-eval` (soft-skip); `case_structural_dead_arm.llt-eval` (dead-arm warning) (`tests/corpus/eval/`)

---

## Stdlib Health — Merged Sprint

### stdlib-health-cleanup: Builtin privacy migration, encapsulation fixes, stdlib doc/test gaps (21 tasks)

Combines: stdlib-conformance-builtin-privacy (8), stdlib-conformance-cleanup (6), stdlib-health-326 (7). `macros.llt` and `ast.llt` are exempt from builtin-privacy (documented).

**Builtin privacy — migrate non-prelude files off raw `builtin-*` calls:**
- [x] `stdlib/net.llt`: Replace `builtin-if`→`if`, `builtin-eq`→`=`, `builtin-length`→`length`, `builtin-split`→`split`, `builtin-merge`→`merge`, `builtin-get`→`get`, `builtin-to-int`→`to-int`, `builtin-reduce`→`reduce`, `builtin-str`→`str`, `builtin-null?`→`null?`, `builtin-raise`→`raise`, `builtin-try`→`try`, `builtin-rest`→`rest`
- [x] `stdlib/encoding.llt`: Replace all `builtin-if`, `builtin-lt`, `builtin-add`, `builtin-sub`, `builtin-mul` with `if`, `<`, `+`, `-`, `*` prelude wrappers
- [x] `stdlib/async.llt`: `retry-impl` `builtin-if`→`if`, `builtin-sub`→`-`, `builtin-raise`→`raise`; `loop-select-impl` `builtin-if`→`if`; `exit`/`graceful-exit`/`finally` `builtin-raise`→`raise`
- [x] `stdlib/codecs/json.llt`: Replace all `builtin-*` with prelude wrappers (`str`, `if`, `=`, `<`, `+`, `raise`, `str-slice`, `str?`, `null?`)
- [x] `stdlib/protocols/dns.llt`: Replace all `builtin-*` arithmetic/control calls with prelude wrappers
- [x] `stdlib/protocols/websocket.llt`: Replace all `builtin-*` calls with prelude wrappers
- [x] `stdlib/protocols/socks5.llt`: Replace all `builtin-*` calls with prelude wrappers
- [x] `stdlib/protocols/grpc.llt`: Replace all `builtin-*` calls with prelude wrappers

**Correctness — verify `loop-select` depth limit post-CEK:**
- [x] Verify whether `loop-select` 230-iteration depth limit (`stdlib/async.llt:178-181`) still applies after CEK machine sprint; update or remove the warning — TCO implemented in tco-proper sprint; comment updated to reflect O(1) stack depth

**Encapsulation — split single-dict files into two-dict pattern:**
NOTE: These tasks are DEFERRED pending bare-include-scope sprint. The two-dict pattern has closure scoping issues (discovered in json-serde-removal). Once bare-include-scope is implemented, these files should use the bare include pattern where private helpers are in scope for the public dict without needing a two-dict split.
- [ ] DEFERRED: Split `stdlib/encoding.llt` into two-dict document pattern (private helpers → first dict, public API → second dict)
- [ ] DEFERRED: Split `stdlib/cli/out/csv.llt` into two-dict pattern (Private: `csv-quote`, `csv-header`, `csv-row`, `csv-rows`, `csv-impl`. Public: `csv`)
- [ ] DEFERRED: Split `stdlib/cli/out/env.llt` into two-dict pattern (Private: `env-entry`, `env-entries`. Public: `env`)
- [ ] DEFERRED: Split `stdlib/cli/out/yaml.llt` into two-dict pattern (Private: `yaml-*` helpers. Public: `yaml`)
- [ ] DEFERRED: Split `stdlib/cli/out/toml.llt` into two-dict pattern (Private: `toml-*` helpers. Public: `toml`)

**Missing comparison aliases (from stdlib-health-326):**
- [x] Add `builtin-gte`, `builtin-lte`, `builtin-gt` to `src/builtins.rs:standard_builtins()`
- [x] Update `stdlib/prelude.llt` private helpers to use `builtin-gte` instead of `gte-impl` workaround

**Undocumented functions (from stdlib-health-326):**
- [x] Add `variant?`, `payload-of`, `unindent` to `doc/11-stdlib.md` with signatures and examples
- [x] Add corpus tests for `variant?`, `payload-of`, `unindent` in `tests/corpus/eval/stdlib/` — tests already exist

**Stale doc classifications (from stdlib-health-326):**
- [x] Fix `num?`, `record?`, `map?` classification in `doc/11-stdlib.md:172-183` to "LLT stdlib" — updated line 373 with complete predicate list
- [x] Update stable `builtin-*` alias list in `doc/11-stdlib.md:238-246` to include all current aliases — added builtin-gt, builtin-lte, builtin-gte
- [x] Update stale LLT function count `~117` in `doc/11-stdlib.md:358` to actual count — updated to ~140 (conservative estimate based on public dict analysis)

---

## Codebase Health Audit (Cycle #341, 2026-05-27)

### post-io-sprint-cleanup: Tests, type fixes, and codec cleanup from lazy-file-io + json-serde-removal (11 tasks)

Combines: lazy-file-io-tests (3), json-codec-cleanup (2), io-builtin-types (2), from-json-error-tests (4).

**Corpus + unit tests for builtin-read-line/builtin-read-chunk [Critical]:**
- [ ] Add corpus tests: `tests/corpus/eval/builtins/read_line_file.llt-eval`, `read_line_eof.llt-eval`, `read_chunk_file.llt-eval`, `read_chunk_boundary.llt-eval`, error tests for invalid handle type (×2)
- [ ] Add unit tests in `src/builtins_io.rs`: closed handle error, invalid type, chunk size ≤0, EOF detection, partial read, \r\n stripping
- [ ] Corpus test: `read_line_stdin.llt-eval` — verify \n stripped, [] on EOF, lazy Seq via prelude `lines`

**Delete vestigial strings.llt include [Critical]:** `stdlib/codecs/json.llt:16` has dead `[include %libdir "strings.llt"]`.
- [x] Delete `[include %libdir "strings.llt"]` at `stdlib/codecs/json.llt:16`

**Fix Type::Unknown return types [Critical]:** Both new I/O builtins return Unknown instead of proper union.
- [x] Fix `builtin-read-line` return type to `Type::Union([Type::Str, Type::Record(empty_row)])` at `src/type_env.rs:2150`
- [x] Fix `builtin-read-chunk` return type to `Type::Union([Type::Bytes, Type::Record(empty_row)])` at `src/type_env.rs:2162`

**from-json error tests [Major]:** Only 1 error test exists. Missing: invalid escape, trailing comma, unclosed string/array.
- [x] Add `tests/corpus/eval/stdlib/from_json_invalid_escape.llt-eval` — `[from-json "\"\\q\""]` raises E080
- [x] Add `tests/corpus/eval/stdlib/from_json_trailing_comma.llt-eval` — `[from-json "[1,]"]` raises E080
- [x] Add `tests/corpus/eval/stdlib/from_json_unclosed_string.llt-eval` — `[from-json "\"abc"]` raises E080
- [x] Add `tests/corpus/eval/stdlib/from_json_unclosed_array.llt-eval` — `[from-json "[1, 2"]` raises E080

### correctness-doc-fixes: Pattern linearity doc, letrec self-ref, sequential generalization, spec consistency (10 tasks)

Combines: pattern-linearity-doc (2), letrec-self-ref-silent (2), sequential-let-generalization (2), doc-spec-consistency-341 (4).

**Pattern linearity documentation [Major]:** `check_pattern_linearity` is `#[cfg(test)]` only — production accepts non-linear patterns with last-binding-wins. Undocumented.
- [x] Document last-binding-wins semantics in `doc/14-patterns.md` as a deliberate language design decision
- [x] Remove or repurpose the `#[cfg(test)]`-gated linearity functions (dead in production)

**Letrec self-reference diagnostic [Major]:** `[name: name]` in letrec dict silently cycles via try-or. Add diagnostic.
- [x] Add T002/T003 diagnostic: warn when a dict entry's value is `VarRef(name)` matching its own key — likely letrec self-reference (`src/resolve.rs` or `src/typecheck.rs`)
- [x] Alternatively: evaluate dict value expressions in PARENT scope when value is a bare VarRef matching its own key

**Sequential let-generalization [Major]:** Sequential handler wraps bare Type via mono(), losing polymorphism.
- [x] In `src/typecheck.rs:1960-1969`: extract TypeSchemes from infer_dict instead of stripping to bare Type; use `child_env.insert_scheme()` to preserve polymorphism
- [x] Corpus test: sequential polymorphic function used at two different types

**Doc/spec consistency [Major]:** Grammar spec inconsistencies found by grammar-architect.
- [x] `doc/02-syntax.md:838-843`: Clarify bracket access removal — explain `a[0]` was removed
- [x] `doc/15-ast.md:256,435-461`: Move Pipe desugaring to §Lowering Pass Rules (Pipe lowered after typecheck, not during desugar)
- [x] `doc/02-syntax.md:798-799`: Audit StackFrame::DocumentHeader for section header component order
- [x] `doc/02-syntax.md:549,575` + `doc/15-ast.md:393-432`: Clarify annotation bracket restriction classification

---

## Codebase Health Audit (Cycle #346, 2026-05-27) — TCO + Integration Review

### tco-fixups: TCO correctness fixes found in Cycle #346 analysis [Major]

Three issues found in the tco-proper implementation:

**Misleading comment about thunk result [Major]:** eval_materialize.rs:1579-1581 says "The outer thunk's result will be set by whatever the body evaluates to" — this is FALSE. In TCO mode, the thunk stays InProgress and is dropped (count==1 means no other references). Result flows via Action::EvalCore, not by setting the thunk.
- [x] Fix misleading TCO comment at `src/eval_materialize.rs:1579-1581` — replace with accurate abandonment explanation: "TCO abandonment: this thunk stays InProgress and will be dropped (strong_count==1). Result flows via Action::EvalCore → run loop → new thunk."

**Variant constructor arms ignore tail_hint [Major]:** When tail_hint=true, Value::Variant arms in apply_cont unconditionally call thunk.set_materialized() on a thunk about to be dropped (count→0). Writes to a OnceCell nobody will read. Value::Builtin arm correctly checks tail_hint; Variant arms do not.
- [x] Guard `set_materialized` behind `if !tail_hint` in Value::Variant arms of apply_cont(PendingCallDispatch) at `src/eval_materialize.rs:1825,1853`

**invoke_function_tco variant constructor dead code [Major]:** The `__variant_tag__` marker in eval_call.rs:237-253 is never inserted anywhere. The variant constructor check in invoke_function_tco is dead code. If reachable, the Err(internal(...)) would propagate incorrectly on the TCO error path.
- [x] Remove dead variant constructor check from `invoke_function_tco` in `src/eval_call.rs:237-253`, or handle variant construction inline in the Value::Function TCO arm

### tco-profiling-spans: TCO collapses profiling spans [Major]

TCO path (eval_materialize.rs:1560-1612) never creates a ProfilingSpanGuard for the tail call. Non-TCO path inherits guard via Memoize continuation. When profiling is enabled, parent span stays open through tail call body, producing merged timing instead of separate child spans.

- [ ] When profiling is enabled and tail_hint=true, either push a synthetic continuation to close parent span before EvalCore, or add explicit span handoff in ProfilingSpanGuard before returning Action::EvalCore (`src/eval_materialize.rs:1560-1612`, `src/profiling.rs`)

### io-async-integration: builtin-read-line/read-chunk nested sync in async context [Major]

stdlib/prelude.llt lines/chunks use builtin-read-line/builtin-read-chunk which call materialize_sync (aliased as materialize in builtins_io.rs). materialize_sync calls async_rt::block_on_anywhere() internally. When the outer context is already async, this nests runtimes. For large files via [collect [lines handle]], creates O(n) nested block_on calls.

- [ ] Document that file I/O builtins (builtin-read-line, builtin-read-chunk) are synchronous-only in `src/builtins_io.rs` module doc and `doc/11-stdlib.md`
- [ ] OR: make builtin_read_line/builtin_read_chunk proper async builtins that don't need materialize_sync

### json-codec-namespace-leak: codecs/json.llt single-dict leaks helpers into json.* namespace [Major]

After restructuring codecs/json.llt to single-dict (for closure scoping), all 180+ helper functions (json-escape, json-string, json-char-at, json-parse-value, etc.) are visible via `[json: [include %libdir "codecs/json.llt"]]` as `json.json-escape`, `json.json-char-at`, etc. This violates the two-dict encapsulation pattern.

- [ ] Restructure codecs/json.llt back to two-dict pattern but fix the closure issue correctly: use the bare-include-scope sprint (TODO.md) to make bare includes promote bindings into scope, which will allow the private dict's bindings to be in scope for the public dict's functions
- [ ] OR: accept the current single-dict structure and document that codecs/json.llt is intentionally a flat namespace (only accessible via named include `[json: [include ...]]` which namespaces all symbols anyway)

