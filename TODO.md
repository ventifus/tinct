# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

---

## Performance & Doc Fixes

### perf-doc-fixes: Eliminate PendingBuiltin pre-clone, fix stale stdlib/error docs

Independent quick fixes across performance and documentation.

- [ ] [Major] Move `args.clone()`, `named.clone()`, `ctx.clone()` in `src/eval_materialize.rs:474-477` to inside the depth-exceeded error branch only — successful builtin calls pay 3 unnecessary clones per call
- [ ] [Major] Update `doc/11-stdlib.md:297` Rust builtin count from 178 to 202 and fix wrapper count mismatch (doc says 12, code has 27+ shadowable wrappers in `stdlib/prelude.llt:1117-1252`)
- [ ] [Major] Add `collect-kv`, `str-repeat`, `str-find` to `doc/11-stdlib.md` function reference with correct module attribution (all in `stdlib/prelude.llt`, doc incorrectly says `strings.llt`)
- [ ] [Major] Add `_` placeholder lambda documentation subsection to `doc/11-stdlib.md` — critical composition pattern enabling `[map [+ _ 1] list]`, completely missing from docs
- [ ] [Minor] Add `UriParseError` (E063) and `SchemaViolation` (E090) to `doc/10-errors.md` error categories table — implemented with error codes but absent from spec
- [ ] [Minor] Fix comment syntax in `src/error.rs:234,249,250` — change `/` to `///` for rustdoc
- [ ] [Minor] Remove or mark as future `versions.llt` module reference in `doc/11-stdlib.md:275` — file does not exist
- [ ] [Minor] Fix interpolated string inner span loss in `src/parser.rs:146` — extend `ParseError` with optional `context: Option<Box<ParseError>>` to preserve both outer and inner error locations when `${...}` parse fails
- [ ] [Minor] Restrict `instantiate_at_level` in `src/type_env.rs:47-80` to only freshen quantified vars — currently freshens all vars in the type, violating principal-type invariant in theory (harmless in practice, but incorrect per Algorithm W)
- [ ] [Minor] Fix per-entry propagation overlap arm in `src/typecheck_dict.rs:148-151` — unification failure during local-to-state.subst merge silently drops the error (no push to `errors`); Pass 3b correctly uses `errors.push(e)` for the analogous case (type-theorist, computer-scientist)
- [ ] [Minor] Register `each-kv`, `each-values`, `each-key` and related iteration builtins in TypeEnv so they don't produce spurious `undefined variable` type warnings — they exist at eval time but lack type env registration (`tests/corpus/eval/builtins/each_kv_collect.llt-eval`)

---

## LSP Improvements

### lsp-caps-and-on-demand: LSP caps assumption + on-demand file loading

- [ ] [Major] Skip caps validation in LSP mode: `eval_pipeline.rs:47-104` checks that declared caps are present at runtime, but the LSP never injects caps (no `--cap-net` etc.), so any `--- caps:` program fails eval with spurious diagnostics. Fix: when running in `no_fs=true` LSP mode (or add a `lsp_mode: bool` flag to `EvalConfig`), pre-seed the eval env with stub values matching the caps declarations so cap validation succeeds. Type checker already handles this correctly (`src/typecheck.rs:502-521`). (Verified by assumption-skeptic agent, 2026-05-10)
- [ ] [Major] In `src/lsp/server.rs` hover handler: before calling `hover_at`, check if the URI is in the document map; if not, read the file from disk via `std::fs::read_to_string` and construct a temporary `Document` (run parse + typecheck), then call `hover_at` on it — same pattern as `document.rs` open handler
- [ ] [Major] Apply the same on-demand loading to the `GotoDefinition` handler in `src/lsp/server.rs`
- [ ] [Minor] Extract the shared "load document from URI path" logic into a helper `fn load_doc_from_uri(uri: &Url) -> Option<Document>` in `src/lsp/document.rs` to avoid duplicating the parse+typecheck sequence across handlers
- [ ] [Minor] Add LSP corpus tests in `tests/lsp_corpus_tests.rs` that send hover/goto requests without a prior `didOpen` and assert non-empty results

