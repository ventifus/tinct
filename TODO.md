# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Known Bugs

### ~~`iife-parse`: `[[fn ...] args]` parsed as Dict, not Call~~ (BY DESIGN)

`[[fn [x] body] arg]` is parsed as a 2-element data array, not a function call.
This is intentional: a bracket expression in head position produces data (Priority 7 fallback),
preserving the `[[condition-result] value]` pair pattern used by `cond` and similar constructs
(30+ occurrences in stdlib). Changing the fallback to "call" would break all of these.

IIFEs are largely unnecessary in tinct: fn-body Sequential and document-level Sequential
both provide local bindings natively. For rare inline cases, use `[call [fn [x] body] arg]`.

Documented in `doc/02-syntax.md` §3.3.1a (head-position rule) and §3.3.1b (IIFE patterns).

### `sequential-lazy`: Sequential fn-body bindings are lazy, not eager

`Expr::Sequential` materializes the outer dict at each step but the binding VALUES
remain as `Unevaluated` thunks in the child env (`eval.rs:671` inserts `ctx.get_thunk(val_thunk_id)` —
no forcing). Pre-computing an expensive value as a Sequential step does NOT make it
cache at a shallow depth — it is forced lazily at whatever depth first demands it.

**Root cause** (`eval_dict.rs:50-79`): non-literal values get `Thunk::new_unevaluated`; `eval_dict`
returns a shallow `Thunk::new_materialized(Value::Dict(dict_map))` where all inner `ThunkId`s
are still unevaluated. Sequential extracts those ThunkIds and inserts them into `child_env` as-is.

**Fix plan** (confirmed by eval-engine + computer-scientist panel, 2026-05-08):

1. **Make Sequential bindings strict (WHNF-only)**: in the Sequential loop, after extracting
   `(Key::String(name), val_thunk_id)`, call `materialize(ctx.get_thunk(val_thunk_id), ...)` and
   insert a pre-materialized thunk instead. Forces at depth ~3-5; subsequent demands return
   the memoized value at O(1) depth. Only semantic break: dead-but-erroring bindings now fail
   eagerly — correct behavior for a config language. WHNF-only (shallow force) preserves laziness
   within bound values. Formally sound: Sequential is `let*` (no mutual recursion), not letrec;
   strict `let*` is well-established (Schmidt-Schauss & Sabel 2015). (`src/eval.rs`)

2. **Raise `MAX_EVAL_DEPTH` from 256 to 1024**: Option A forces at depth ~5; the TOML parser needs
   ~900 levels, so 512 is insufficient. At 1024 and ~400 bytes/frame, Rust stack usage is ~400 KB
   against a 64 MB available stack — zero risk. Also addresses `depth-limit-toml` for most
   practical call depths. (`src/eval.rs:21`)

3. **Add `[force expr]` builtin**: thin complement for user-controlled forcing in fn-body Sequential
   where strict bindings may be unwanted. One line of Rust + `Strictness::Seq`. (`src/builtins.rs`,
   `src/builtins_meta.rs`)

4. **Apply existing `materialize` depth-check optimization**: move the depth check inside the
   `Unevaluated`/`PendingBuiltin` arms (not before the state check) so already-materialized thunks
   return at O(1) depth regardless of call depth. Separate but related improvement. (`src/eval.rs`)

**Note**: The CEK machine migration is complete (sprints a, b1-b5, d all done). MAX_EVAL_DEPTH is the LLT-level depth counter enforced inside the iterative `run()` loop — it is a design choice (informative errors vs cryptic crashes), not a Rust-stack artifact. Raising it is low-risk; eliminating it entirely would require a design decision to drop the user-visible depth limit.

### `depth-limit-toml`: `parse-toml-lite` exceeds depth on large TOML files

The recursive tinct parser in `stdlib/toml-lite.llt` uses ~15 depth levels per
TOML line (via `parse-lines-impl` → `parse-line-dispatch` → `parse-key-value` →
`parse-value-try-int` → `try-or` → `try`). On a Cargo.toml with 60 non-blank
lines, it requires ~900 depth levels.

With `sequential-lazy` fix #2 above (MAX_EVAL_DEPTH=1024), the TOML parser fits when
called from depth ≤124. For deeper call sites or larger TOML files, a permanent fix is
still needed: (b) rewrite using `builtin-reduce` (Rust-level iteration resets depth per
iteration) or (c) add a `parse-toml-lite-iter` builtin that processes line-by-line in Rust.

