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

## Known Bugs (Type Signatures)

### `split-return-type`: `split` typed as `Seq[String]` but returns `Dict`

`type_env.rs` registers `split` with return type `Seq[String]`. At runtime, `builtin_split` builds an integer-keyed `IndexMap` — a `Dict`, not a `Seq`. Every downstream use of `split`'s result with dict operations (`length`, `get`, `builtin-reduce`) produces spurious "cannot unify Seq[String] with [...]" type errors. Confirmed in `samples/versions.llt` errors at lines 18 and 78.

- [ ] Change `split` return type registration in `type_env.rs` from `Type::Seq(Box::new(Type::Str))` to an open record type (`Type::Record(Row { fields: {}, tail: RowTail::RowVar(fresh) })`), matching the actual `Dict` that `builtin_split` produces (`src/type_env.rs`)
- [ ] Corpus test: `[length [split "," "a,b,c"]]` type-checks without error (`tests/corpus/eval/`)

### `length-narrow-type`: `length` typed as Dict-only but accepts String and Bytes

`type_env.rs` registers `length` with parameter type `[...]` (open record — Dict only). At runtime, `builtin_length` dispatches on `Value::String` and `Value::Bytes` in addition to `Value::Dict`. Passing a String to `length` produces spurious "cannot unify String with [...]" type errors. Confirmed in `samples/versions.llt` errors at lines 60 and 69.

- [ ] Change `length` parameter type in `type_env.rs` to `Type::Unknown`, matching the dual-dispatch behavior — same strategy used for other polymorphic builtins (`src/type_env.rs`)
- [ ] Corpus test: `[length "hello"]` and `[length [str-bytes "hi"]]` type-check without error (`tests/corpus/eval/`)

## Macros

### `tmpl-macro`: Migrate `i"..."` string interpolation from parser to `[defmacro tmpl]`

`desugar_interpolated_string()` in `src/parser.rs` converts `i"Hello $name"` tokens directly to `[str "Hello " name]` at parse time. The `[defmacro]` system is now complete — this logic belongs in `stdlib/macros.llt` as `[defmacro tmpl]`, making it corpus-testable and modifiable without recompiling tinct. See `doc/whatif/completed/macro-rewrite.md` for the design.

- [ ] Change parser to emit `[tmpl "Hello $name"]` call node instead of expanding `InterpolatedString` inline; the raw template string is passed as an opaque argument (`src/parser.rs`, `src/lexer.rs`)
- [ ] Implement `[defmacro tmpl [template] ...]` in `stdlib/macros.llt`: parse the template string char-by-char splitting on `$`, produce `[str segment1 var1 segment2 ...]` (`stdlib/macros.llt`)
- [ ] Remove `desugar_interpolated_string()` from `src/parser.rs` (`src/parser.rs`)
- [ ] Corpus tests: `i"Hello $name"` still expands correctly; nested expressions `i"val: $[+ x 1]"` work; empty interpolation `i"plain"` works (`tests/corpus/eval/`)

## Codebase Health