### lsp-completion: Implement textDocument/completion

Autocomplete is the single highest-value missing LSP feature for VS Code users. Complete
dict keys visible in the current scope, `$`-prefixed builtins, and prelude function names.

- [ ] [Major] Add `completion_at(doc: &Document, uri: &Url, offset: usize, include_graph: &IncludeGraph) -> Vec<CompletionItem>` to `src/lsp/analysis.rs`: walk the AST to collect all dict entry key names visible at the cursor's scope depth, return each as a `CompletionItem` with `kind: Variable`
- [ ] [Major] Add builtin name completions: when the cursor is after `$` or at a bare word, include all builtin names from `standard_builtins()` as `CompletionItem` with `kind: Function` — source the list from `src/builtins.rs`
- [ ] [Major] Add `Completion::METHOD` handler in `src/lsp/server.rs`: convert LSP position to offset, call `completion_at`, serialize to `CompletionResponse::Array`
- [ ] [Minor] Register `completion_provider: Some(CompletionOptions::default())` in the `ServerCapabilities` block in `src/lsp/server.rs`
- [ ] [Minor] Add prelude function completions: seed completion list with names exported by `stdlib/prelude.llt` (parse prelude at LSP startup, extract top-level key names) — reuse the include-graph infrastructure already in place

---

## Doc Verification

### doc-verify-error-codes: doc/10-errors.md missing and mismatched error code entries

Audit of `doc/10-errors.md` against `src/error.rs` found the following discrepancies between the documented and implemented error model. Docs are authoritative; source needs updates where noted, or docs need additions where new source variants are undocumented.

- [ ] [Major] Add `UriParseError` (E063, Conversion category) to all three doc/10-errors.md tables: the variant catalog (Part 1), the error codes table (Part 2), and the error categories table — source has `ErrorKind::UriParseError { detail: String }` with code `"E063"` and display `"URI parse error: {detail}"` (`src/error.rs:227-229, 295, 592`); entirely absent from docs (note: already tracked as a Minor item in perf-doc-fixes but that item bundles it with SchemaViolation and only targets the categories table — the variant catalog and code table also need updates)
- [ ] [Major] Add `SchemaViolation` (E090, new "Schema validation" category) to all three doc/10-errors.md tables: the variant catalog (Part 1), the error codes table (Part 2), and the error categories table — source has `ErrorKind::SchemaViolation { violations: Vec<(String, String)> }` with code `"E090"` and multi-line display `"schema validation failed with {n} error(s):\n  {field}: {msg}\n..."` (`src/error.rs:247-257, 298, 606-616`); entirely absent from docs
- [ ] [Major] Fix `CircularDependency` variant definition in doc/10-errors.md Part 1 variant catalog and Part 2 constructor table — source has `CircularDependency { name: String, cycle_path: Vec<(String, Span)> }` and constructor `circular_dependency(name, definition_span, cycle_path)` (`src/error.rs:232-239, 978-996`), but doc shows only `CircularDependency { name: String }` and constructor `circular_dependency(name, span)`. The Display also includes cycle path output (`"\n  cycle: {label} ({span}) → ... [back to {name}]"`) not mentioned in the message pattern table.
- [ ] [Major] Fix `EvalError` struct field name in doc/10-errors.md Part 1 representation — doc shows field `sec_span: Option<(Span, String)>` but actual field is `secondary_span: Option<(Span, String)>` (`src/error.rs:824`). Also doc's struct omits three fields present in source: `macro_expansion: Option<(String, Span)>`, `blame: Option<BlameLabel>`, and `pipeline_stage: Option<PipelineBlame>` (`src/error.rs:829-835`).
- [ ] [Major] Correct the "34 ErrorKind variants" claim in the "Error Categories — Complete Reference" section header — source has 36 variants (the listed 34 plus `UriParseError` and `SchemaViolation`), so the introductory sentence "All 34 ErrorKind variants map to stable error codes" and exhaustiveness claim "The variants above are exhaustive" are both wrong (`doc/10-errors.md:889, 928`).
- [ ] [Minor] Fix the Internal variant's category comment in doc/10-errors.md Part 1 variant catalog — doc shows `// --- Escape hatch (E090-E099) ---` for `Internal`, but source shows `// --- Schema validation (E090-E094) ---` for `SchemaViolation` and `// --- Escape hatch (E095-E099) ---` for `Internal` (`src/error.rs:246-255`). The doc range needs to change to `E095-E099` for `Internal`.
- [ ] [Minor] Correct the `EvalError::Display` format description in doc/10-errors.md Part 4 — doc states the materialization clause always uses the word "materialized at" (e.g., example output `"(materialized at 7:1-7:8)"`) but source dynamically infers the verb via `infer_materialization_verb()`: outputs `"called at"` for function-call frames, `"accessed at"` for field-access frames, and `"materialized at"` only as a fallback (`src/error.rs:1501-1517, 1570`). The display format spec and example output need updating.
- [ ] [Minor] Correct the `sec_span` populated-at-sites table in doc/10-errors.md Part 1 — doc claims `sec_span` is populated at three sites including "Builtin argument type mismatch (`require_num`, `require_string`, `require_dict`, `require_bool`)" with label `"argument produced here"`, but no such `with_secondary_span` call exists in any `require_*` path in source; the label `"argument produced here"` does not appear anywhere in the codebase. Actual `with_secondary_span` call sites are: `ThunkState::Guarded` failures with label `"value produced here"` (`src/eval_materialize.rs:1012, 1069`) and `if` condition type mismatch with label `"condition evaluated to {type} here"` (`src/builtins_math.rs:499-502`). The `require_*` builtin-argument site is documented but unimplemented.
- [ ] [Minor] Mark `EvalError::depth_exceeded()` constructor as test-only in doc/10-errors.md Part 2 constructor table, or add note that it is `#[cfg(test)]` (`src/error.rs:998-1010`) — doc presents it as a public constructor alongside the others with no indication it is test-only.