---

## Research

### `research-parameterized-dict`

Investigate whether tinct's type system should support a parameterized
`Dict[K V]` type constructor — algebraic type constructors with kind
`Type → Type → Type`. Motivated by the need to type `transitions` in
`stdlib/regex.llt` as `Dict[Int Seq@Int]` (char-code → successor state
ids) rather than the current unparameterized `@Dict` with a runtime
invariant comment.

**The gap:** BAS (`doc/whatif/boolean-algebraic-subtyping.md`) encodes
multi-field records as intersections of single-field types and handles
union/intersection over specific named fields — but cannot express "all
values in this dict are of type T" because that requires universal
quantification over field labels (∀f. {f: T}), which is outside BAS's
scope. The `transitions` and `groups` dicts in `NfaState`/`NfaDict`
(lib-regex.md) are the concrete cases that remain untyped.

**Questions for the research phase:**

- [ ] Survey how comparable languages type parameterized maps: Haskell
  `Map k v`, TypeScript `Record<K, V>`, Nickel's contract-based approach,
  CUE's structural constraints (`{[string]: int}`). Which model fits
  tinct's use cases?
- [ ] Can BAS accommodate a `Dict[K V]` constructor as a primitive
  type constructor (not derived from records)? What interaction does
  `Dict[K V]` have with union/intersection (`Dict[Int Str] | Dict[Str Int]`)?
- [ ] Is `Dict[K V]` the right primitive, or should tinct distinguish
  between structural records (field names known statically) and dynamic
  maps (keys are runtime values)? The current `Dict` conflates both.
- [ ] Identify all stdlib functions whose type signatures benefit from
  `Dict[K V]`: `transitions` in regex NFA, `groups` in NFA, the `stat`
  return dict, `tls-peer-cert` result, `list-dir` entry dict.
- [ ] Write a `doc/whatif/parameterized-dict.md` proposal.

**Depends on:** BAS adoption (`doc/whatif/boolean-algebraic-subtyping.md`),
since the interaction between `Dict[K V]` and union/intersection types
requires the full BAS constraint solver to be sound.

## Evaluation

### `sequential-strict`: Make Sequential bindings strict + raise depth limit

Fixes the `sequential-lazy` and partially fixes `depth-limit-toml` Known Bugs.
See Known Bugs section for root cause analysis and panel review findings.

- [ ] Raise `MAX_EVAL_DEPTH` from 256 to 1024 (`src/eval.rs:21`) — zero risk; ~400KB Rust stack at 1024 depth vs 64MB available
- [ ] In `Expr::Sequential` loop (`src/eval.rs:667-673`): after extracting `(Key::String(name), val_thunk_id)`, call `materialize(ctx.get_thunk(val_thunk_id), Some(&seq_expr.span), ctx, depth + 1)?` and insert `Rc::new(Thunk::new_materialized(forced_value, seq_expr.span))` into `child_env` instead of the unevaluated thunk; apply only to `Key::String` entries (named bindings); integer-keyed entries remain lazy (`src/eval.rs`)
- [ ] Same change in `eval_document` (`src/eval_pipeline.rs:149`): force string-keyed binding values eagerly at document-level Sequential step time (`src/eval_pipeline.rs`)
- [ ] Move depth check in `materialize` inside the `Unevaluated`/`PendingBuiltin` match arms so already-`Materialized` thunks return at O(1) depth without a depth check (`src/eval.rs`)
- [ ] Add `force` Rust builtin: single-arg, `Strictness::Seq`, calls `materialize` on argument and returns `Thunk::new_materialized`; gives users explicit control in fn-body Sequential (`src/builtins_meta.rs`, `src/builtins.rs`)
- [ ] Update `doc/09-documents.md` §[SEQ-SCOPE] (line 292): change "values remain lazy" to document strict-binding semantics; note WHNF-only (not deep), dead-but-erroring bindings now fail eagerly (`doc/09-documents.md`)
- [ ] Corpus tests: verify binding that previously errored lazily (unused) now errors eagerly; verify heavy computation forced at step depth not demand depth; verify `[force expr]` forces its argument (`tests/corpus/eval/`)
- [ ] Remove stale `TODO(iterative-eval)` comments left in `src/eval.rs` after the completed CEK migration (lines 698, 764, 1363, 8870) — the migration is fully done (sprints a, b1-b5, d all in DONE.md); these comments are dead documentation debt (`src/eval.rs`)
