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

2. **Remove `MAX_EVAL_DEPTH` entirely**: the depth limit is an LLT-level design choice, not a Rust
   stack safety requirement (CEK machine is heap-based). Resource bounding is handled by
   `--max-memory` and `--timeout`. Informative errors for infinite recursion are covered by cycle
   detection (InProgress blackholing). The `depth` parameter may be retained for error span
   context but the depth CHECK is removed from `eval()`, `materialize()`, and `deep_materialize()`.
   Also resolves `depth-limit-toml` entirely. The `depth: usize` parameter is removed from all evaluation functions. (`src/eval.rs`, `src/eval_materialize.rs`)

3. **Add `[force expr]` builtin**: thin complement for user-controlled forcing in fn-body Sequential
   where strict bindings may be unwanted. One line of Rust + `Strictness::Seq`. (`src/builtins.rs`,
   `src/builtins_meta.rs`)

4. **Apply existing `materialize` depth-check optimization**: move the depth check inside the
   `Unevaluated`/`PendingBuiltin` arms (not before the state check) so already-materialized thunks
   return at O(1) depth regardless of call depth. Separate but related improvement. (`src/eval.rs`)

**Note**: The CEK machine migration is complete (sprints a, b1-b5, d all done). MAX_EVAL_DEPTH is being removed entirely in the `sequential-strict` sprint — resource bounding is handled by `--max-memory`/`--timeout`; cycle detection handles infinite recursion.

### ~~`depth-limit-toml`: `parse-toml-lite` exceeds depth on large TOML files~~ (RESOLVED)

Resolved by removing `MAX_EVAL_DEPTH` in the `sequential-strict` sprint. The recursive
tinct parser in `stdlib/toml-lite.llt` required ~900 depth levels for a 60-line TOML file;
without a depth limit this is no longer a concern.

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

- [x] Survey comparable languages — Nickel `{_: Type}`, TypeScript index signatures, Haskell `Map k v`; see `doc/whatif/parameterized-dict.md` §References
- [x] Can BAS accommodate `Dict[K V]`? — BAS is only needed for union/intersection *over* map types (Phase 3); annotation and inference are BAS-independent
- [x] Record vs Map split — yes; `Dict: [type [Record Map]]` is the right model; see `doc/whatif/parameterized-dict.md` §Design
- [x] Stdlib functions that benefit — `transitions` and `groups` in regex NFA are the primary cases; `stat`/`tls-peer-cert`/`list-dir` are structural Records, not Maps
- [x] Write proposal — see `doc/whatif/parameterized-dict.md`

## Evaluation

### `sequential-strict`: Make Sequential bindings strict + raise depth limit

Fixes the `sequential-lazy` and partially fixes `depth-limit-toml` Known Bugs.
See Known Bugs section for root cause analysis and panel review findings.

- [ ] Remove `MAX_EVAL_DEPTH` constant, all three depth checks, and the `depth: usize` parameter from `eval()`, `materialize()`, and `deep_materialize()` — update all call sites; remove `EvalError::depth_exceeded` error path; resource bounding via `--max-memory`/`--timeout`; cycle detection (InProgress blackholing) handles self-referential thunks (`src/eval.rs`, `src/eval_materialize.rs`)
- [ ] In `Expr::Sequential` loop (`src/eval.rs:667-673`): after extracting `(Key::String(name), val_thunk_id)`, call `materialize(ctx.get_thunk(val_thunk_id), Some(&seq_expr.span), ctx, depth + 1)?` and insert `Rc::new(Thunk::new_materialized(forced_value, seq_expr.span))` into `child_env` instead of the unevaluated thunk; apply only to `Key::String` entries (named bindings); integer-keyed entries remain lazy (`src/eval.rs`)
- [ ] Same change in `eval_document` (`src/eval_pipeline.rs:149`): force string-keyed binding values eagerly at document-level Sequential step time (`src/eval_pipeline.rs`)
- [ ] Move depth check in `materialize` inside the `Unevaluated`/`PendingBuiltin` match arms so already-`Materialized` thunks return at O(1) depth without a depth check (`src/eval.rs`)
- [ ] Add `force` Rust builtin: single-arg, `Strictness::Seq`, calls `materialize` on argument and returns `Thunk::new_materialized`; gives users explicit control in fn-body Sequential (`src/builtins_meta.rs`, `src/builtins.rs`)
- [ ] Update `doc/09-documents.md` §[SEQ-SCOPE] (line 292): change "values remain lazy" to document strict-binding semantics; note WHNF-only (not deep), dead-but-erroring bindings now fail eagerly (`doc/09-documents.md`)
- [ ] Corpus tests: verify binding that previously errored lazily (unused) now errors eagerly; verify heavy computation forced at step depth not demand depth; verify `[force expr]` forces its argument (`tests/corpus/eval/`)
- [ ] Remove stale `TODO(iterative-eval)` comments left in `src/eval.rs` after the completed CEK migration (lines 698, 764, 1363, 8870) — the migration is fully done (sprints a, b1-b5, d all in DONE.md); these comments are dead documentation debt (`src/eval.rs`)

## Codebase Health