---

### doc-verify-stdlib: doc/11-stdlib.md and doc/11a-builtins.md against implementation

Full audit of `doc/11-stdlib.md` and `doc/11a-builtins.md` against `src/builtins.rs`, `src/builtins_meta.rs`, `src/builtins_seq_gen.rs`, and `stdlib/prelude.llt`. Docs are authoritative — implementation gaps produce TODO items; doc inaccuracies that the source has already diverged from are recorded here for doc fixes.

**Builtin counts:**

- [ ] [Major] Fix builtin count throughout: `doc/11a-builtins.md:2` says "178 Rust-native builtins" and `doc/11-stdlib.md:297` repeats "178" — actual count per `standard_builtins_count()` test at `src/builtins.rs:6177` is **189**. The summary in `doc/11a-builtins.md:680` says "90 Rust-native builtins + 12 stable aliases = 102" which is also wrong (189 + 27 stable aliases). Update all three sites to match the value in the test assertion.

**`try` return format — wrong tag keys in both docs:**

- [ ] [Major] Fix `$try` return format in `doc/11a-builtins.md:142` and `doc/11-stdlib.md:158`. Both docs say `$try` returns `[ok: value]` on success and `[error: msg]` or `[err: message]` on failure (tagged dicts with string keys). The actual implementation (`src/builtins_meta.rs:160-190`) returns `Value::Variant { tag: "Ok", payload: Some(value) }` on success and `Value::Variant { tag: "Err", payload: Some(message) }` on failure — these are ADT variants, not dicts with keys "ok"/"error". User code must use `$match` or `$tag-of` to destructure them, not dict access (`result.ok`). Update both docs to say: "Returns `[Ok value]` on success or `[Err message]` on failure (ADT variants, destructured with `match`)."

**`tls-connect` does not exist — `tls-layer` is the registered builtin:**

- [ ] [Major] The `doc/11a-builtins.md:385-432` section documents `tls-connect` as a Rust builtin with signatures for Connector form and Handle form. `tls-connect` does NOT appear in `standard_builtins()` (`src/builtins.rs:960-1437`). The actual registered builtin is `tls-layer` (line 1141). Rename the section to `tls-layer` and update all examples and signatures accordingly. The two-form description (Connector form opens fresh TCP+TLS; Handle form layers TLS on existing stream) needs to be reconciled with `tls-layer`'s actual 3-arg signature `handle sni opts`.

**`http-connect` does not exist as a Rust builtin:**

- [ ] [Major] `doc/11a-builtins.md:529-555` documents `http-connect` as a Rust builtin. There is no `http-connect` in `standard_builtins()` (`src/builtins.rs:960-1437`) — the name does not exist anywhere in `src/`. Remove this section or mark it as a planned future builtin. The related `fetch` described in the same section is an LLT function in `stdlib/net.llt:78-79`, not a Rust builtin.

**`uri-params`, `uri-origin`, `uri->string` are LLT functions, not Rust builtins:**

- [ ] [Major] `doc/11a-builtins.md:608-630` documents `uri-params`, `uri-origin`, `uri->string` as Rust builtins. They are NOT in `standard_builtins()`. All three are implemented in `stdlib/net.llt:83-107` as LLT functions. `doc/11-stdlib.md:311` also incorrectly lists them as Rust builtins alongside `uri`, `url`, `urn`. Move the reference for these three functions to the LLT stdlib section in both docs, under `stdlib/net.llt`.

**`range` 1-arg description is wrong in doc/11a-builtins.md:**

- [ ] [Major] `doc/11a-builtins.md:275` says "`[call $range 5]` → `0..5`". This is doubly wrong: (1) 1-arg range is infinite starting from the given argument, not a 0-based bounded range, and (2) the notation implies it produces 0 through 4, whereas `[call $range 5]` actually produces `5, 6, 7, ...` forever. Correct to: "`[call $range 5]` → infinite Seq: `5, 6, 7, ...`". Source: `src/builtins_seq_gen.rs:57-66`.

**Timestamp and duration builtin names use `->` not `-to-`:**

- [ ] [Major] `doc/11-stdlib.md:309` lists `timestamp-to-unix` and `unix-to-timestamp` as Rust builtin names. Actual registered names are `timestamp->unix` and `unix->timestamp` (arrow notation, consistent with `uri->string`). Same issue: `duration-to-seconds` and `duration-to-nanos` in the doc vs actual `duration->seconds` and `duration->nanos` in `standard_builtins()` (`src/builtins.rs:1365-1373`). Fix all four names in `doc/11-stdlib.md:309` and any other references.

**`=` builtin description in both docs is stale — structural equality for dicts is now implemented:**

- [ ] [Minor] `doc/11a-builtins.md:49` says `=` uses "reference equality" for dicts and `doc/11-stdlib.md:788-793` says EQ-INCOMP returns `false` for all non-matching type pairs. Both are stale: `src/builtins_math.rs:268-400` now implements full structural equality for dicts (order-insensitive key comparison, recursive value comparison with cycle detection via coinduction). Variants also support recursive structural equality. Update both docs: `$=` performs structural equality for dicts and variants; functions and seqs still return `false`. Update the EQ-INCOMP dispatch table to add Dict/Dict and Variant/Variant rows.

**`until` is now a Rust builtin, not a recursive LLT function:**

- [ ] [Minor] `doc/11-stdlib.md:382` says `until` "Recursive; hits MAX_EVAL_DEPTH (~256) on large inputs". `until` was moved to Rust in `src/builtins_meta.rs:194-261` (using an explicit Rust loop) expressly to avoid this limit. The prelude even has a comment at line 513 saying "until: Moved to Rust builtin to avoid recursion depth limit." Update the doc to say `until` is a Rust builtin with unlimited iterations, and remove the depth warning.

**Undocumented prelude functions (beyond already-tracked `collect-kv`, `str-repeat`, `str-find`):**

- [ ] [Minor] Add the following prelude functions to the function reference in `doc/11-stdlib.md` — they exist in `stdlib/prelude.llt` but are absent from both docs:
  - `sorted` (line 675): like `sort` but accepts Seq or Dict input; collects a Seq first
  - `sorted-by` (line 681): like `sort-by` but accepts Seq or Dict input
  - `between` (line 1171): predicate factory `lo hi → (v → Bool)` for inclusive range check
  - `non-negative` (line 1178): predicate for `v >= 0`
  - `positive` (line 1185): predicate for `v > 0`
  - `Result` type and combinators (`and-then`, `result-map`, `result-or`, `result-ok`, `result` monad dict) at lines 1031-1077: Result ADT with Haskell-style bind/map/or combinators; entirely absent from docs

**Undocumented Rust builtins in doc/11a-builtins.md:**

- [ ] [Minor] The following builtins exist in `standard_builtins()` but have no coverage in `doc/11a-builtins.md`: `force` (WHNF forcing), `eval-ast` (AST evaluation), `gensym` (unique symbol generation), `llt-repr` (value as LLT source), `tag-of` (variant tag extraction), `variant` (variant construction), `record?` and `map?` (dict subtype predicates), `float` (type conversion), `get?` (optional key lookup), `decimal` and `big-int` (extended numeric types), `send-datagram` and `recv-datagram` (UDP/datagram I/O). Add entries for each with arity, signature, and error cases to `doc/11a-builtins.md`.

**`unfold` step return dict — key names are positional, not named:**

- [ ] [Minor] `doc/11a-builtins.md:279` says unfold step returns `[value: v  next: state']`. `doc/11-stdlib.md:510` says `[value state]`. The actual implementation (`src/builtins_seq_gen.rs:385-391`) extracts the **first two values by insertion order** (ignoring keys): `let mut iter = map.values(); let value_id = iter.next(); let next_seed_id = iter.next()`. The step function's return dict keys are irrelevant — only position matters. This is a silent gotcha: `[value: v  next: s]` and `[next: s  value: v]` produce different results (swapped!). Document that the step dict must have the value as the **first** entry and the next state as the **second** entry, regardless of key names. The `doc/11-stdlib.md` entry for `unfold` (line 510) using `[value state]` is the closest to correct but still misleading since it implies the key names matter.

---

### ~~doc-verify-data-model~~: RESOLVED (2026-05-10)

All items resolved via doc updates. URI values documented as Dict-returning builtins. `Value::Null` references replaced with "empty dict". `Decimal` documented as implemented. Value type list corrected. All changes are permanent design decisions (B).

---

### doc-verify-documents: doc/09-documents.md against implementation

Major items RESOLVED (2026-05-10) — doc/09-documents.md updated: `Σ` state definition, cache key type, include rules, and base_dir all corrected. These are permanent architectural decisions (B — code is correct, doc was stale).

**Remaining:**

- [ ] [Minor] Update Implementation Correspondence tables in `doc/09-documents.md` — stale file:line references. `eval_document` is at `src/eval_pipeline.rs:33`; `eval_file_with_input` at `src/eval_pipeline.rs:256`. Part 6 table says `Σ (EvalState) | eval.rs:41-45` — actual is `src/eval.rs:109-144`.

---

### doc-verify-tooling: doc/12-tooling.md and doc/16-architecture.md against implementation

Most items RESOLVED (2026-05-10) — docs updated to match implementation:
- `--no-stdin`: removed from docs (B — `%stdin` is only injected when `-i` is present; no separate flag needed)
- `just ext-package`: merged into single `just ext` command in docs (B)
- `--algo` hash flag: removed from docs; hash algorithms table reduced to BLAKE3 only (B — only BLAKE3 implemented)
- `--allow-network`: removed from docs (B — network allowed automatically with `--cap-net`)
- `RLIMIT_FSIZE`: removed from resource table (B — not implemented, not needed)
- Sandbox init order: corrected in docs to match actual sequence (B)
- `EvalConfig` struct in doc/16: updated to match current source (B)

**Remaining (aspirational -- implement, don't fix doc):**

- [ ] [Minor] (aspirational) Add SHA3-256, SHA3-512, and SHA-256 hash verification support to `parse_integrity_hash()` in `src/builtins_meta.rs` — currently only BLAKE3 is supported. Doc has been reduced to BLAKE3-only; if additional algorithms are implemented, update doc/12-tooling.md hash table accordingly.

---

### doc-verify-type-extensions: doc/07-type-extensions.md vs BAS implementation divergence

Partially RESOLVED (2026-05-10) — BAS is the permanent type system (B). Section header and Row struct code block updated. Stale `src/typecheck_annot.rs` comment fixed. BAS supersedes Remy permanently.

**Remaining doc cleanup (all B — update doc to match code):**

- [ ] [Major] Complete archival of Rémy Parts 1-9 content in doc/07-type-extensions.md — header and Row struct updated, but `Substitution` (row_map), `TypeScheme` (row_vars), Access Chain pseudocode (RowVar generation), Row Display (tail-based output), and `unify_rows` description still show stale Rémy content. Update each to document BAS-era behavior or mark as archived.
- [ ] [Minor] Document the `repr:` annotation property in doc/07-type-extensions.md — fully implemented at `src/typecheck_annot.rs:100-127`: accepts `"u8"`, `"i8"`, `"u16"`, `"i16"`, `"u32"`, `"i32"`, `"u64"`, `"i64"` and enforces numeric type. Not mentioned in doc/07 or doc/05.
- [ ] [Minor] (aspirational — implement, don't fix doc) `Dict = Record ∨ Map[K V]` at doc/07:806-814 — `Type::Map` exists but `@Map` annotation is unimplemented. Mark as aspirational or move to `doc/whatif/`.
- [ ] [Minor] (aspirational — implement, don't fix doc) Nominal Result Type at doc/07:794-803 — runtime returns `Value::Variant { tag: "Ok"/"Err" }` but `Type::Variant` does not exist yet. Static typing uses `Unknown`. Add aspirational note.

---

### doc-verify-eval08: doc/08-evaluation.md divergences from implementation

Most items RESOLVED (2026-05-10) — doc/08-evaluation.md updated. The iterative CEK machine is the permanent architecture (B). All depth-related changes are permanent design decisions:
- `[MATERIALIZE-DEPTH]` rule: removed. Depth tracking paragraph replaced with CEK machine description.
- All judgment forms: `d` parameter removed throughout (`materialize(θ)`, `eval(e, ρ, Σ)`, delta rules).
- `PendingBuiltin` `pd` field: removed from rules and prose. Depth semantics rationale paragraph removed.
- `[MATERIALIZE-GUARD-DEPTH]` and `[MATERIALIZE-GUARD-OUTER-DEPTH]`: consolidated into `[MATERIALIZE-GUARD-NONCACHEABLE]`.
- `Action`/`Cont` pseudocode: updated to match current 3-variant `Action` and 6-variant `Cont`.
- `run()` signature: updated to current `pub(crate) fn run(initial: Action, ctx: &Rc<EvalContext>) -> EvalResult<Value>`.
- Semantic Commitment 3: rewritten for CEK machine (no MAX_EVAL_DEPTH).
- Strictness table intro: "all 59 builtins" corrected to "core evaluation and collection builtins".
- Backward edge description: updated to reference builtins-only origin for DepthExceeded.
- `Cont` size budget note: now references compile-time assertion correctly.

**Remaining:**

- [ ] [Minor] Document the strict let* semantics of document scope chain in doc/08-evaluation.md — `eval_document` eagerly materializes named bindings (`src/eval_pipeline.rs:108-155`), unlike letrec dict entries which remain lazy. This semantic distinction is undocumented.
- [ ] [Minor] Update `deep_materialize` description in doc/08-evaluation.md to reference `MAX_COLLECT_SIZE` (1,000,000) instead of `MAX_EVAL_DEPTH` (256) for Seq spine guards.
- [ ] [Minor] Update TypeAssert strictness exception in doc/08-evaluation.md — validation is now via `Cont::TypeAssertCheck` continuation, not immediate blocking materialization.
- [ ] [Minor] Update the planned-but-unimplemented Cont variants table (lines ~1281-1292) — mark `CallForceFunc`, `DocumentScope`, `DictBuildKey`, etc. as future work (aspirational — implement, don't fix doc).
