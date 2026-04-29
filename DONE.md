# Completed Work

Completed milestones and sprints, moved from TODO.md.

## Parser — Complete

- [x] pest PEG grammar (`src/grammar.pest`)
- [x] AST with source spans (`src/ast.rs`)
- [x] pest-to-AST parser (`src/parser.rs`)
- [x] Formal specification (`SPEC.md`)
- [x] Corpus-based testing infrastructure
- [x] Library/binary separation
- [x] `Fn@Return [Params]` function types, generalized `@` annotations
- [x] File/Document/Expression hierarchy with `---` separator

### Parser static constraints (SPEC section 5)

- [x] Positional entries must precede named entries (SPEC 5.1)
- [x] Duplicate key detection (SPEC 5.3)
- [x] Multiple variadics rejected (SPEC 5.4)
- [x] Variadic must be last param (SPEC 5.4)

### Performance

- [x] Pre-computed line-offset table for O(log n) `offset_to_position` (was O(n) per call)

## Evaluator — End-to-End Pipeline

The smallest slice that produces a working `parse -> eval -> output` pipeline. Lazy from the start — every value is a thunk.

**Deliverable:** `tinct eval input.llt` outputs JSON. End-to-end pipeline.

### eval-foundation: Value, Thunk, Environment

Foundation types. No evaluation logic yet.

- [x] `Value` enum: Int, Float, String, Bool, Dict, Function, Builtin
- [x] `Key` enum: Int(i64), String(String)
- [x] `Thunk` struct with `RefCell<ThunkState>` (Unevaluated / InProgress / Materialized)
- [x] Source location on thunks for error reporting (definition-site)
- [x] `Environment` with `parent` chain (lexical scoping)
- [x] `EvalError` type with source span (definition-site + materialization-site)
- [x] `Dict` uses insertion-ordered map (`IndexMap` or similar) with `Key` keys and `Rc<Thunk>` values

### eval-core: Core Eval

Evaluate literals, variable references, and dict construction. After this step, `[x: 1 y: hello]` produces a dict.

- [x] `eval(ast, env) -> Result<Rc<Thunk>, Box<EvalError>>` — wraps AST nodes in thunks
- [x] `materialize(thunk) -> Result<Value, Box<EvalError>>` — forces a thunk, memoizes result
- [x] Literal evaluation: Int, Float, Bool, Str -> immediate `Materialized` thunks
- [x] VarRef lookup: walk the environment parent chain
- [x] Dict evaluation: create new `Environment` from entries, all values are thunks sharing it (letrec)
- [x] Auto-indexing: unkeyed entries get integer keys 0, 1, 2, ...
- [x] Keyed entries: evaluate key expression, insert with explicit key
- [x] Cycle detection: `InProgress` state triggers circular dependency error on re-entry

### eval-access: Access Chains — Complete

Depends on eval-core. After this step, `$data.name` and `$data[0]` work.

- [x] DotAccess: materialize expr, look up string key in dict
- [x] BracketAccess: materialize expr, evaluate key, look up in dict
- [x] RangeAccess: materialize expr, filter dict entries by key range
- [x] TypeAssert: evaluate as identity (type checker enforces in types-core)
- [x] Annotated: evaluate as the bare string (type checker interprets in types-core)

### eval-documents: Document Evaluation — Complete

Depends on eval-core and eval-access. After this step, multi-expression scope chains and `$$` pipeline work.

- [x] Multi-expression documents: each expression's result dict becomes parent scope for the next
- [x] Multi-document files: `---` resets scope, previous document's output becomes `$$`
- [x] `$$` starts as `[]` (empty dict) for the first document
- [x] `$$` passes lazily between documents (no materialization at `---` boundary)

### eval-functions: Functions — Complete

Depends on eval-core. After this step, `[fn [x] $x]` and `[call $f $x]` work.

- [x] `fn` evaluation: capture params + body + current env as `Value::Function`
- [x] `call` evaluation: materialize function, bind args to params in new env, wrap body as thunk
- [x] `$_` implicit lambda: evaluator wraps `[...]` containing VarRef("_") in `[fn [_] [...]]`
- [x] Named argument binding: match named args to params with `default:` annotations
- [x] Arity checking: wrong argument count is an error
- [x] Variadic params: collect remaining positional args into a dict with integer keys

## Type System

Separate AST pass that runs between parsing and evaluation (see DESIGN.md pipeline).

### types-core: Core Types & Inference

Depends on eval-functions (full evaluation model must be stable). After this step, the type checker infers types for all data forms and validates annotations.

- [x] Type representation: Type enum (Int, Float, Str, Bool, Number, Record, Function, TypeVar, Any)
- [x] Type environment: maps names to types, Rc-based scope chain mirroring evaluation's Environment
- [x] Literal type inference (Int, Float, Bool, String)
- [x] Record type construction from dict entries (three-pass letrec: bind all to Any, register aliases, infer values)
- [x] Access chain type checking: dot access verifies field exists, bracket access checks key type, range access validates bounds
- [x] Document-level type inference: scope chain across expressions within a document, `$$` typing across documents
- [x] TypeAssert enforcement: `[@Type $expr]` validates subtype at compile time
- [x] Annotated node interpretation: `x@Number` in type context, `Fn@Return` for function types
- [x] Type alias expansion: `[type ...]` registers alias in TypeEnv, excluded from record fields
- [x] Type error reporting with source spans (TypeError with message + Span)

### types-polymorphism: Polymorphism & Function Types

Depends on types-core. After this step, polymorphic functions and open records work.

- [x] Function type inference from params + body + annotations
- [x] `Fn@Return [Params]` type interpretation
- [x] Type variable introduction (lowercase names: `a`, `b`, `k`, `v`)
- [x] Type variable unification (Hindley-Milner style)
- [x] Row polymorphism: open records with `...`, named row variables (`...rest`)
- [x] Polymorphic function type checking (e.g., `map: Fn@[b] [Fn@b [a]  [a]]`)
- [x] `Any` as escape hatch with `[@Type $expr]` as the way back to concrete types

## Runtime Pipeline

Build the end-to-end runtime. Builtins get proper type signatures from the Type System milestone.

### builtins-core: Rust-Native Builtins

Depends on eval-functions + types-polymorphism. The 28 true primitives that MUST be Rust: operations Tinct cannot express. Everything else is derived in Tinct in `stdlib/prelude.llt`.

- [x] Builtin registration: populate root environment with `Value::Builtin` entries
- [x] Arithmetic: `+`, `-`, `*`, `/` with auto-promotion (Int+Int=Int, mixed=Float)
- [x] Comparison: `<`, `=`
- [x] Control: `if` (selective materialization: only chosen branch evaluated)
- [x] Dict primitives: `keys`, `length`, `merge` (right-biased), `append`
- [x] String: `str` (concat/toString), `split`, `replace`, `upper`, `lower`, `trim`
- [x] Numeric: `floor`, `round` (Rust's f64::round, half-away-from-zero)
- [x] Parsing: `to-int`, `to-float` (string-to-number only; numeric conversion is Tinct)
- [x] Evaluation control: `eval`, `error`, `try`, `apply`
- [x] Type introspection: `type-of`
- [x] I/O: `from-json`

### stdlib-loading: Tinct Stdlib Loading

Depends on builtins-core. Load `stdlib/prelude.llt` to provide the rest of the stdlib.

- [x] `include_str!("../stdlib/prelude.llt")` to bundle at compile time
- [x] Parse and evaluate prelude with Rust builtins as parent environment
- [x] User code inherits from stdlib environment
- [x] Verify all prelude functions work end-to-end

### cli-json: CLI + JSON Output

Depends on builtins-core. After this step, `tinct eval input.llt` produces JSON.

- [x] JSON serialization of Value (`value_to_json` in `lib.rs`, `serde_json`)
- [x] `tinct eval input.llt` — evaluate file, serialize final value as JSON to stdout (clap CLI)
- [x] Stdin input: parse stdin as JSON, inject as `$$` for the first document (`eval_file_with_input`)
- [x] `--format` flag: output as JSON (default) or Tinct (YAML deferred — would require `serde_yaml` dependency)
- [x] `--eval` flag: deep-force all thunks before serializing (surface errors before partial output)

### include: `$include`

Depends on eval-core. Complex enough to be its own step: file I/O, cycle detection, scope merging.

- [x] Evaluate a file, return its dict
- [x] Namespaced usage: `utils: [call $include "utils.llt"]`
- [x] Merged usage: include result becomes parent scope
- [x] Cycle detection: error on circular includes
- [x] Path resolution relative to including file

### error-polish: Error Reporting Polish

Ongoing throughout earlier phases, but final polish here.

- [x] Call stack reconstruction: chain of materialization sites
- [x] Clear messages: "key not found", "type mismatch", "arity mismatch", "circular dependency"
- [x] Source spans on all errors (definition-site + materialization-site)
- [x] TypeAssert `default:` fallback support
- [x] Thread call-site spans through BuiltinFn signature (resolve Span::origin sentinel in builtin errors)

## Stdlib Boundary Analysis — Complete

- [x] Identify the minimal set of builtins that MUST be implemented in Rust (28 total: arithmetic, comparison, if, keys/length/merge/append, string ops, numeric conversion, eval/error/try/apply, type-of, from-json, include)
- [x] Identify which stdlib functions CAN be implemented as Tinct code (all control flow, collection ops, composition, list ops, sorting, sequences, assertions — implemented in `stdlib/prelude.llt`)
- [x] Document the boundary in DESIGN.md with rationale for each Rust-native builtin (see "Rust-Native vs Tinct-Implemented Boundary" section)
- [x] Design the stdlib loading mechanism (`include_str!` prelude, Rust builtins → Tinct stdlib → user code environment chain)
- [x] Update task list to reflect the split: Rust-native builtins vs Tinct stdlib (builtins-core = Rust builtins, stdlib = Tinct already in prelude.llt)

## Stdlib Validation & Expansion

The Tinct stdlib is implemented in `stdlib/prelude.llt` (already working; 79 corpus test files cover stdlib functions). This milestone validates and expands it. Rust-native builtins (strings, numeric conversion) were registered in builtins-core. Tinct-implemented functions (`and`, `or`, `map`, `filter`, etc.) are already in the prelude.

### Validate prelude functions

- [x] Run prelude end-to-end with evaluator and fix any runtime bugs
- [x] Test each Tinct stdlib function against expected behavior (79 corpus tests covering all public stdlib functions)
- [x] Performance check: identify any functions that need Rust reimplementation for practical use (see below)
- [x] Unify `value_to_display_string` and `value_to_json` via shared visitor pattern (analyzed: kept separate — divergent leaf rendering, error handling, and dict assembly means a visitor adds more code than it removes)
- [x] Clear thread-local `INCLUDE_CTX` after evaluation for library API safety (`clear_include_context()`)

Remaining stdlib functions stay in Tinct prelude: logic (`and`, `or`), control flow (`cond`, `when`, `unless`), dict utilities (`get`, `get-or`, `get-in`, `has?`, `values`, `entries`, `empty?`, `set`, `remove`, `update`), list ops (`first`, `nth`, `last`, `reindex`), collection ops (`map-entries`, `fold`, `slice`, `find-deep`), composition (`compose`, `->`), error handling (`try-or`), assertions (`assert`), identity (`identity`).

## types-correctness: Critical Correctness Fixes

Critical correctness bugs found by 9-agent review (2026-04-19). Fix before types-major-fixes.

- [x] Fix literal unification accepting different values — IntLiteral/StringLiteral comparison in `unify()` (`src/types.rs:261-282`). Test: `test_unify_int_literal_different_values`. [Fixed in 20491ff]
- [x] Fix monomorphic function arity check bypass — arity check moved before `has_type_vars()` early return (`src/typecheck.rs:410-422`). Test: `test_call_monomorphic_arity_mismatch`. [Fixed in 20491ff]
- [x] Enforce annotation bracket restriction — `build_annotation_value` rejects non-dict-entries content (`src/parser.rs:602-608`). Tests: `test_annotation_bracket_special_form_rejected`, `test_type_assert_special_form_rejected`. [Fixed in 20491ff]
- [x] CRLF line ending support — already implemented: `grammar.pest` WHITESPACE rule includes `\r`, `parser.rs` LineTable handles `\r\n` (line 152), tests at lines 2885/2922/2934. [Verified 2026-04-19]

## types-major-fixes: Major Type System Fixes

Major type system bugs from 9-agent review (2026-04-19). Fix before stdlib-fixes.

- [x] Fix TypeAssert `default:` raising spurious type errors — already fixed: `resolve_type_assert` checks `has_default` at `typecheck.rs:516`. Test: `test_type_assert_default_suppresses_mismatch`. [Fixed in 20491ff]
- [x] Fix type alias expansion cycle detection — not a bug: aliases resolve against parent env, preventing cycles. Regression test: `test_type_alias_cycle_errors_not_loops`. [Verified 2026-04-19]
- [x] Fix closed record subtyping field check — already correct: bidirectional check at `types.rs:51-55` + `60`. Regression tests: `test_subtype_closed_record_extra_field_rejected`, `test_subtype_closed_record_same_fields_ok`. [Verified 2026-04-19]
- [x] Clarify annotation PropertyDict rest entry semantics — rest entries alongside `type:` key now rejected at parse time (`parser.rs:597-612`). Tests: `test_annotation_bracket_rest_entry_with_type_key_rejected`, `test_annotation_bracket_rest_entry_without_type_key_allowed`. SPEC.md §5.6 updated. [Fixed 2026-04-19]

## types-eval-nits: Minor Type & Eval Fixes

Minor and nit fixes from 9-agent review (2026-04-19).

- [x] Add `Eq` derive to `RowRest` — added to `src/types.rs:13` [Fixed 2026-04-19]
- [x] Verify type assertion default uses correct scope — verified correct: default evaluates in outer scope (same `env`). Added regression test `test_type_assert_default_accesses_outer_scope`. [Verified 2026-04-19]
- [x] Fix `func_label` incorrectly stripping "call " prefix — not a bug: no `strip_prefix("call ")` exists in codebase. `func_label` only prepends "call " and it's used as-is for stack frames. [Verified 2026-04-19]
- [x] Fix `eval` depth check using `>=` instead of `>` — changed to `>` in eval, materialize, and deep_materialize so depth=MAX_EVAL_DEPTH is allowed. Updated test. [Fixed 2026-04-19]

## stdlib-fixes: Stdlib Fixes & Test Coverage

Fix stdlib bugs and add missing test coverage. Identified by stdlib-author review (2026-04-19).

- [x] Fix `get-in` crash on missing intermediate keys — added `has?` check, error on missing key; added `get-in-or` with default fallback [Major, stdlib-author]
- [x] Document recursive sequence generator depth limits (~250 elements max) in prelude docstrings; already documented at prelude.llt lines 491, 498, 505 [Critical, stdlib-author]
- [x] Add tests for empty collection edge cases — map, filter, reduce, join, flatten, zip, sort with empty dict `[]` input (`tests/corpus/eval/stdlib/`) [Major, stdlib-author]
- [x] Add test for compose with multi-step chains (`tests/corpus/eval/stdlib/compose_chain.txt`) [Major, stdlib-author]
- [x] Add negative tests for `assert` (failure case) and `find-deep` (key not found) — already existed: `errors/assert_false.txt`, `errors/find_deep_missing.txt` [Minor, stdlib-author]
- [x] Add test for slice with negative indices or document positional-only (`tests/corpus/eval/stdlib/slice.txt`) — positional-only confirmed [Nit, stdlib-author]

## stdlib-pre-seq: New Stdlib Functions (Pre-Sequences)

New functions implementable in Tinct without Seq support. Identified by stdlib-author review (2026-04-19).

- [x] `const` — returns first argument, ignores second
- [x] `from-entries` — inverse of `$entries`; reconstruct dict from `[key value]` pairs
- [x] `until` — iterate function until predicate holds; functional loop
- [x] `any?` / `all?` — short-circuit predicate tests over collections

## Sequences and Fully Lazy Operations (completed sprints)

The original design calls for everything to be lazy. Several operations are currently eager due to the `BuiltinFn` return type (`Value` instead of `Rc<Thunk>`) and the absence of lazy function application. This milestone adds `Value::Seq` for lazy computation and `PendingCall` thunk state to restore laziness across the language. See DESIGN.md "Sequences and Lazy Computation" for full design.

### laziness-analysis: Laziness Boundary Analysis

Before starting implementation, document the PendingCall/Seq design and analyze current eager operations.

- [x] Analyze all current eager operations and categorize by fix strategy (PendingCall, Seq, inherently eager)
- [x] Document the PendingCall/Seq design decisions in DESIGN.md before starting pending-call
- [x] Document current-vs-planned laziness for `$if`, `$merge`, `$apply` in DESIGN.md

### pending-call: PendingCall Thunk State

Add `PendingCall(func: Rc<Thunk>, args: Vec<Rc<Thunk>>, call_span: Span)` and `Failed(Box<EvalError>)` to `ThunkState`. This enables lazy function application at runtime without AST nodes and error caching.

- [x] Add `PendingCall` variant to `ThunkState` in `value.rs`
- [x] Add `Failed(Box<EvalError>)` variant to `ThunkState` for error memoization (Nix's `nFailed` pattern — cache failures instead of restoring `Unevaluated` and re-evaluating)
- [x] Handle `PendingCall` in `materialize()`: extract func+args, call function, memoize result
- [x] Handle `PendingCall` in `Thunk::take_*` methods for state management
- [x] Handle `PendingCall` in cycle detection (set `InProgress`, restore on error)
- [x] Add `Thunk::new_pending_call(func, args, span)` constructor
- [x] Tests: PendingCall materializes correctly, memoizes, cycle-detects

### eval-quality: Eval/Value Code Quality

Nit-level improvements to eval and value code, deferred to pending-call implementation window.

- [x] Extract `decorate_err` closure to standalone `attach_materialization_context()` function for testability (`src/eval.rs:842-866`, used at lines 871 and 885) [Nit, eval-engine]
- [x] Add `EvalResult<T>` type alias for `Result<T, Box<EvalError>>` — appears 200+ times across eval.rs and builtins.rs [Nit, integration-verifier]
- [x] Rewrite materialize docstring to mention memoization — currently says "force a thunk" without explaining it (`src/eval.rs:795-806`) [Nit, laziness-auditor]
- [x] Add rationale to eval docstring about "immediately materialized thunks" (`src/eval.rs:52-53`) [Nit, laziness-auditor]
- [x] Extend PendingBuiltin args comment to clarify builtin vs function handling (`src/eval.rs:415-437`) [Nit, laziness-auditor]
- [x] Consider simpler `set_state` method for `Thunk::transition` — closure takes `&ThunkState` but most callers ignore it (`src/value.rs:240-265`) [Nit, eval-engine]
- [x] Change `Thunk::origin` from `Option<String>` to `String` — it is always set in practice (`src/value.rs:186`) [Nit, span-integrity-checker]
- [x] Extract `transition_to_failed` helper — 7 identical `thunk.transition(|_| ThunkState::Failed(Box::new((*e).clone())))` blocks in materialize() (`src/eval.rs:888-969`) [Nit, eval-engine + test-crafter]
- [x] Add origin label parameter to `new_pending_call()` — currently sets `origin: None`, stack traces won't show which operation created the deferred call (`src/value.rs:249`) [Minor, eval-engine]
- [x] Add `#[allow(clippy::type_complexity)]` to `take_pending_call` for consistency with `take_pending_builtin` (`src/value.rs:332`) [Nit, test-crafter]
- [x] Update unreachable message in materialize() to explain why Failed/InProgress can't reach it (`src/eval.rs:974`) [Nit, eval-engine]

### builtin-thunk-return: BuiltinFn Signature Change

Change `BuiltinFn` to return `Rc<Thunk>` instead of `Value`. This removes the forced materialization boundary for builtins.

- [x] Change `BuiltinFn` type alias: `-> Result<Value, ...>` to `-> Result<Rc<Thunk>, ...>`
- [x] Update `materialize()` PendingBuiltin handler to use returned thunk
- [x] Update all 28 builtins to wrap return values in `Thunk::new_materialized()`
- [x] Update `$if` to return the chosen branch thunk directly (no materialization)
- [x] Update `$merge` to return lazy overlay (right dict shadows left, no cloning)
- [x] Defer function materialization in `eval_call` — skipped: needs PendingCall named args, marginal benefit
- [x] Tests: verify all builtins still work, $if laziness preserved
- [x] Consider `BuiltinArgs` struct to reduce BuiltinFn signature verbosity (4 parameters) (`src/value.rs:16-17`) [Nit, integration-verifier]

### seq-core: Value::Seq (Core)

Add `Value::Seq(head: Rc<Thunk>, tail: Rc<Thunk>)` for lazy sequences — core type and serialization.

- [x] Add `Seq` variant to `Value` enum
- [x] Add `seq?` type check to `type-of` builtin
- [x] Handle `Seq` in `value_to_json` (error: must $collect first)
- [x] Handle `Seq` in `value_to_display_string` (show `Seq(head, ...)`)
- [x] Handle `Seq` in `deep_materialize` (force head, recurse on tail up to depth limit)
- [x] Add visited set to `deep_materialize` for cycle tracking across mutual dict references (Nix `forceValueDeep` pattern) — also flagged by eval-engine + performance-expert reviews
- [x] Tests: Seq in value_to_json error, display format, deep_materialize depth limit

### seq-builtins-types: Sequence Builtins & Types

Sequence builtins, type system integration, and stdlib fixes for Seq.

- [x] Add `Type::Seq(Box<Type>)` to type system — monomorphic in element type, not a subtype of Record (`types.rs`)
- [x] Sequence builtins (Rust-native): `seq`, `head`, `tail`, `collect`, `seq?`
- [x] Tests: seq construction, head/tail, collect, type-of
- [x] Fix `empty?` to not hang on infinite sequences — currently does not short-circuit (`stdlib/prelude.llt:156`) [Minor, stdlib-author]

### seq-constructors: Sequence Constructors — Complete

Rewrite `range`, `repeat`, `cycle` (currently in `stdlib/prelude.llt`) as Rust builtins returning `Seq` instead of eagerly-built dicts. Add new constructors `iterate` and `unfold`.

- [x] Move `range`, `repeat`, `cycle` from Tinct prelude to Rust builtins
- [x] `range` returns Seq; 1-arg form `[call $range start]` is infinite, 2-arg form finite
- [x] `repeat` returns infinite Seq; 1-arg form `[call $repeat val]` only
- [x] `cycle` returns infinite Seq; 1-arg form `[call $cycle xs]` only
- [x] `iterate`: `[call $iterate $f $x]` -> x, f(x), f(f(x)), ...
- [x] `unfold`: `[call $unfold $step $seed]` -> step returns `[value state]` or `[]`
- [x] Move `take` from Tinct prelude to Rust builtin; dual-dispatch Dict (preserve keys) + Seq (return finite Seq)
- [x] Remove old `range`, `repeat`, `cycle`, `take` (and helpers) from prelude.llt
- [x] Tests: finite/infinite range, repeat, cycle, iterate, unfold, take on Seq

### span-failed-fix: Critical Span Fix

Fix Failed state materialization_span overwrite found by span-integrity-checker review (2026-04-20). When re-materializing a Failed thunk, the original materialization_span is overwritten instead of preserved.

- [x] Fix Failed state unconditional materialization_span overwrite — preserve original span, add subsequent access sites as stack frames instead of overwriting (`src/eval.rs:882-887`) [Critical, span-integrity-checker]
- [x] Update `test_failed_state_updates_materialization_span` to verify correct behavior: first access sets mat_span, subsequent accesses push frames instead of overwriting (`src/eval.rs`) [Critical, span-integrity-checker]

### dual-dispatch-map: Dual-Dispatch Map/Filter

Move `$map` and `$filter` to Rust builtins with dual-dispatch on Dict vs Seq. Note: `$take` dual-dispatch already implemented in seq-constructors.

- [x] `$map` on dict: return dict with PendingCall thunks (lazy, same keys)
- [x] `$map` on seq: return lazy seq
- [x] `$filter` on dict: return seq (must evaluate predicates)
- [x] `$filter` on seq: return lazy seq
- [x] Move `map`, `filter` from Tinct prelude to Rust builtins
- [x] Tests: map/filter on dicts (lazy verification), map/filter on seqs, mixed pipelines

### drop-reduce: Drop/Reduce + Typing Strategy

Additional sequence operations and typing decisions for dual-dispatch ops.

- [x] `$drop` on seq: return seq skipping first n elements
- [x] `$reduce` on seq: accumulate, materializing each step
- [x] Move `drop`, `reduce` from Tinct prelude to Rust builtins
- [x] Decide typing strategy for dual-dispatch ops (`$map`/`$filter` on Record vs Seq): `Any` escape hatch, union types, or separate functions [Major, type-theorist]
- [x] Document `join` O(n^2) due to repeated str concatenation; optimize in Rust builtin (`stdlib/prelude.llt:88-97`) [Minor, stdlib-author]
- [x] Tests: drop on seq, reduce on seq and dict

### include-cache: Include Caching

Cache `$include` results so re-including the same file returns the cached thunk instead of re-evaluating. Jsonnet caches import thunks; Tinct currently re-evaluates every `$include` call.

- [x] Add `HashMap<PathBuf, Rc<Thunk>>` to `IncludeContext` for result caching
- [x] Return cached thunk on re-include of the same resolved path
- [x] Tests: same file included twice returns identical thunk, cache respects path normalization

### lazy-remaining-ops: Make Remaining Eager Ops Lazy

Make remaining eager operations lazy where possible. Seq-aware dual-dispatch for collection ops.

- [x] `$map` on dict -- returns eager dict (fix: PendingCall thunks, dual-dispatch-map)
- [x] `$filter` on dict -- returns eager dict (fix: return Seq, dual-dispatch-map)
- [x] `$range` -- builds full dict eagerly, O(n^2) (fix: return Seq, seq-constructors)
- [x] `$repeat` -- builds full dict eagerly (fix: return Seq, seq-constructors)
- [x] `$cycle` -- builds full dict eagerly (fix: return Seq, seq-constructors)
- [x] `$if` -- materializes chosen branch (fix: return branch thunk, builtin-thunk-return)
- [x] `$merge` -- clones both dicts (Rc-clones thunks already; full lazy overlay needs dict proxy, deferred)
- [x] `$update` -- already lazy via PendingCall (pending-call) + lazy merge (builtin-thunk-return); verified with laziness test
- [x] `$concat` -- Seq path: lazy chain via recursive PendingCall; Dict path: stays eager (existing behavior)
- [x] `$apply` -- double-forces by materializing `invoke_function()`'s result thunk (fix: return thunk directly, builtin-thunk-return)

### seq-collection: Seq-Aware Collection Builtins

Add Seq support to collection builtins that currently only work on Dict.

- [x] `$drop` Seq path: replace eager materialization loop with lazy step function — `builtin_drop_seq_step` using PendingBuiltin pattern
- [x] `$cons` -- Seq path: O(1) prepend via `$seq`; Dict path stays eager (cons-impl)
- [x] `$rest` -- Seq path: O(1) tail via `$tail`; Dict path stays eager (rest-impl)
- [x] `$zip` -- Seq+Seq path: lazy `zip-seq` producing Seq of pairs; mixed/Dict path: collect then eager `zip-dict`

### doc-eagerness-eval: Document Inherent Eagerness — Core Eval

Document why these core eval/dict operations must materialize. Add comments at each call site.

- [x] `eval_key` materializes all dict keys — necessary for IndexMap insertion; comment added
- [x] `eval_as_dict` materializes target for all access chains — necessary for key lookup; comment added
- [x] `builtin_keys` materializes dict — keys are always evaluated; comment added
- [x] `builtin_length` materializes dict — must count entries; comment added
- [x] `$reduce`, `$fold` -- accumulator pattern requires sequential materialization; comment added
- [x] `$sort`, `$sort-by` -- must compare values to determine order; comment added
- [x] `$reverse` -- must know all entries to reverse; comment added
- [x] `$reindex` -- must traverse all entries to rebuild with dense 0..n keys; comment added

### doc-eagerness-collection: Document Inherent Eagerness — Collection & Comparison

Document why these collection/comparison operations must materialize.

- [x] `$flatten` -- must inspect values to check if they are lists (inherently materializing)
- [x] `$length` -- must know all entries (on seqs: must traverse entirely)
- [x] `$empty?` -- depends on `$length`, inherently materializing
- [x] `$get-in`, `$get-in-or` -- must traverse nested dict path, materializing each step
- [x] `$=`, `$<` and comparisons -- must inspect values
- [x] `$+`, `$-`, `$*`, `$/` and arithmetic -- must compute
- [x] `$quot`, `$mod`, `$ceil`, `$trunc` -- derived arithmetic, inherently materializing
- [x] `$find-deep` -- must traverse structure

### doc-eagerness-string: Document Inherent Eagerness — String & Conversion

Document why these string/conversion operations must materialize.

- [x] `$str`, `$split`, `$replace`, `$upper`, `$lower`, `$trim` -- must inspect string content
- [x] `$join`, `$words` -- derived string ops, inherently materializing
- [x] `$to-int`, `$to-float`, `$floor`, `$round` -- must convert
- [x] `$type-of` -- must inspect value variant
- [x] `$from-json` -- must parse entire JSON string

### doc-eagerness-control: Document Inherent Eagerness — Control Flow

Document why these control flow operations must materialize.

- [x] `$eval` -- deep-forces all thunks by definition
- [x] `$error` -- constructs error value (structural, but the error itself is concrete)
- [x] `$try`, `$try-or` -- must materialize body to catch errors
- [x] `$assert` -- must materialize condition to check
- [x] `$any?`, `$all?` -- short-circuit but materializes elements until condition met/failed

### eval-correctness-2: Eval Correctness Fixes (Cycle 1 Review)

Correctness issues found by eval-engine and computer-scientist reviews (2026-04-20 cycle 1).

- [x] Fix cycle detection leaving thunk in InProgress instead of Failed — after circular dependency error, thunk stays InProgress permanently; should transition to Failed for error caching and consistent subsequent access (`src/eval.rs:897-907`) [Major, eval-engine]
- [x] Add named args support to PendingCall — `ThunkState::PendingCall` only stores positional args (`Vec<Rc<Thunk>>`); named args with defaults lost in lazy function application. Add `named: IndexMap<String, Rc<Thunk>>` field and thread through `materialize()` (`src/value.rs:186-190`, `src/eval.rs:983`) [Minor, eval-engine]
- [x] Add origin parameter to `new_pending_builtin()` — always sets `origin: String::new()`, making builtin calls invisible in stack traces; inconsistent with `new_pending_call` which accepts explicit origin (`src/value.rs:228-246`) [Nit, eval-engine]

### lazy-inventory: Laziness Inventory — Already Lazy (reference)

Already lazy — no work needed. Kept for completeness.

- [x] Dict value construction -- values are `Unevaluated` thunks (letrec semantics)
- [x] Function bodies -- thunks until called
- [x] Builtin calls -- `PendingBuiltin` defers until result needed
- [x] `$get`, `$get-or` -- returns thunk from dict (structural)
- [x] `$keys` -- keys are always evaluated, values untouched
- [x] `$values`, `$entries` -- returns thunks
- [x] `$first` -- returns first value thunk
- [x] `$nth` -- returns nth value thunk
- [x] `$identity` -- returns argument as-is
- [x] `$and`, `$or` -- short-circuit via lazy `$if` args

## Tooling

Execution order: REPL → LSP → tree-sitter. One commit per item.

### repl: REPL (`tinct repl`)

- [x] Add `rustyline` dependency (optional, under `repl` feature)
- [x] Expose eval internals: `eval_document`, `eval`, `Environment`, `Value`, `Thunk` as pub in lib.rs
- [x] Implement REPL session (`src/repl.rs`)
  - Mirror `eval_document()` scope chain semantics: each Dict result creates child env
  - Bracket matching for multi-line input (count `[` vs `]`)
  - Display results via `value_to_display_string()`
  - Handle `$$` (previous result) across expressions
  - Error recovery: print error, continue session
  - Session starts with `create_stdlib_env()` as root
  - Prompts: `llt> ` (first line), `...> ` (continuation)
- [x] Wire CLI subcommand: `Repl` variant in `Commands` enum, worker thread with 64MB stack
- [x] Integration tests (bracket matching, scope chain, `$$` pipeline, error recovery, stdlib)
- [x] Just recipe: `just repl` (with `-it` for interactive TTY)

### lsp: LSP server (`tinct lsp`)

Uses `lsp-server` (sync), not `tower-lsp` (async), because `Rc<RefCell<Environment>>` is not Send/Sync.

- [x] Add `lsp-server` + `lsp-types` dependencies (optional, under `lsp` feature)
- [x] Document store (`src/lsp/document.rs`): HashMap<Url, DocumentState>, re-parse/eval/typecheck on change
- [x] Span conversion (`src/lsp/convert.rs`): Tinct Span (offset, 1-indexed) ↔ LSP Position (0-indexed, UTF-16)
- [x] Analysis + hover (`src/lsp/analysis.rs`)
  - Expose `infer_expr` or add `type_at_position()` helper in typecheck.rs
  - Hover on `$var` shows inferred type, hover on `[call ...]` shows signature
  - Parse errors → Error diagnostics, type errors → Warning diagnostics (advisory)
  - Go-to-definition: stretch goal (requires span tracking in Environment bindings)
- [x] Main loop (`src/lsp/server.rs`): `Connection` + crossbeam message loop
  - Requests: `initialize`, `shutdown`, `textDocument/hover`
  - Notifications: `didOpen`, `didChange`, `didClose`
  - Publish diagnostics on every document change
- [x] CLI wiring: `Lsp` variant in `Commands`, `tinct lsp` on stdio
- [x] Tests (span conversion, hover analysis, diagnostic generation, simulated LSP client)
- [x] Just recipe: `just lsp`

### tree-sitter: tree-sitter grammar

After this step, editors with tree-sitter support get syntax highlighting, code folding, and incremental parsing for `.llt` files.

- [x] Scaffold `tree-sitter-llt/` (package.json, grammar.js skeleton, tree-sitter.json)
- [x] Implement grammar rules (port from grammar.pest / SPEC.md)
  - `token.immediate()` for whitespace-sensitive `.` and `[` access
  - Special form keywords via tree-sitter `word` rule keyword extraction
  - Comments as `comment` node type in extras
  - `---` document separator via external scanner (C, negative lookahead)
- [x] Test corpus (49 tests: literals, dicts, special forms, access, documents, annotations, comments)
- [x] Highlight queries (`queries/highlights.scm`)
- [x] Just recipes: `just ts-generate`, `just ts-test`, `just ts-parse FILE`

## Parser Rewrite (E2) — completed sprints

### lexer-core: Lexer Core (`src/lexer.rs`)

Tokenizer producing a flat token stream. Whitespace-sensitivity for access chains handled here.

- [x] Token enum: OpenBracket, CloseBracket, Colon, Semicolon, Dot, Range, At, Ellipsis, DocSeparator, Newline, Comment(String), Int(i64), Float(f64), BareWord(String), QuotedString(String), VarRef(String), BoolLit(bool)
- [x] Single-pass tokenization with source spans on every token
- [x] Whitespace-sensitive access detection: Dot/OpenBracket immediately after VarRef or CloseBracket (no whitespace) emits access-context tokens
- [x] Comment tokens (`#` to EOL, preserves text for formatter)
- [x] Newline tokens (significant whitespace for blank line detection; consecutive Newlines encode blank lines)
- [x] String escapes (`\"`, `\\`, `\n`, `\t`, `\r`)
- [x] Bare word denylist matching grammar.pest rules
- [x] CRLF line ending support: track line boundaries correctly for `\r\n` (pest and LSP convert.rs both assume `\n` only)

### grammar-comments: Grammar Documentation

- [x] Add inline comments to grammar.pest explaining why access_expr/access_chain are compound-atomic (`grammar.pest:137-148`) [Major, grammar-architect]
- [x] Fix grammar.pest COMMENT rule misleading NEWLINE comment (`grammar.pest:8`) [Nit, grammar-architect]

### formatter: Formatter/Pretty-Printer (`tinct fmt`)

Uses the hand-written lexer's token stream (comment-preserving, unlike pest). See DESIGN.md §Formatter for full design.

- [x] Design formatting rules (single-line threshold, comment attachment, semicolon handling) — see DESIGN.md §Formatter
- [x] Formatting engine (`src/formatter.rs`)
  - Indent nested `[]` by 2 spaces per depth
  - One entry per line (unless bracket expr fits within 80 chars AND has ≤4 entries)
  - Comments: line-affinity attachment (trailing = same line, leading = own line)
  - Semicolons always removed (canonical whitespace-separated style)
  - Consistent spacing: one space after `:`
  - `---` separators get blank lines above and below
  - Collapse multiple blank lines to one
  - Access chains never broken across lines
  - Strip trailing whitespace, ensure trailing newline
- [x] CLI subcommand: `Fmt` variant with `--check`, `--in-place`, `--stdin` (zero config)
- [x] Tests (idempotency, comment preservation, indentation, single-line vs multi-line, edge cases)
- [x] Just recipes: `just fmt-llt FILE`, `just fmt-llt-check FILE`

## theoretical-foundations — completed sprints

### Documentation gaps

- [x] Qualify "Hindley-Milner" claim in DESIGN.md — changed to "bottom-up type inference with annotation-driven polymorphism, inspired by Hindley-Milner" with note about missing let-generalization and principal type guarantee (`DESIGN.md:232`) [Critical, computer-scientist]
- [x] Add formal references section to DESIGN.md — added "Formal References" section citing Damas & Milner (1982), Robinson (1965), Rémy (1994), Launchbury (1993), Danvy & Nielsen (2003), Felleisen & Friedman (1986), Ford (2004) with mappings to tinct subsystems [Major, computer-scientist]
- [x] Document substitution idempotence invariant — added to DESIGN.md type system section: `Substitution::apply()` achieves idempotence via transitive chasing, citing Robinson (1965) (`src/types.rs:126-129`) [Minor, computer-scientist]
- [x] Document alpha-equivalence stance — added to DESIGN.md type system section: variable names are significant, `instantiate()` performs alpha-renaming with fresh names for call-site freshening (`src/types.rs:346-358`) [Minor, computer-scientist]
- [x] Document `$$` environmental typing — added to DESIGN.md `$$` section: context-dependent typing (empty closed record vs `Any`), documented as known limitation with `[@Type $$]` as escape hatch (`src/eval.rs:288-293`, `src/typecheck.rs:71`) [Minor, computer-scientist]
- [x] Qualify purity claim — changed to "pure modulo `$include`, which performs filesystem I/O as a controlled side effect with sandboxing" with Nix/Dhall comparison (`DESIGN.md:1119`) [Minor, computer-scientist]
- [x] Document Record type excludes positional entries — added "Type-theoretic implication" paragraph to Principle 1 explaining that `Record` type tracks only string-keyed fields (`DESIGN.md:44-59`) [Minor, computer-scientist]

## Documentation Divergences — completed sprints

### DESIGN.md vs Code

- [x] **`<` does not work on Bool** — Added Bool support to `builtin_lt` with `false < true` ordering (consistent with Haskell, Python, Rust). Since `>`, `<=`, `>=` derive from `<`, all comparison operators now work on Bool.
- [x] **`--eval` CLI flag undocumented** — Added to DESIGN.md CLI examples section.
- [x] **`-` stdin source undocumented** — Added to DESIGN.md CLI examples section.
- [x] **`type-of` Builtin→Function mapping undocumented** — Documented in DESIGN.md materialization behavior table.
- [x] **JSON null→empty dict mapping undocumented** — Documented in DESIGN.md "No Null" section.
- [x] **Dict equality always false undocumented** — Documented in DESIGN.md comparison operator description.
- [x] **`make-entry` documentation inconsistency** — Renamed "Internal Helpers" to "Utility Functions" with clarification.

### SPEC.md vs Code

- [x] **Row polymorphism marked as "deferred to Phase 2b"** — Updated to document row polymorphism as implemented.
- [x] **Rest entry exemption from positional-before-named** — Added exemption note to SPEC Section 5.1.
- [x] **MAX_PARSE_DEPTH not in SPEC Section 5** — Added as SPEC Section 5.5.
- [x] **Annotation bracket restriction undocumented** — Added as SPEC Section 5.6.
- [x] **Parameter-after-variadic implicit** — Added explicit error case to SPEC Section 5.4.
- [x] **Duplicate VarRef key detection extends SPEC 5.3** — Updated SPEC Section 5.3 to document VarRef key duplicate detection.

### CLAUDE.md (from REVIEW.md)

- [x] **CLAUDE.md references "Phase 6" for hand-written parser, should be Phase 7** — STALE: CLAUDE.md simplified, no longer contains phase references. [Resolved, grammar-architect]

### seq-resource-safety: Sequence Resource Safety

- [x] Add `MAX_COLLECT_SIZE` limit to `builtin_collect` — added 1,000,000 element limit with helpful error suggesting `$take`. [Critical, computer-scientist]
- [x] Fix `builtin_iterate` passing `depth: 0` to PendingBuiltin tail — captured depth from BuiltinArgs, passes `depth + 1`. [Major, computer-scientist]
- [x] Increment depth in sequence combinator PendingBuiltin chains — incremented depth in 11 PendingBuiltin creation sites (range, repeat, cycle, iterate, unfold, map, filter, drop, reduce). [Major, computer-scientist]
- [x] Migrate `concat` Seq path from stdlib to Rust builtin — implemented as PendingBuiltin chain with dual Seq/Dict dispatch. [Major, computer-scientist]

## error-structured: Structured Error Model Implementation

Implement the `ErrorKind` enum and migrate all error construction sites. See DESIGN.md §Structured Error Model.

### error-structured-types: ErrorKind Type Definitions

- [x] Add `ErrorKind` enum with 25 variants and `ArityBound` enum to `src/error.rs`
- [x] Add `ErrorKind::code()` method returning stable error code strings
- [x] Add `ErrorKind::is_cacheable()` method returning `false` for `DepthExceeded`
- [x] Add `Display` impl for `ErrorKind` and `ArityBound` (rustc style)
- [x] Replace `message: String` with `kind: ErrorKind` in `EvalError` struct
- [x] Update `EvalError::Display` to include error code prefix `[E001]`
- [x] Update named constructors to construct `ErrorKind` variants
- [x] Add `EvalError::internal(message, span)` replacing `EvalError::new` (kept as backward-compatible shim)

### error-structured-migrate-a: Priority Semantic Fixes

Fix error classification bugs where the wrong ErrorKind variant is produced.

- [x] Migrate `$error` builtin to use `ErrorKind::UserError` instead of `EvalError::new` (→ Internal E099) — `$error` is the canonical user error source; every user-generated error displays `[E099]` (internal) instead of `[E080]` (user). Add `EvalError::user_error(msg, span)` constructor. (`src/builtins.rs:791`) [Major, computer-scientist]
- [x] Migrate `eval()` depth check to use `EvalError::depth_exceeded()` — currently `EvalError::new()` → Internal (E099), inconsistent with `materialize()` which correctly uses `depth_exceeded()`. Breaks `is_cacheable()` invariant when integrated. (`src/eval.rs:59-64`) [Major, computer-scientist + eval-engine]
- [x] Migrate `deep_materialize_impl()` depth check to use `EvalError::depth_exceeded()` — same issue as `eval()` depth check (`src/eval.rs`) [Minor, eval-engine]
- [x] Generalize `FloatNotFinite` Display message — currently says "cannot be converted to Int" but variant covers all non-finite contexts; change to context-independent message like "{builtin}: result is not finite ({value})" (`src/error.rs:228-230`) [Minor, span-integrity-checker + computer-scientist]
- [x] Migrate `require_dict` and `require_string` helpers to use `EvalError::type_mismatch()` — currently use `EvalError::new(format!(...))` → Internal, losing structured error classification (`src/builtins.rs:165-169,177-181`) [Minor, span-integrity-checker]
- [x] Migrate `reject_named` to use `ErrorKind::NamedArgRejected` variant — currently `EvalError::new(format!(...))` → Internal (`src/builtins.rs:192-194`) [Nit, span-integrity-checker + integration-verifier]
- [x] Migrate `checked_f64_to_i64` to use `ErrorKind::FloatNotFinite` — currently `EvalError::new(format!(...))` → Internal (`src/builtins.rs:108-109`) [Nit, span-integrity-checker]

### error-structured-migrate-b: Bulk Migration

Migrate remaining EvalError::new call sites across eval.rs and builtins.rs.

- [x] Migrate eval.rs remaining `EvalError::new` call sites (~13) to typed `ErrorKind` variants (`src/eval.rs`)
- [x] Migrate builtins.rs remaining `EvalError::new` call sites (~72) to typed `ErrorKind` variants (`src/builtins.rs`)
- [x] Update `builtin_try` to extract `e.kind.to_string()` instead of `e.message` (`src/builtins.rs:864`)
- [x] Update all `err.message` references in unit tests to `err.kind.to_string()` or pattern matching (`src/error.rs`, `src/eval.rs`)
- [x] Add PROP-CYCLE bypass comment to circular dependency error construction — explain why it skips DECORATE closure. (`src/eval.rs:899-908`) [Nit, eval-engine panel]
- [x] Document `.message()` as compatibility shim — clarify that `.kind` field is canonical API, `.message()` is for test migration. New code should match on `.kind`. (`src/error.rs`) [Minor, integration-verifier]

### error-structured-migrate-c: Safety Integration + Spec

Error safety mechanisms and specification updates.

- [x] Add `ErrorKind::is_catchable()` method returning `false` for `DepthExceeded` — `$try` currently catches ALL errors including depth-exceeded, defeating the safety net. Users can circumvent depth limits via `$until` + `$try` wrapping deeply recursive code. GHC makes `StackOverflow` uncatchable; Racket separates `exn:fail:resource`. Have `builtin_try` check `is_catchable()` and re-raise uncatchable errors directly. (`src/builtins.rs:793-871`, `src/error.rs`) [Critical, computer-scientist]
- [x] Integrate `is_cacheable()` into `cache_failure` — currently `Thunk::cache_failure()` unconditionally caches all errors including DepthExceeded. Requires state-restore mechanism: save pre-InProgress thunk state so non-cacheable errors can restore it instead of transitioning to Failed. (`src/value.rs:384-386`) [Major, eval-engine + computer-scientist panel]
- [x] Fix `FloatNotFinite` containing `f64` with `PartialEq` derive — `f64::NAN != f64::NAN` so two FloatNotFinite errors with NaN values compare as not-equal. Will affect Failed thunk cache identity when variant is constructed. (`src/error.rs`) [Minor, computer-scientist panel]
- [x] Update SPEC.md §9 with `ErrorKind` variants, error codes in display format (§9.2), and revised exhaustiveness claim in §9.3
- [x] Update DESIGN.md §Error Semantics field name: `message` → `kind` — spec still references old field name (`DESIGN.md`) [Minor, eval-engine]
- [x] Update DESIGN.md error constructors table to reflect ErrorKind variants — either expand or add "representative" note (`DESIGN.md`) [Minor, eval-engine]

### error-structured-migrate-c2: Formal Rule Drift Fixes

DESIGN.md inference rules don't reflect is_cacheable()/is_catchable() guards added in migrate-c.

- [x] Update PROP-EVAL/PROP-BUILTIN/PROP-RESULT inference rules to add `is_cacheable()` precondition on Failed transition — rules show unconditional `thunk.state <- Failed(e')` but implementation conditionally restores pre-error state for non-cacheable errors. Add alternative conclusion showing state restoration when `!is_cacheable()`. (`DESIGN.md:4584-4617`) [Critical, computer-scientist]
- [x] Add TRY-UNCATCHABLE rule and is_catchable() precondition to TRY-ERR — rule unconditionally converts Err(e) to err dict, but builtin_try now re-raises uncatchable errors (DepthExceeded). Add precondition `e.kind.is_catchable()` to TRY-ERR and new TRY-UNCATCHABLE rule showing re-raise. (`DESIGN.md:4690-4699`) [Critical, computer-scientist]
- [x] Update MEMO-CACHE rule to add `is_cacheable()` precondition — rule and prose say "All error paths cache via cache_failure" unconditionally. Add precondition and MEMO-SKIP rule for non-cacheable errors. (`DESIGN.md:4648-4655`) [Critical, computer-scientist]
- [x] Update Implementation Correspondence table line numbers after is_cacheable integration shifted eval.rs structure (`DESIGN.md:4745-4759`) [Minor, computer-scientist]
- [x] Fix E2 property `e.message` to `e.kind` — field was replaced by ErrorKind in error-structured-types (`DESIGN.md:4725`) [Minor, computer-scientist]
- [x] Add note to SPEC.md §9.4 that DepthExceeded errors are not catchable by $try — users may be surprised when $try doesn't catch resource limit errors (`SPEC.md:1436-1449`) [Minor, computer-scientist]

### error-structured-migrate-c3: Residual Doc Precision

- [x] Update TRY-BUILTIN error parenthetical with is_catchable() qualifier (`DESIGN.md:4745`) [Minor, computer-scientist]
- [x] Fix Error-to-value correspondence to match actual code path — table updated to show `e.message()` delegation (`DESIGN.md:4794`) [Minor, computer-scientist]
- [x] Update PROP-DEPTH constructor notation to typed ErrorKind style — `new()` → `depth_exceeded()` (`DESIGN.md:4647`) [Nit, computer-scientist]

### error-structured-migrate-d: Test Coverage

- [x] Add missing ErrorKind constructor methods for remaining ~13 variants — DuplicateKey, NamedArgConflict, UnknownNamedArg, ParseConversion, TypeAssertFailed, UndefinedVariable, all JSON/Include variants. Add constructors and migrate call sites. (`src/error.rs`, `src/eval.rs`, `src/builtins.rs`) [Minor, computer-scientist + eval-engine]
- [x] Add ErrorKind Display unit tests for all 25 variants — covers all 26 ErrorKind variants with exact string assertions. (`src/error.rs`) [Major, test-crafter panel]
- [x] Add ArityBound Display unit tests — Exact/AtMost/Range Display impls. (`src/error.rs`) [Minor, test-crafter panel]
- [x] Add is_cacheable() unit tests — verify DepthExceeded returns false, all others true. (`src/error.rs`) [Minor, test-crafter panel]
- [x] Add error code prefix verification to corpus error tests — new test verifies `[E0XX]` prefix in all error corpus outputs. (`tests/corpus/eval/errors/`) [Minor, test-crafter panel]
- [x] Add `ErrorKind::code()` exhaustiveness unit test — asserts all variants return "E" + digits, all codes unique. (`src/error.rs`) [Minor, test-crafter]
- [x] Add stack frame propagation integration tests — unit tests for frame accumulation/dedup/display + 4 corpus error tests. (`src/error.rs`, `tests/corpus/eval/errors/`) [Major, test-crafter]

### underscore-desugar-a: Core Desugar Implementation

Build the desugar.rs module with all transformation logic.

- [x] Create `src/desugar.rs` module with `desugar_file()` and `desugar_expr()` public functions
- [x] Implement `is_direct_underscore(expr) -> bool` — DIRECT predicate (bare `$_` VarRef, not nested in sub-expression)
- [x] Implement DESUGAR rewrite: top-down WRAP check on raw children, wrapping in `[fn [_] original_expr]` when any child is DIRECT
- [x] Handle all WRAP cases: WRAP-CALL (args only, not func), WRAP-DICT (values only, not keys), WRAP-DOT, WRAP-BRACKET, WRAP-RANGE
- [x] Implement depth-based lexical shadowing — depth counter incremented inside `Fn` with `_` parameter, suppresses desugaring at depth > 0
- [x] Preserve spans: generated `Fn` nodes reuse span of original expression

### underscore-desugar-b: Pipeline Integration & Cleanup

Wire desugar pass into pipeline and remove old eval-time desugaring.

- [x] Integrate into pipeline: `eval_source()` calls `desugar_file()` after parsing, before typecheck (`src/lib.rs`)
- [x] Remove eval-time desugaring: `should_desugar_underscore`, `contains_direct_underscore`, runtime env check (`src/eval.rs:66-74`)
- [x] Migrate existing `test_underscore_*` tests to call `desugar_expr()` before eval
- [x] Add tests: shadowing, exclusions (func position, bracket access keys, range bounds, dict entry keys), nested `$_`
- [x] Reconcile WRAP-CALL `not DIRECT(f)` guard — DESIGN.md spec includes guard but implementation omits it; implementation's behavior for `[call $_ $_]` is arguably more useful (wraps both). Update spec to match implementation or add guard. (`src/desugar.rs:70-86`, `DESIGN.md:2029-2030`) [Minor, computer-scientist]

### underscore-desugar-c: Doc Fixes & Spec Updates

Update DESIGN.md and SPEC.md to reflect completed desugaring migration.

- [x] Update DESIGN.md desugar sketch signatures to match `&mut` implementation — sketch shows value-returning API but code uses in-place mutation (`DESIGN.md:2110-2122`) [Nit, computer-scientist]
- [x] Update DESIGN.md WRAP-CALL pseudocode to show unconditional recursion — spec shows selective `if DIRECT(a) then a else DESUGAR(a)` but impl recurses all children; proven equivalent but spec should match impl (`DESIGN.md:2034-2038`) [Nit, panel consensus]
- [x] Update DESIGN.md line 2096 tense — still says "This replaces the current eval-time..." but migration is complete; update to past tense (`DESIGN.md:2096`) [Nit, computer-scientist]
- [x] Update DESIGN.md line 2124 migration paragraph tense — entire paragraph uses future tense ("is removed once", "move to", "must call") for completed work; update to past tense (`DESIGN.md:2124`) [Nit, computer-scientist]
- [x] Update SPEC.md `desugar_underscores` function name — SPEC.md §6.5 (line 728) and §8.6 (line 1064) reference a function `desugar_underscores` that doesn't exist; actual functions are `desugar_file()` and `desugar_expr()` in `src/desugar.rs` (`SPEC.md:728,1064`) [Minor, computer-scientist]
- [x] Update SPEC.md §6.5 exclusion text for reconciled WRAP-CALL — line 759 says "`[call $_ ...]` — `$_` as the function is an ordinary variable lookup" which is incomplete after WRAP-CALL reconciliation; when both func and arg are DIRECT (e.g., `[call $_ $_]`), wrapping DOES fire and both references bind to the `_` parameter. Update to match DESIGN.md line 2071 explanation. (`SPEC.md:759`) [Minor, computer-scientist]
- [x] Add precondition doc comment to `desugar()` noting parser-bounded AST depth — depends on MAX_PARSE_DEPTH but doesn't assert it; future programmatic AST construction (macros, quasiquoting) must respect bound (`src/desugar.rs:47`) [Nit, computer-scientist]
- [x] Add desugar step to `eval_to_json_with_input` test helper — `lib.rs` test helper parses then evals without calling `desugar_file()`, skipping the desugaring step that all three production entry points (eval_source, main.rs, repl.rs) perform. Currently latent. (`src/lib.rs:520-535`) [Minor, computer-scientist]

### underscore-desugar-d: Edge Case Tests

Additional corpus and unit tests for desugaring edge cases.

- [x] Add explicit origin tags to synthetic `$_` desugared AST nodes — distinguish user-written lambdas from sugar-generated ones for future tooling. Pombrio & Krishnamurthi (2014). [Minor, computer-scientist]
- [x] Add test for `[call $_ $_]` (func and arg both DIRECT) — documents chosen behavior for edge case (`src/desugar.rs`) [Nit, computer-scientist]
- [x] Add test for `$_` in annotations inside shadowing functions — `[fn [_] [fn [x: [@Number $_]] $x]]` edge case; implementation correct but untested (`src/desugar.rs`) [Nit, test-crafter + sprint-reviewer]
- [x] Add corpus tests for exclusion positions — bracket key `$data[$_]`, range bounds `$data[$_..5]`, dict key `[$_: value]`; unit tests exist but corpus tests validate full pipeline (`tests/corpus/eval/`) [Minor, test-crafter + grammar-architect]
- [x] Add corpus test for func-only DIRECT case — `[call $_ $x]` should NOT wrap; documents asymmetry with arg-only case (`tests/corpus/eval/`) [Nit, test-crafter + integration-verifier]
- [x] Add corpus error tests for `$_` — undefined `$_`, key-not-found on desugared access chain, type mismatch inside desugared lambda; validates error span quality (`tests/corpus/eval/errors/`) [Minor, span-integrity-checker]
- [x] Add desugar step to `create_stdlib_env()` — `builtins.rs:2656-2664` parses `prelude.llt` then evaluates without calling `desugar_file()`, skipping desugaring that all production entry points perform. (`src/builtins.rs:2656-2664`) [Minor, integration-verifier C32]

## evalcontext-refactor: EvalContext Parameter Threading

Replace thread-local `INCLUDE_CTX` with parameter-passed `EvalContext`. Unlocks LSP multi-file support and clean sandboxing. See DESIGN.md §EvalContext.

**Unlocks:** `sandbox` (filesystem allowlist lives in EvalConfig)

### evalcontext-types: EvalContext Type Definitions

Define the EvalContext types and add ctx fields to existing structs.

- [x] Create `EvalConfig` struct — `base_dir: PathBuf`, `stdlib_env: Rc<RefCell<Environment>>` (`src/eval.rs`; `allowed_paths` deferred to sandbox sprint)
- [x] Create `EvalState` struct — `include_guard: HashSet<PathBuf>`, `include_cache: HashMap<PathBuf, Rc<Thunk>>` (`src/eval.rs`)
- [x] Create `EvalContext` struct — `config: Rc<EvalConfig>`, `state: Rc<RefCell<EvalState>>` (`src/eval.rs`)
- [x] Add `ctx: Rc<EvalContext>` field to `ThunkState::Unevaluated`, `PendingBuiltin`, `PendingCall` (`src/value.rs:176-190`)
- [x] Add `ctx: Rc<EvalContext>` field to `BuiltinArgs` (`src/builtins.rs`)

### evalcontext-thread: EvalContext Threading and Migration

Thread EvalContext through eval pipeline, migrate include, update API.

**Depends on:** `evalcontext-types`

- [x] Thread `EvalContext` through `eval()`, `materialize()`, and builtin dispatch (`src/eval.rs`)
- [x] Migrate `builtin_include` to use `ctx.state` instead of thread-local `INCLUDE_CTX` (`src/builtins.rs:1024-1170`)
- [x] Remove thread-local `INCLUDE_CTX` and `set_include_context`/`clear_include_context` (`src/builtins.rs:58-70`, `src/lib.rs`)
- [x] Update CLI (`main.rs`): construct `EvalContext` from file path
- [x] Update public API: `EvalContext`, `EvalConfig`, `EvalState` are public; remove `set_include_context`/`clear_include_context`
- [x] Update all include-related tests
- [x] Add EvalContext isolation tests — two contexts with different base_dirs resolve includes independently, include cache persists across calls, include guard detects cycles via ctx.state [Critical, test-crafter C34]
- [x] Add thunk memoization ctx preservation test — verify Unevaluated→Materialized transition preserves correct ctx (Rc::ptr_eq) [Critical, test-crafter C34]
- [x] Suppress unused `ctx` parameter warning in `materialize()` — rename to `_ctx` with TODO comment (`src/eval.rs:863`) [Major, eval-engine C34]

### evalcontext-polish: EvalContext Polish and Documentation

Fix-later findings from evalcontext-thread panel review (C34).

- [x] Add `EvalContext::with_base_dir()` helper to share config via `Rc::clone` instead of allocating new EvalConfig per include (`src/builtins.rs:1065-1072`) [Minor, eval-engine + performance-expert + computer-scientist C34]
- [x] Add cross-context state-sharing test — two contexts sharing `state: Rc::clone(&ctx1.state)` but different `config` should share include_cache and include_guard (`src/eval.rs`) [Minor, eval-engine + test-crafter + integration-verifier C34]
- [x] Add documenting comment on `materialize()` `_ctx` parameter explaining thunks use captured ctx, parameter exists for future use (`src/eval.rs:863`) [Minor, eval-engine + test-crafter + span-integrity-checker + computer-scientist C34]
- [x] Fix stale docstring in repl.rs:106 referencing `IncludeContext` (removed) — should say `EvalContext` (`src/repl.rs:106`) [Nit, computer-scientist C34]
- [x] Fix include guard leak on materialize failure — `?` operator in builtin_include skips cleanup() on Err path; match on result like eval_result (`src/builtins.rs:1091`) [Major, computer-scientist C34]
- [x] Update DESIGN.md evaluation judgment forms to include Sigma (EvalContext) parameter — `<e, rho, Sigma, d> => v` (`DESIGN.md:2247-2329`) [Minor, computer-scientist C34]

### let-gen-verify: Let-Generalization Verification and Testing

Comprehensive tests for HM let-generalization (C40).

- [x] Add test_let_gen_polymorphic_identity — verify identity function generalizes to `∀α. α → α`
- [x] Add test_let_gen_any_touched_monomorphic — verify Any-contaminated vars stay monomorphic
- [x] Add test_let_gen_nested_dicts_level_increment — verify level scoping in nested dicts
- [x] Add test_let_gen_mutual_recursion — verify mutual recursion type assertions
- [x] Add test_instantiate_scheme_with_row_var_body — verify scheme instantiation with row var bodies
- [x] Add test_instantiate_scheme_leaves_free_vars_unchanged — verify selective freshening
- [x] Add corpus test for polymorphic identity at multiple types
- [x] Add corpus test for cross-document scheme threading

### doc-ast-fixes: doc/15-ast.md Accuracy Fixes

Fix AST documentation discrepancies (C41).

- [x] Fix `doc/15-ast.md` `Fn` node missing `desugared: bool` field [Critical, grammar-architect C39]
- [x] Fix `doc/15-ast.md` `TypeAssert` fictional `resolved_type: Option<Type>` field [Major, grammar-architect C39]
- [x] Fix `doc/13-examples.md` §8.9 and §8.10 TypeAssert AST examples showing fictional `resolved_type: None` field [Major, grammar-architect C40]
- [x] Fix `doc/15-ast.md` nesting depth section describing non-existent iterative parser [Major, grammar-architect C39]

### seq-cycle-fix: Seq Cycle Detection Asymmetry

Fix deep_materialize Seq→Dict terminal documentation and Launchbury sharing preservation (C42).

- [x] Document deep_materialize Seq→Dict terminal case — added 2-line comment explaining Seq tail recurses to empty Dict `[]` as terminal value; infinite sequences hit MAX_EVAL_DEPTH (`src/eval.rs:1243-1244`) [Minor, eval-engine]
- [x] Fix deep_materialize breaking Launchbury sharing — replaced `HashSet<*const Thunk>` with `HashMap<*const Thunk, Option<Rc<Thunk>>>` dual-purpose cache: `None` sentinel = blackholing (cycle detection), `Some(rc)` = sharing cache (return cached result); added `deep_materialize_thunk` helper encapsulating the protocol; three new unit tests validating `Rc::ptr_eq` preservation for dict, seq, and cross-structure sharing (`src/eval.rs:1260-1280`) [Minor, computer-scientist]

### let-gen-soundness: CALL-POLY Level Poisoning Fix

Fix CALL-POLY level poisoning (Kiselyov 2013 soundness), stale eval_file doc comments, type doc accuracy, and deep_materialize test gaps (C43).

- [x] Fix CALL-POLY `instantiate()` level poisoning — added `instantiate_at_level(ty: &Type, state: &mut InferState) -> Type` to `types.rs` creating fresh vars at `state.level` and registering in `state.levels`; replaced `instantiate(&func_ty, &mut state.name_counter)` at `typecheck.rs:525` with `instantiate_at_level(&func_ty, state)`; gated old `instantiate()` with `#[cfg(test)]` (`src/typecheck.rs:525`, `src/types.rs:496-518`) [Major, computer-scientist C43]
- [x] Fix stale doc comments in `eval_file` and `eval_file_with_input` — replaced Note blocks referencing deleted `set_include_context`/`clear_include_context` with EvalContext::new() guidance (`src/eval.rs:315-316,332-333`) [Major, integration-verifier C43]
- [x] Fix `doc/05-type-annotations.md:201` "four passes" → "five passes (0–4)" binding to fresh type vars [Minor, type-theorist C43]
- [x] Fix `doc/06-type-inference.md:531` wrong `collect_type_vars()` signature — corrected to out-param `&mut BTreeSet<String>` [Minor, type-theorist C43]
- [x] Fix `doc/06-type-inference.md:184` false claim about Type::Function carrying parameter names [Nit, type-theorist C43]
- [x] Add `test_deep_materialize_cycle_sentinel` unit test — exercises `Some(None)` blackholing branch via pre-populated cache (`src/eval.rs`) [Critical, test-crafter C43]
- [x] Add `test_deep_materialize_preserves_sharing_through_eval` — sharing test using `Thunk::new_unevaluated` to exercise production cache-population path (`src/eval.rs`) [Major, test-crafter C43]

### bidirectional-typing: Core check_expr Framework

Implement bidirectional type checking with synthesis (⇒) and checking (⇐) modes. Pierce & Turner (2000), Dunfield & Krishnaswami (2021).

- [x] Implement `check_expr(expr, expected, env, state, type_map) -> Result<(), Vec<TypeError>>` (`src/typecheck.rs`)
- [x] Apply `check_expr` at CALL-MONO argument positions (expected type fully concrete) (`src/typecheck.rs`)
- [x] Apply `check_expr` for function body with return annotation (`src/typecheck.rs`)
- [x] Apply `check_expr` for `TypeAssert` inner expression (`src/typecheck.rs`)
- [x] Keep `unify` + U-SUBSUME for CALL-POLY argument positions (type variables present) (`src/typecheck.rs`)
- [x] Fix function variance inconsistency between `unify` and `is_subtype` — `check_expr` applies [SUB] at leaves, resolving the dual-path divergence (`src/types.rs:291-315, 67-82`) [Major, computer-scientist]
- [x] Implement lambda checking mode — `infer_fn` checks against expected function type. Pierce & Turner (2000), Dunfield & Krishnaswami (2021). (`src/typecheck.rs:449`) [Major, computer-scientist]
- [x] Implement checking mode propagation to call arguments — Pierce-Turner S-App rule. (`src/typecheck.rs:389-446`) [Major, computer-scientist]

### bidirectional-typing-b: Bidirectional Type Checking (Part 2)

Monomorphic fix, constraint synthesis, tests, and doc comments. Split from bidirectional-typing.

- [x] Fix monomorphic function calls skipping argument type checking — `!func_ty.has_type_vars()` bypasses argument-parameter unification (`src/typecheck.rs:421-422`) [Major, computer-scientist]
- [x] Implement constraint-based type argument synthesis for polymorphic calls — Pierce-Turner constraint generation with variance-aware minimal substitution (`src/typecheck.rs:425-439`) [Minor, computer-scientist]
- [x] Add tests: literal promotion via subsumption, lambda parameter inference from context
- [x] Add doc comment to `instantiate()` explaining level-0 convention for call-site vars — call-site vars created by `instantiate()` are intentionally absent from `InferState.levels` (treated as level 0 = never generalize) in contrast to `InferState::fresh_var()` which always registers. (`src/types.rs:481-493`) [Minor, type-theorist C43]
- [x] Split `TypeScheme.vars: Vec<String>` into `type_vars` and `row_vars` before row-unification — current conflation works due to `apply_inner` `_` arm fallback but creates forward-incompatibility: row-unification sprint requires separate `type_map`/`row_map` substitutions and the conflated `vars` field becomes a blocking impedance mismatch. Pre-split now reduces the row-unification diff surface. (`src/types.rs:172-184`) [Major, type-theorist C43]
- [x] Mark `doc/06-type-inference.md` §Unification `[U-SUBSUME]` as proposed design — current text describes `[U-SUBSUME]` as a "general bidirectional is_subtype fallback" but `src/types.rs:397-424` has 8 specific bidirectional promotion rules and the fallthrough is `Err(type_mismatch)`, not a general subsumption check. The doc is aspirational; mark `[U-SUBSUME]` and the "no promotion rules" claim as PROPOSED DESIGN / NOT YET IMPLEMENTED; document the 8 current promotion rules as deliberate pragmatic extensions of Robinson. (`doc/06-type-inference.md:249-307`) [Major, type-theorist C43]
- [x] Add `generalize()` early-exit for monomorphic types — for every Pass 4 entry, `generalize()` calls `collect_type_vars()` which walks the full type tree and builds a BTreeSet even when the type has no type vars (common case: all-concrete config dicts). Add `if !ty.has_type_vars() { return TypeScheme::mono(ty.clone()); }` early exit. (`src/types.rs:520-541`) [Minor, computer-scientist C43]
- [x] Fix `check_call` zero-param polymorphic function return bypass — line 539 returns `*ret.clone()` (pre-instantiation return type) for zero-param polymorphic functions, bypassing `inst_ret`. The `else` branch should return `*inst_ret.clone()` to use the instantiated type. (`src/typecheck.rs:539`) [Minor, computer-scientist C43 panel]

### bidirectional-typing-c: Bidirectional Type Checking (Part 3)

Nits and stdlib fixes found during bidirectional-typing review. Split from bidirectional-typing.

- [x] Eliminate double instantiation waste — VAR-POLY at `typecheck.rs:191` via `instantiate_scheme` and CALL-POLY at `typecheck.rs:525` via `instantiate_at_level` both instantiate. Refactored to `check_call_with_scheme()` for single instantiation. (`src/typecheck.rs:191,525`) [Nit, computer-scientist C43 panel]
- [x] Add `concat` row to strictness signature table — dual-dispatch (lazy Seq chain via PendingBuiltin, eager Dict append); added to "Higher-order collection operations" group. (`doc/08-evaluation.md:425-530`) [Minor, stdlib-author C43]
- [x] Fix `flatten` Seq input — adds `$seq?` guard with descriptive error "flatten: expected Dict, got Seq — collect the Seq first". (`stdlib/prelude.llt:421`) [Minor, stdlib-author C43]

### call-convention-kotlin: Kotlin-Model Call Convention

- [x] Implement C-COVERAGE: per-parameter coverage check replacing count-based arity — BIND-ARITY loop checks each required param for positional coverage or named arg coverage. (`src/eval.rs:614-632`) [Major, eval-engine]
- [x] Allow named args for any parameter — removed `get_default(p).is_some()` guard; BIND-POSITIONAL case (ii) fires for ANY param (required or optional). (`src/eval.rs:644-648`) [Major, eval-engine]
- [x] Implement Garrigue default-env separation — `default_env` (caller env for normal calls, closure env for $apply) and `closure_env` (parent of call env) correctly separated. (`src/eval.rs:596-704`) [Minor, eval-engine]
- [x] Implement 4 error classes — new E024 MissingRequiredParam (names missing param); E021 NamedArgConflict (C-NO-OVERLAP), E022 UnknownNamedArg (C-NAMED-VALID) pre-existing; ArityMismatch E020 retained for excess args. (`src/error.rs`) [Major, eval-engine]
- [x] Support named args from dict in `$apply` — split dict by key type: Key::Int sorted by value → positional, Key::String → named. (`src/builtins.rs:866-878`) [Minor, eval-engine]
- [x] Add tests for each binding constraint and error class — 10 eval success + 3 eval error corpus tests covering C-COVERAGE, C-NO-OVERLAP, C-NAMED-VALID, C-VARIADIC, $apply split. (`tests/corpus/eval/fn_kotlin_*.llt-eval`) [Minor, test-crafter]
- [x] Add tests for interleaved required/optional parameters — interleaved, two-optionals-before-required, optional-middle, mixed key $apply scenarios. (`tests/corpus/eval/fn_kotlin_*.llt-eval`) [Minor, test-crafter]

## let-generalization: Levels-Based Let-Generalization

Implement proper Hindley-Milner let-polymorphism with levels-based generalization (Kiselyov 2013). Without this, polymorphism requires explicit annotations. See doc/06-type-inference.md §Let-Generalization (Levels-Based).

### let-gen-types: Type System Infrastructure

Data structure changes and signature migration. All interdependent — must land together.

- [x] Add `TypeScheme` struct — `vars: Vec<String>`, `body: Type`, `TypeScheme::mono(ty)` constructor (`src/types.rs`)
- [x] Add `InferState` struct — `name_counter: u32`, `level: u32`, `levels: HashMap<String, u32>` (`src/types.rs`)
- [x] Change `Type::TypeVar(String)` to `TypeVar(String, u32)` with manual `PartialEq` on name only (`src/types.rs:36`)
- [x] Change `RowRest::RowVar(String)` to `RowVar(String, u32)` with level (`src/types.rs`)
- [x] Change `TypeEnv.bindings` from `IndexMap<String, Type>` to `IndexMap<String, TypeScheme>` (`src/types.rs`)
- [x] Replace `counter: &Cell<u32>` parameter with `state: &mut InferState` in `infer_expr`/`infer_dict`/`infer_fn` (`src/typecheck.rs`)
- [x] Update all `TypeVar("a".into())` in tests to `TypeVar("a".into(), 0)` (`src/types.rs`, `src/typecheck.rs`)

### let-gen-inference: Core Inference Rules and Generalization

Core algorithm implementation using let-gen-types data structures. All 7 items are interdependent — must land together.

- [x] Implement `instantiate_scheme(scheme, state) -> Type` — freshen all vars at current level (`src/types.rs`)
- [x] Implement `generalize(level, ty) -> TypeScheme` — collect vars with level > given, abstract them (`src/types.rs`)
- [x] Update VAR rule: `instantiate_scheme(env.get(name)?, state)` (`src/typecheck.rs`)
- [x] Update `infer_dict` to 5 passes: key resolution, bind-all (fresh α at `level+1`), type aliases, infer values (at `level+1`), generalize (`src/typecheck.rs`)
- [x] Implement symmetric level lowering in unify U-VAR rules (`src/types.rs`)
- [x] Implement Any-unification level zeroing: `unify(α, Any)` sets `level(α) = 0` (`src/types.rs`)
- [x] Update `typecheck_document` to thread `TypeScheme`s across `---` boundaries (`src/typecheck.rs`)

(let-gen-verify and let-gen-soundness archived above)

## typeassert-structural: TypeAssert Structural Contract Checking

Replace nominal type tag checking with structural contract validation. See DESIGN.md §TypeAssert Runtime Validation.

- [x] Add `resolved_type: Option<Type>` field to `Expr::TypeAssert` (`src/ast.rs`)
- [x] Update `resolve_type_assert()` to set `resolved_type` on AST node (elaboration) (`src/typecheck.rs:503-523`)
- [x] Implement `value_matches_type(value, type, span) -> Result<bool, EvalError>` for immediate validation (`src/eval.rs`)
- [x] Add `ThunkState::Guarded { inner, expected, field_path, guard_span }` variant (`src/value.rs`)
- [x] Implement proxy contract wrapping: shape check + guard wrapping for record field thunks. Findler & Felleisen (2002). (`src/eval.rs:117-157`)
- [x] Implement guard memoization: `Guarded` → `Materialized` or `Failed` after first force (`src/eval.rs`)
- [x] Handle `--no-typecheck` fallback — degrade to current nominal behavior when `resolved_type` is `None` (`src/eval.rs`)
- [x] Implement blame tracking with even-odd polarity for contract positions. Findler & Felleisen (2002). (`src/eval.rs`) [Major, computer-scientist]

## row-unification — completed sub-sprints

### row-unification-a: Row Variable Pre-Unification Fixes

Bug fixes and documentation in existing row handling code. Independent of the core algorithm.

- [x] Fix row variable substitution creating duplicate fields — `merged.extend(extra_fields)` doesn't check for key collisions (`src/types.rs:166-184`) [Critical, type-theorist]
- [x] Fix sub_rest ignored in record subtyping — `is_subtype` destructures sub record as `(sub_fields, _sub_rest)`, ignoring RowVar/Open rest; a record with `RowVar(r)` may have additional fields via its row variable not checked against a Closed supertype. Latent issue that becomes real with row variable binding. (`src/types.rs:52-64`) [Major, computer-scientist]
- [x] Fix RowVar treated identically to Open in `is_subtype` — add TODO comment explaining row-unification placeholder (`src/types.rs:59-62`) [Major, type-theorist]
- [x] Document RowVar instantiation via TypeVar namespace coincidence — `instantiate()` renames TypeVar names and RowVar names share the same namespace, so RowVars get freshened correctly by accident. Should be documented as intentional or given separate namespace handling. (`src/types.rs:318-330`) [Minor, computer-scientist]

### row-unification-b: Core Remy-Style Unification

Core algorithm implementation. Requires row-unification-a.

- [x] Extend `Substitution::apply` to splice bound row variable fields into records (e.g., `[a: Int | ...r]` with `r → [b: String]` produces `[a: Int, b: String]`)
- [x] Unify row rests: `RowVar` vs `RowVar` binds one to the other, `RowVar` vs `Closed` binds the var to the leftover fields as a closed record
- [x] Handle "remainder" binding: `unify([a: Int | ...r], [a: Int, b: String | Closed])` binds `r → [b: String | Closed]`
- [x] Extend `Type` representation if needed to support partial-row bindings (row var bound to fields + another row var)
- [x] Update `instantiate` to freshen row variables alongside type variables
- [x] Add row-specific occurs check for `RowVar("r")` with `Record(..., RowVar("r"))` (infinite row type prevention)
- [x] Add row variable substitution cycle handling — `Substitution::apply` must handle cycles when row variables bind to records containing the same row variable (`src/types.rs`) [Major, type-theorist]
- [x] Fix open-record unification silently dropping non-shared fields — only shared fields unified, unique fields ignored without constraint; Remy-style would bind fresh row variables to capture remainders (`src/types.rs:334-338`) [Major, computer-scientist]

### row-unification-c: Verification and Bug Fixes

Bug fixes and verification for Rémy-style row-variable unification. Requires row-unification-b.

- [x] Fix `unify_remainders` reachable silent-success when `rho1 == rho2` with non-empty unique fields — added explicit error arm; changed `_ => Ok(())` fallback to `unreachable!()` (`src/types.rs`) [Major, computer-scientist C51]
- [x] Fix row occurs check not chasing TypeVar bindings through substitution — `row_var_occurs_in_type` now chases TypeVar bindings through `subst.type_map` (`src/types.rs`) [Minor, computer-scientist C49]
- [x] Fix anonymous `[...]` open record annotations sharing `_open` row variable name and hardcoded level 0 — now generates fresh `_open{N}` names at `state.level` (`src/typecheck.rs`) [Minor, computer-scientist C49]
- [x] Test inference through polymorphic functions that extend/restrict records — two corpus tests added: `row_poly_extend.llt-eval`, `row_poly_project.llt-eval`
- [x] Verify consistency between `unify` and `is_subtype` for all RowRest combinations — 12 unit tests added; post-substitution consistency confirmed correct
- [x] Ensure row variable occurs check before binding — all 3 binding cases (Cases 2, 3, 4) call `row_var_occurs` before `row_map.insert`; verified sound
- [x] Add debug assertion in `apply_row` field merge — rejected: duplicates in `apply_row` are legitimate (explicit field wins over row-variable-inherited field); explanatory comments added instead

### row-unification-d: Verification Continuation — Tests, Comments, Doc Fixes

Verification follow-on for row-unification-c. Requires row-unification-c.

- [x] Add comment to `unify_tails` RowVar/RowVar binding explaining why no occurs check is needed — Robinson (1965) vacuous satisfaction. (`src/types.rs:547`)
- [x] Add comment to `row_var_occurs_in_type` TypeVar chase explaining no-cycle invariant. (`src/types.rs:495`)
- [x] Update `doc/07-type-extensions.md` pseudocode for `unify_remainders` — added Case 7, `u2_empty`/`u1_empty` guards, `when ρ₁ ≠ ρ₂` on Case 4.
- [x] Corpus error test for same-rho soundness — determined infeasible (annotation mapping isolation prevents shared row vars end-to-end; unit test is sufficient)
- [x] Add cross-function anonymous open record test — `test_cross_function_anonymous_open_records_get_fresh_vars` (`src/typecheck.rs:2405`)
- [x] Add `test_lower_row_var_levels_prevents_generalization` (`src/types.rs:4510`)
- [x] Add `test_unify_tails_empty_vs_rowvar` symmetric direction (`src/types.rs:4557`)
- [x] Add multi-hop TypeVar chase test (`src/types.rs:4585`)

### row-unification-f-b: State.Subst Doc Sync and Minor Fixes

Doc/06 sync and minor correctness fixes for the access-chain changes. Requires row-unification-f.

- [x] Apply `state.subst` before `is_subtype` in `check_expr` default path — bilateral `state.subst.apply()` on both `actual` and `expected`. (`src/typecheck.rs:438-439`)
- [x] Apply `state.subst` to target type in `check_bracket_access` — `state.subst.apply()` on `target_ty`. (`src/typecheck.rs:717`)
- [x] Update doc/06 `[DOT-OPEN]` rule to reflect constraint generation — replaced with [DOT-VAR] and [DOT-ROWVAR] rules. (`doc/06-type-inference.md:193-201`)
- [x] Add `subst` field to `InferState` struct block and reference table in doc/06. (`doc/06-type-inference.md:409, 542-543`)
- [x] Fix doc/07:134 `### Default type validation` heading inconsistency — reverted to bold paragraph header. (`doc/07-type-extensions.md:134`)
- [x] Tighten doc/07:138 TypeScript/Haskell/OCaml analogy. (`doc/07-type-extensions.md:138`)
- [x] Apply `state.subst` in `check_call_with_scheme` CALL-MONO return — `state.subst.apply(ret)`. (`src/typecheck.rs:835`)
- [x] Apply `state.subst` to `target_ty` in `check_range_access`. (`src/typecheck.rs:762`)

### row-unification-g: Test Coverage and Nit Fixes

Test coverage gaps and nit fixes from the row-unification-e and row-unification-f panel reviews. Requires row-unification-f.

- [x] Remove vestigial `unify_tails` line from doc/07 Part 5 pseudocode — line 562 shows `unify_tails(RowVar(ρ), RowVar(ρ_fresh))` with comment "not needed, just bind:" followed by the actual direct binding; the vestigial line is confusing. (`doc/07-type-extensions.md:562`) [Nit, computer-scientist C52]
- [x] Add `check_dot_access` occurs-check error path test — `typecheck.rs:621-628` returns "infinite row type: {rho} occurs in its own binding" when dot-access on an open-record produces a self-referential row binding; zero tests trigger this path. Add `test_dot_access_open_record_infinite_row_cycle`. (`src/typecheck.rs:621-628`) [Major, test-crafter C52]
- [x] Strengthen `test_dot_access_typevar_generates_constraint` assertion — currently only checks result is `TypeVar` not `Any`; does not verify the correct constraint `α = Record({name: β}, ρ)` was generated. Extract the TypeVar name for `result`, then verify `data`'s inferred type is a `Record` with field `name` equal to that same TypeVar. (`src/typecheck.rs:1617-1637`) [Major, test-crafter C52]
- [x] Strengthen `test_dot_access_open_record_extends_tail` assertion — verifies `r1` and `r2` are TypeVars but not that they are DISTINCT TypeVars; if the implementation returned the same fresh β twice, test would pass. Add assertion that the TypeVar names differ. (`src/typecheck.rs:1649-1661`) [Major, test-crafter C52]
- [x] Add TypeAssert default inference-error propagation test — `resolve_type_assert` at `typecheck.rs:1058-1061` propagates `Err(errs)` when the default expression itself fails to infer; no test exercises this arm. Add `check_err("[@[type: Number  default: $undefined_var] 42]")` asserting any error is produced. (`src/typecheck.rs:1058-1061`) [Minor, test-crafter C52]
- [x] Add corpus tests for Part 5 access chain constraint generation — `tests/corpus/eval/typecheck/` has row_poly tests but none for access chains; add test for `[result: $data.name  data: [name: hello]]` type-checking correctly, and multi-field accumulation. [Minor, test-crafter C52]
- [x] Add corpus tests for TypeAssert default behavior — `tests/corpus/eval/type_assertions/` has only 3 pass tests and 1 error test; add corpus tests for `default:` suppression (wrong main expr, correct default) and default type mismatch (hard error). [Minor, test-crafter C52]
- [x] Add comment to `mem::take` in `check_dot_access` TypeVar case — borrow-split rationale not documented; future readers may be confused by the pattern. (`src/typecheck.rs:661`) [Nit, type-theorist C52]

### row-unification-g-b: Test Coverage and Nit Fixes (Continued)

Overflow from row-unification-g plus nit fixes from the row-unification-f panel review. Requires row-unification-g.

- [x] Add unit test for `validate_and_wrap_record` nested `field_path` error message — `src/eval.rs:178-193` builds a `"field \"x\": record missing field \"y\""` prefix when `field_path` is non-empty (fires during `ThunkState::Guarded` materialization of nested record fields), but zero tests exercise this branch; any refactor of the `field_path_prefix` concatenation would go uncaught. Add a unit test that calls `validate_and_wrap_record` with a non-empty `field_path` and verifies the error message contains the expected prefix. (`src/eval.rs:178-193`) [Major, test-crafter C53]
- [x] Add corpus laziness test for `ThunkState::Guarded` record proxy — `tests/corpus/eval/laziness/` has nine tests but none demonstrate that proxy contract field validation fires lazily (only when the guarded field is accessed, not at assertion time). Add `tests/corpus/eval/laziness/typeassert_record_lazy_guard.llt-eval`: a closed record TypeAssert where one field contains a side-effectful or error-producing thunk (`$error`) that is never accessed — if the guard fires eagerly the test fails, if lazy it passes. (`tests/corpus/eval/laziness/`) [Major, test-crafter C53]
- [x] Fix termination comment for recursive `unify_rows` call — line 890 says "Terminates because each round binds at least one row variable, and the occurs check bounds the number of variables"; the first clause is false for concrete-field-only recursion. Replace with: "Terminates because each recursive entry requires Step 3 to have bound at least one row variable (surfacing new_shared fields), strictly reducing the number of unbound row variables. The occurs check prevents cyclic bindings." (`src/types.rs:887-892`) [Nit, computer-scientist C53]
- [x] Annotate Pass 3b row-tail limitation — `tail: row.tail.clone()` does not chase the tail through local subst; add comment: "Tail not applied through local subst here — Pass 3c's `subst.apply()` chases tail chains transitively." (`src/typecheck.rs:543-555`) [Nit, type-theorist C53]
- [x] Cite `row-unification-f-b` in `test_let_gen_typevar_in_dot_access` comment — the comment documents known incompleteness ("β is not yet resolved to IntLiteral(1)") but doesn't cite the tracking TODO item. Append "See row-unification-f-b in TODO.md." (`src/typecheck.rs:2917-2920`) [Nit, sprint-reviewer C53]
- [x] Add ASSERT-DEFAULT suppression test — add `test_typeassert_default_suppresses_main_error_but_propagates_ok` that calls `check("[result: [@[type: Number  default: 0] hello]]")` and verifies the result is `Ok`, confirming main-check error is suppressed when a valid default is present. (`src/typecheck.rs`) [Minor, sprint-reviewer C53]
- [x] Add CALL-POLY state.subst constraint test — add a test where a forward-reference dot-access binds a TypeVar in state.subst before a polymorphic call whose return type references that same TypeVar, verifying `state.subst.apply()` in the CALL-POLY arm changes the result. Without this, removing `state.subst.apply()` from `check_call`/`check_call_with_scheme` would not break any test. (`src/typecheck.rs:837, 958`) [Minor, test-crafter C53]
- [x] Add typecheck corpus runner — `tests/corpus/eval/typecheck/` is executed by the eval runner which discards type errors (`let _ = typecheck::typecheck_file(...)` at corpus_tests.rs:81); type-checker regressions that don't change runtime output are invisible. Add a dedicated `test_typecheck_corpus()` that calls `typecheck_file()` and validates Ok/Err. (`tests/corpus_tests.rs:81`, `tests/corpus/eval/typecheck/`) [Major, test-crafter C54]

### row-unification-g-c: Test Coverage and Security Hardening (C55 Overflow)

Overflow from row-unification-g-b plus new findings from the C55 panel review. Requires row-unification-g-b.

- [x] Add `unify_rows` recursion depth counter — `unify_rows` is called recursively in Step 3.6 (via `unify()` on shared fields); adversarial nested row types can produce call stacks proportional to nesting depth; add a `depth: usize` parameter with a `MAX_UNIFY_DEPTH` guard (e.g. 256) returning `TypeError` on overflow, consistent with `MAX_EVAL_DEPTH` in the evaluator. (`src/types.rs`) [Minor, security-expert C55] — decided: depth counter not implemented; parser's MAX_PARSE_DEPTH=256 bounds unification recursion structurally. Visited-set defense-in-depth added instead.
- [x] Add visited set to `row_var_occurs_in_type` — the TypeVar-chase branch added in row-unification-c follows `subst.type_map[α]` recursively; a cyclic type_map (impossible under correct occurs-check invariants but not defended against) causes unbounded recursion. Add a `&mut HashSet<&str>` visited parameter and return `false` on re-visit, consistent with the defense-in-depth pattern used elsewhere in the type system. (`src/types.rs:460-481`) [Minor, security-expert C55]
- [x] Add `check_call` CALL-MONO `state.subst.apply` regression test — no test verifies that removing `state.subst.apply()` from the CALL-MONO return in `check_call_with_scheme` (line 835) changes any result; the KNOWN ISSUE from row-unification-f-b requires a future scenario where CALL-MONO fires with a TypeVar in state.subst. Add a documentation test or pending placeholder with a comment explaining the condition under which it will become observable. (`src/typecheck.rs:835`) [Major, test-crafter C55]
- [x] Add `check_range_access` TypeVar arm coverage test — `check_range_access` handles `Type::Record`, `Type::Any`, and `_` (error); no test exercises the TypeVar fall-through (which would produce a spurious "expected record type" error on an inferred TypeVar target); add `test_range_access_typevar_target` that calls range access on an open-record-typed dict and verifies the error message or result. (`src/typecheck.rs:677`) [Minor, test-crafter C55]
- [x] Rename `test_dot_access_open_record_infinite_row_cycle` and document occurs-check invariant — test name promises "infinite row cycle" but the body documents "the error path is likely unreachable" and tests a different code path (TypeVar forward-ref). Renamed to `test_dot_access_constraint_generation_on_open_record_with_known_field`. Added formal proof-sketch comment and `debug_assert!(false, "unreachable: fresh row var occurs in its own binding")` + `Err(...)` for defense-in-depth. (`src/typecheck.rs:1707-1758, 657-661`) [Major, test-crafter C56]
- [x] Fix `test_dot_access_typevar_generates_constraint_verified` dual-accept assertion — collapsed from dual-accept (StringLiteral OR TypeVar) to single assertion: TypeVar type + registered in state.levels. Removed the subsumed older `test_dot_access_typevar_generates_constraint`. (`src/typecheck.rs:1793-1805, 1661`) [Major, test-crafter C56]
- [x] Fix `resolve_type_assert` panic! → debug_assert! — the write-once invariant guard at `src/typecheck.rs:1065-1069` uses `if prev.is_some() { panic!(...) }` which fires in production builds; the type checker is advisory and should not panic. Change to `debug_assert!(prev.is_none(), "resolved_type written twice — elaboration invariant violated (span: {:?})", annotation.span)` so the invariant is enforced in debug/test builds but stripped in release. (`src/typecheck.rs:1065-1069`) [Minor, type-theorist C56] — done in test-crafter-c62 (DONE.md line 1060)

### cycle-findings-c33-a: Major Findings (Cycle #33)

Major findings from Cycle #33 codebase review. All items independent unless noted.

- [x] Fix `eval_range_access` Proxy and wildcard error paths bypass `push_frame` — wrapped both error paths through `push_frame(...)` for consistent stack frames. (`src/eval.rs:1116-1129`) [Major, grammar-architect C33]
- [x] Fix `$join` output string size unbounded — added `MAX_STRING_SIZE` guard with saturating arithmetic before `.join()` calls on both Dict and Seq paths, plus empty-collection arithmetic guard. (`src/builtins.rs:2535, 2578`) [Major, security-expert C33]
- [x] Add no-match fast-path to `builtin_replace` — added `if match_count == 0` early return to skip redundant second string traversal. (`src/builtins.rs:596-609`) [Major, performance-expert C33]
- [x] Fix `$concat` dict path uses unchecked `idx += 1` arithmetic — replaced with `checked_add(1).ok_or_else(|| EvalError::integer_overflow(...))`. (`src/builtins.rs:2646, 2651`) [Major, security-expert C33]
- [x] Fix `check_range_access` missing `Type::Proxy` arm — added `Type::Proxy => Err(...)` with "range access is not supported on Proxy values" error, matching runtime behavior. (`src/typecheck.rs:837`) [Major, computer-scientist C33]
- [x] Fix `eval_dot_access` allocates `Key::String` on every field lookup — implemented `StrKey` wrapper with `Hash` + `Equivalent<Key>` for zero-allocation dot-access lookups. (`src/eval.rs:1037`, `src/value.rs`) [Major, performance-expert C33]
- [x] Fix `doc/03-data-model.md` stale `eval_as_dict` references — updated Property 4 proof, FORCE-DICT row, ACCESS-* line numbers. (`doc/03-data-model.md:355, 389-392`) [Major, grammar-architect C33]
- [x] Fix `doc/08-evaluation.md` dead `eval_as_dict` row in Laziness Design table — removed stale row. (`doc/08-evaluation.md:842`) [Major, grammar-architect C33]

## eval-sandbox-flags: Adversarial Evaluation Flags

Extend `llt eval` with `--no-fs`, `--timeout`, and structured exit codes for adversarial/sandboxed use. See `doc/12-tooling.md` §Adversarial Evaluation.

- [x] Add `--no-fs` flag to the `eval` subcommand in clap — sets `EvalConfig::no_fs = true`; `$include` returns an error immediately when `no_fs` is set (`src/main.rs`, `src/builtins.rs`)
- [x] Add `no_fs: bool` to `EvalConfig`; check it at the top of `builtin_include` and emit `EvalError::IncludeForbidden` (`src/eval.rs`, `src/builtins.rs`)
- [x] Add `--timeout <duration>` flag to the `eval` subcommand — parse duration string (e.g. `30s`, `500ms`); install SIGALRM handler via `libc::alarm()` at start of `run_eval`; handler exits with code 2 (`src/main.rs`)
- [x] Define exit code constants: 0=success, 1=eval/parse/type error (current behavior), 2=timeout (SIGALRM), 3=resource limit (SIGXCPU/SIGXFSZ from rlimit) — update `run_eval` to map error variants to correct exit codes (`src/main.rs`)
- [x] Add corpus test: `--no-fs` with an `$include` call produces an error (`tests/corpus/eval/errors/`)
- [x] Add corpus test: `--timeout 0s` causes exit code 2 (or smallest meaningful duration that reliably fires before eval completes)

## sandbox-polish-a: Sandbox Code Hardening

Code hardening for the `eval-sandbox-flags` sprint. See DONE.md for original sprint.

- [x] Add `#[cfg(unix)]` guard on SIGALRM/alarm code in `src/main.rs`; return clear error on non-Unix platforms when `--timeout` is used (`src/main.rs:170-191`)
- [x] Add alarm cancellation on success path: `unsafe { libc::alarm(0); }` before `Ok(())` return in `run_eval` — prevents stale alarm from firing during slow stdout serialization (`src/main.rs`)
- [x] Replace `libc::signal()` with `libc::sigaction()` for SIGALRM handler installation — more portable, avoids unspecified handler-reset and syscall-restart behavior (`src/main.rs:182`)
- [x] Add inline comment explaining exit code allocation: `// Exit code 2 is reserved for --timeout; panics are general errors (code 1)` at panic exit branch (`src/main.rs:111`)
- [x] `parse_duration` unit test: `999ms` rounds up to 1 second (boundary case) (`src/main.rs`)
- [x] `parse_duration` unit test: very large minutes value (e.g. `100000000m`) rejected as out-of-range (`src/main.rs`)
- [x] Document `IncludeForbidden` catchability as a conscious design decision — add note to `doc/10-errors.md` or `doc/12-tooling.md` §Adversarial Evaluation explaining why sandbox violations are catchable via `$try` (Nix `tryEval` model: graceful degradation; tradeoff: allows attacker to detect `--no-fs` mode)
- [x] Correct `doc/12-tooling.md` §Adversarial Evaluation flag-scope description — currently says "flags are global (before the subcommand)" but they are correctly scoped to the `eval` subcommand (`doc/12-tooling.md`)

## sandbox-polish-b: Sandbox Integration Tests

Integration test coverage for sandbox flags. Requires sandbox-polish-a.

- [x] CLI test: `--no-fs` does not break normal evaluation (happy path — `[x: 1]` with `--no-fs` should succeed with exit code 0) (`tests/cli_tests.rs`)
- [x] CLI test: `--timeout` with fast-completing program succeeds (exit code 0, not timeout) (`tests/cli_tests.rs`)
- [x] CLI test: invalid `--timeout` argument (e.g. `abc`) rejects at parse time with exit code 1 (`tests/cli_tests.rs`)
- [x] CLI test: `--no-fs` and `--timeout` flags compose correctly (`tests/cli_tests.rs`)
- [x] Corpus test: `IncludeForbidden` E042 error format in `tests/corpus/eval/errors/` (`tests/corpus/eval/errors/`)
- [x] `parse_duration` unit test: millisecond u32::MAX overflow path (`4294967296000ms` should be rejected as out-of-range) (`src/main.rs`)
- [x] `parse_duration`: use `checked_add` for `ms + 999` rounding expression to prevent overflow on extreme millisecond inputs (`src/main.rs`)

## sandbox-polish-c: Sandbox Test Polish

Test coverage gaps and naming consistency from sandbox-polish-b panel review.

- [x] Extend corpus test harness to support `no_fs` directive, then add E042 IncludeForbidden corpus test (`tests/corpus_tests.rs`, `tests/corpus/eval/errors/`)
- [x] Add `parse_duration` test for `checked_add` None path: `"18446744073709550617ms"` (u64::MAX - 998, exact boundary) (`src/main.rs`)
- [x] Rename sandbox CLI tests to include `_flag` infix for consistency with existing `no_fs_flag_blocks_include` and `timeout_flag_exits_with_sigalrm` (`tests/cli_tests.rs`)

## overridable-ops: Overridable Operators and `$proxy`

Make comparison, arithmetic, and collection operators shadowable from stdlib, enabling embedded DSLs (e.g. `stdlib/sql.llt`) to intercept them. Add `$proxy` as a generic field-access interception primitive. See `doc/whatif/sql-translation.md` §Implementation in stdlib.

- [x] Add `$proxy handler-fn` Rust builtin: returns `Value::Proxy { handler }` — a new value variant where any field access `.field` calls `handler("field")` and returns the result (`src/builtins.rs`, `src/eval.rs`)
- [x] Handle `Value::Proxy` in field-access evaluation — force handler and call with field name string (`src/eval.rs`)
- [x] Add stable Rust aliases in `create_root_env()` for operators that need wrappers: `builtin-lt` (=`<`), `builtin-eq` (=`=`), `builtin-add` (=`+`), `builtin-sub` (=`-`), `builtin-mul` (=`*`), `builtin-div` (=`/`), `builtin-if` (=`if`), `builtin-filter` (=`filter`), `builtin-map` (=`map`), `builtin-reduce` (=`reduce`), `builtin-take` (=`take`), `builtin-drop` (=`drop`) (`src/builtins.rs`)
- [x] Add prelude wrappers in `stdlib/prelude.llt` for `<`, `=`, `+`, `-`, `*`, `/` — each delegates to its `$builtin-X` alias — so these names become shadowable without breaking the prelude's own uses (which continue to resolve via the letrec scope to the wrapper, which calls the Rust primitive) (`stdlib/prelude.llt`)
- [x] Add prelude wrappers for `filter`, `map`, `reduce`, `take`, `drop` — each delegates to its `$builtin-X` alias (`stdlib/prelude.llt`)
- [x] Corpus test: shadow `<` with a custom version in user scope; verify it is called instead of the builtin (`tests/corpus/eval/`)
- [x] Corpus test: `$proxy` field access calls handler; verify different field names produce different results (`tests/corpus/eval/`)
- [x] Add composition test that verifies conjunctive enforcement: program uses `$include` AND runs with `--timeout`, both flags active (`tests/cli_tests.rs`)

### test-crafter-c62-b: Test Additions and Doc Bibliography (Cycle #62)

Overflow from test-crafter-c62. Items independent of one another.

- [x] Add `test_call_poly_state_subst_isolation` — `test_call_poly_state_subst_applied` at `src/typecheck.rs:3606` documents in a 50-line comment that removing `state.subst.apply()` from the CALL-POLY return does NOT break this test; add a companion test using cross-document scope where state.subst is populated by a prior dot-access constraint BEFORE the polymorphic call, so that removing only the CALL-POLY `state.subst.apply()` changes the result. (`src/typecheck.rs`) [Minor, test-crafter C62]
- [x] Fix `_rho_level` bound-but-ignored in `check_dot_access` RowVar arm — `src/typecheck.rs:642` binds `_rho_level` from the RowTail but line 666 re-looks up the level via `state.levels.get(rho).copied().unwrap_or(0)`, creating a silent implicit dependency. Use `_rho_level` directly with a `debug_assert!` that it matches `state.levels.get(rho)`, making the invariant explicit. (`src/typecheck.rs:642, 666`) [Nit, integration-verifier C62]
- [x] Fix doc/07 `unify_remainders` pseudocode Cases 2/3 missing guards — lines 465 and 471 show `(false, _, true, RowVar(ρ₂))` and `(true, RowVar(ρ₁), false, _)` without the `u2_empty`/`u1_empty` guards that prevent shadowing Case 4. The implementation is correct but the pseudocode omits the guards; a reader would not know why they exist. Add guards to pseudocode with comment: "Guard prevents shadowing Case 4 — when both have unique fields with different RowVars, Case 4 applies." (`doc/07-type-extensions.md:465, 471`) [Nit, integration-verifier C62]
- [x] Add Wand (1987), Gaster & Jones (1996), Harper & Pierce (1991) to `doc/17-references.md` — all three cited in `doc/07-type-extensions.md:678-680` by name but absent from the canonical bibliography. Promote citation text to doc/17 §Row polymorphism. [Supersedes doc-rowunification-retrospective-b Major] (`doc/17-references.md:21-23`) [Major, grammar-architect C62]
- [x] Remove stale "Task 2:" sprint label from `test_dot_access_constraint_generation_on_typevar_forward_ref` comment — the comment at `src/typecheck.rs` still has "Task 2:" prefix from the test-crafter-c62 sprint plan; replace with blank or the function's actual description. (`src/typecheck.rs`) [Nit, sprint-reviewer C62]
- [x] Add `// TODO(row-unification-f-b)` comment at `test_dot_access_typevar_generates_constraint` assertion site — the dual-accept `assert!(matches!(..., Type::StringLiteral(_) | Type::TypeVar(_, _)))` at `src/typecheck.rs` will become single-accept when row-unification-f-b lands (TypeVar resolved → StringLiteral). Without a tracked comment, the row-unification-f-b sprint won't know to tighten the assertion. (`src/typecheck.rs`) [Nit, test-crafter C62]
- [x] Add `tests/corpus/valid/complex` to `test_corpus_structure` required_dirs — directory exists (added in a prior sprint) but is not in the `required_dirs` guard at `tests/corpus_tests.rs`; deleting it would not fail the structure test. (`tests/corpus_tests.rs`) [Nit, test-crafter C62]

### test-crafter-c62: Test Coverage and Doc Corrections (Cycle #62)

New findings from Cycle #62 full codebase health review (test-crafter). Items independent of one another unless noted.

- [x] Fix `check_call` CALL-MONO missing `state.subst.apply()` — `check_call` at `src/typecheck.rs:904` returns `Ok(*ret.clone())` without applying `state.subst`, while the parallel `check_call_with_scheme` CALL-MONO at line 836 correctly returns `Ok(state.subst.apply(ret))`. The `has_type_vars()` invariant makes this safe today but if the guard is ever relaxed for RowVar-only polymorphism the inconsistency becomes a silent regression. Change line 904 to `return Ok(state.subst.apply(ret))` for defensive consistency. [Supersedes row-unification-h-b Minor] (`src/typecheck.rs:904`) [Minor, test-crafter C62]
- [x] Documented `test_dot_access_typevar_generates_constraint_verified` dual-accept rationale — test accepts `Type::StringLiteral("hello")` OR `Type::TypeVar` because forward-ref letrec constraint propagation is incomplete (see `row-unification-f-b`); `Any` would indicate no constraint generated. Restored `test_dot_access_typevar_generates_constraint` baseline as companion test. Full resolution to StringLiteral deferred to `row-unification-f-b`. [Supersedes row-unification-g-c Major] (`src/typecheck.rs:1794-1805, 1737`) [Major, test-crafter C62]
- [x] Rename `test_dot_access_open_record_infinite_row_cycle` — test name promises "infinite row cycle" but body documents "the error path is likely unreachable" and exercises the TypeVar constraint-generation path. Rename to `test_dot_access_constraint_generation_on_typevar_forward_ref`. [Supersedes row-unification-g-c Major] (`src/typecheck.rs:1708`) [Major, test-crafter C62]
- [x] Fix `test_corpus_structure` missing directories — `required_dirs` at `tests/corpus_tests.rs:235-242` omits `tests/corpus/eval/typecheck/` and `tests/corpus/eval/laziness/`; deleting either would not fail the structure test. Expand to cover all directories with test files. [Supersedes row-unification-h Minor] (`tests/corpus_tests.rs:235-242`) [Minor, test-crafter C62]
- [x] Fix `resolve_type_assert` panic in production — `src/typecheck.rs:1066-1070` uses `panic!()` for the write-once invariant guard; the type checker must not crash in release builds. Change to `debug_assert!(prev.is_none(), "resolved_type written twice — elaboration invariant violated (span: {:?})", annotation.span)`. [Supersedes row-unification-g-c Minor] (`src/typecheck.rs:1066-1070`) [Minor, test-crafter C62]
- [x] Fix doc/07 Part 4 stale "must be split" sentence — line 540 reads "The current implementation conflates them in a single `BTreeSet<String>` and single `Substitution::map` — this must be split"; the split was done in row-unification-b. Replace with: "The implementation uses separate namespaces: `type_map` for type variables, `row_map` for row variables." [Supersedes doc-rowunification-retrospective Major] (`doc/07-type-extensions.md:540`) [Major, grammar-architect C62]
- [x] Fix doc/07 Part 5 stale "can be implemented after Parts 1-4" text — line 583 still describes Part 5 as a future enhancement; Part 5 is fully implemented in row-unification-e. Replace with: "Part 5 is complete as of row-unification-e." [Supersedes doc-rowunification-retrospective-b Minor] (`doc/07-type-extensions.md:583`) [Minor, grammar-architect C62]
- [x] Fix doc/07 Part 8 uses `_r{n}` but code uses `_open{n}` — display example at line 623 shows `RowVar("_r0")` and line 640 says "generates `_r{n}` names"; all tests assert `starts_with("_open")`. Update doc to use `_open{n}`. [Supersedes doc-rowunification-retrospective Major] (`doc/07-type-extensions.md:623, 640`) [Major, integration-verifier C62]

### test-crafter-c64: Test Coverage Findings (Cycle #64)

New test coverage gaps and infrastructure issues from Cycle #64 full codebase health review (test-crafter). Focus: error corpus quality after overridable-ops sprint, split_test_file robustness, stale TODO items.

- [x] Add error codes to expected substrings in all 39 error corpus tests — `test_eval_error_corpus` checks only substring containment; none of the 39 `.llt-eval` files in `tests/corpus/eval/errors/` include `[EXXX]` in their expected section. `test_eval_error_corpus_has_error_codes` verifies the runtime output has A code, but not the CORRECT code — if TypeMismatch regressed from E010 to E099, both tests would pass. Fix: update each expected section to include the error code prefix (e.g., `"type mismatch: expected Function or Builtin, got Int"` → `"[E010] type mismatch"`). This enforces code correctness without requiring exact message text. (`tests/corpus/eval/errors/*.llt-eval`, 39 files) [Critical, test-crafter C64]
- [x] Add corpus test for Proxy JSON serialization error — `lib.rs:209` raises `EvalError::new("cannot serialize Proxy to JSON")` (E099/Internal); a unit test exists (`test_json_proxy_error` at `src/lib.rs:532`) but no corpus test. Created CLI test instead (corpus runner uses display_string, not JSON serialization). (`tests/cli_tests.rs`) [Major, test-crafter C64]
- [x] Fix `split_test_file` false-positive no_fs activation — directive parsing changed from `contains("no_fs")` to exact token match after `#` prefix stripping. Added 7 unit tests. (`tests/corpus_tests.rs:62`) [Major, test-crafter C64]
- [x] Mark two stale TODO.md items as complete — both `test_corpus_structure` required_dirs items already done. [Minor, test-crafter C64]
- [x] Document `split_test_file` '#' stripping in corpus test format guide — added docstring to `split_test_file` explaining directive line behavior. (`tests/corpus_tests.rs:50-60`) [Nit, test-crafter C64]

### computer-scientist-c65: Theoretical Soundness Findings (Cycle #65)

New findings from Cycle #65 full codebase health review (computer-scientist). Focus: HM invariants, bidirectional checking completeness, doc/06 accuracy. All items independent.

- [x] Fix return annotation with TypeVar using subsumption instead of unification — completeness gap — `infer_fn` at `src/typecheck.rs:1051` calls `check_expr(body, &declared, ...)` where `declared` may contain TypeVars from annotations (e.g., `[fn@a [x@a] 42]` resolves return annotation to `TypeVar("_t5")`). `check_expr` uses `is_subtype(IntLiteral(42), TypeVar("_t5"))` which returns `false` (TypeVars only match by reflexive equality in `is_subtype`). Standard HM would unify the body type with the declared return type, binding `a = IntLiteral(42)` and producing `Fn(IntLiteral(42) -> IntLiteral(42))`. Fix: when return annotation contains TypeVars (checked via `declared.has_type_vars()`), use `unify(&body_ty, &declared, &mut state.subst, state, body.span)` instead of `check_expr(body, &declared, ...)`. This matches Damas & Milner (1982): return annotations with type variables are unification constraints, not checking judgments. Pierce & Turner (2000) section 3.2 notes that checking mode assumes the expected type is "known" (ground), which TypeVars are not. (`src/typecheck.rs:1045-1055`) [Minor, computer-scientist C65] — fixed in computer-scientist-c65
- [x] Fix doc/06 line 507 factually misleading about CALL-POLY/CALL-MONO interaction — text says "the instantiated type typically has no remaining type variables (the fresh `_tN` variables are monomorphic instances), so CALL-POLY sees `has_type_vars() = false` and takes the CALL-MONO fast path". Fresh `_tN` variables from `instantiate_scheme` ARE type variables — `has_type_vars()` returns `true` for them. The statement confuses "monomorphic instances" (TypeVars at current level) with "fully concrete types" (no TypeVars). Instantiation of a polymorphic scheme always produces TypeVars; CALL-MONO only fires when the scheme is monomorphic (no quantified vars) and the body is concrete. Fix: rewrite to describe the actual routing: polymorphic schemes go to `check_call_with_scheme` (line 231), monomorphic schemes with concrete bodies take CALL-MONO. (`doc/06-type-inference.md:507`) [Nit, computer-scientist C65] — fixed in computer-scientist-c65

### computer-scientist-c66: Theoretical Soundness Findings (Cycle #66)

New findings from Cycle #66 full codebase health review (computer-scientist + type-theorist). Focus: C65-induced gaps in bidirectional typing completeness, Algorithm W substitution threading, doc/06 spec drift. All items independent.

- [x] Fix `check_expr` lambda checking mode TypeVar-subsumption gap — parallel to C65 `infer_fn` fix — when a lambda `[fn@b [y@b] $y]` is checked against a concrete function type (CALL-MONO argument checking), `resolve_annotation` creates fresh TypeVars in `ann_mapping`. Then `is_subtype(expected_ty, &resolved)` at `src/typecheck.rs:363` returns `false` because `is_subtype` only matches TypeVars reflexively. Same issue at line 406 for return annotation: `is_subtype(&declared, expected_ret)` fails when `declared = TypeVar("_t7")`. Mirror the C65 fix: when `resolved.has_type_vars()`, use `unify(expected_ty, &resolved, ...)` instead of `is_subtype`; when `declared.has_type_vars()`, use `unify(&declared, expected_ret, ...)`. Restores Damas & Milner (1982): annotation variables are constraint-solved, not reflexively compared. (`src/typecheck.rs:363, 406`) [Major, computer-scientist C66]
- [x] Fix `check_call` missing `state.subst.apply()` before `has_type_vars()` test — C65-induced wrong return type for inline lambda polymorphic return annotations — `infer_fn` stores TypeVar bindings in `state.subst` when using unification mode. `check_call` at `src/typecheck.rs:888` calls `infer_expr` to get `func_ty` but does not apply `state.subst`. For `[call [fn@a [x@a] 42] 1]`: `infer_fn` returns `Fn(TypeVar("_t5") -> TypeVar("_t5"))` with `_t5 -> IntLiteral(42)` in `state.subst`; `check_call` sees `has_type_vars()=true`, fires CALL-POLY; `instantiate_at_level` renames `_t5 -> _t7` (fresh, NOT in `state.subst`); unification binds `_t7 -> IntLiteral(1)` (argument); result: `IntLiteral(1)` instead of `IntLiteral(42)`. Fix: Add `let func_ty = state.subst.apply(&func_ty);` at line 888. Subsumes TODO.md row-unification-h-b item tracking the same missing apply for TypeVar func expressions. (`src/typecheck.rs:888`) [Major, type-theorist + computer-scientist C66]
- [x] Fix `doc/06-type-inference.md` [FN] rule stale after C65 return annotation fix — lines 135-146 unconditionally describe checking mode (`body ⇐ σᵣ`) but the C65 fix added conditional dispatch: unification mode when `declared.has_type_vars()`, checking mode otherwise. Split the [FN] rule into two cases: (1) when `has_type_vars(σᵣ)`: infer body type `τ_body`, then `unify(τ_body, σᵣ, S)` (unification mode); (2) otherwise: `body ⇐ σᵣ` (checking mode). Update prose at line 146 to describe the conditional and the rationale (TypeVars are not ground; `is_subtype` cannot bind them). (`doc/06-type-inference.md:135-146`) [Major, type-theorist + computer-scientist C66]
- [x] Fix `invoke_proxy_handler` uses `stdlib_env` as `default_env` — third, undocumented behavior — `eval.rs:1002` uses `ctx.config.stdlib_env` as the `default_env` for proxy handler optional params. `eval_call` uses caller's `env`; `builtin_apply` uses closure `env`; `invoke_proxy_handler` uses `stdlib_env`. A proxy handler with default params referencing closure variables gets `UndefinedVariable` instead of the expected value. Either (a) align with `$apply`: use handler's `closure_env` as `default_env`, or (b) document the choice in `doc/08-evaluation.md` alongside proxy forcing rules. (`src/eval.rs:1002`) [Minor, eval-engine + computer-scientist C66]

### computer-scientist-c67: Theoretical Soundness Findings (Cycle #67)

Findings from Cycle #67 full codebase health review (computer-scientist). Focus: HM inference (Algorithm W), Remy row types, Launchbury sharing, evaluation semantics.

- [x] Fix `doc/06-type-inference.md:131` [FN] rule references nonexistent "Limitation #5" — variadic param limitation is Limitation #4 (only 4 limitations exist, numbered 1-4). Changed `Limitation #5` to `Limitation #4`. (`doc/06-type-inference.md:131`) [Nit, computer-scientist C67]
- [x] Fix `doc/06-type-inference.md:350` S-ANY-TOP/S-ANY-BOT note cross-references wrong limitation — Any-as-top-and-bottom is Limitation #2, not #3 (Limitation #3 is about forward reference monomorphism, unrelated to Any subtyping). Changed `Limitation #3` to `Limitation #2`. (`doc/06-type-inference.md:350`) [Nit, computer-scientist C67]

### grammar-architect-c67: Parser and Grammar Findings (Cycle #67)

New findings from Cycle #67 full codebase health review (grammar-architect). All items independent.

- [x] Fix `doc/02-syntax.md` §8 claims dot/bracket access "desugars to `$get`" — false

### grammar-architect-c68: Parser and Grammar Findings (Cycle #68)

New findings from Cycle #68 full codebase health review (grammar-architect). All items independent.

- [x] Fix `variadic_param` allows space between `...` and name (non-atomic) while `rest_entry` is atomic — changed from `!{}` (non-atomic) to `@{}` (atomic). Parser updated to extract name from raw text. Doc/02, doc/04 grammar listings updated. (`src/grammar.pest:107`, `src/parser.rs:540-563`, `doc/02-syntax.md:488,782`, `doc/04-functions.md:77`) [Minor, grammar-architect C68]
- [x] Fix `doc/15-ast.md` NamedArg struct omits `$`-stripping behavior of `name` field — added prose note documenting that both `key: val` and `$key: val` produce `NamedArg { name: "key", ... }`. (`doc/15-ast.md:114-117`) [Minor, grammar-architect C68]
- [x] Fix `TODO.md` grammar-architect-c66 nit target for `ast.rs:128` is doubly stale — corrected existing item to reference `typecheck.rs:1131`. (`TODO.md`) [Nit, grammar-architect C68] — the section header "Key-based access (brackets and dot — desugars to `$get`)" and every example annotation `# -> [call $get $person name]` imply a syntactic desugaring pass. The evaluator handles `DotAccess` and `BracketAccess` AST nodes directly via `eval_dot_access()` (`src/eval.rs:1027`) and `eval_bracket_access()` (`src/eval.rs:1058`) — no desugaring occurs. `$get` is a stdlib convenience wrapper, not the implementation mechanism. `doc/03-data-model.md:128-129` also uses `→` arrows implying desugaring. Fix: change §8 header to "semantically equivalent to `$get`"; replace `# -> [call $get ...]` annotations with equivalent descriptions; update `doc/03-data-model.md:128-129` to use `≡` or "equivalent to" phrasing. (`doc/02-syntax.md:947-952`, `doc/03-data-model.md:128-129`) [Minor, grammar-architect C67]

### cycle-findings-c69-a: Critical and Major Bug Fixes (Cycle #69)

- [x] Fix Guarded thunk stuck in InProgress on non-cacheable error — `take_guarded()` leaves thunk in InProgress when inner materialization fails with a non-cacheable error; next access produces spurious CircularDependency. Added `else { thunk.set_state(ThunkState::Guarded { ... }); }` in the non-cacheable branch. (`src/eval.rs:1559-1572`) [Critical, computer-scientist C48]
- [x] Fix `deep_materialize_thunk` stale cache sentinel on error — `None` blackhole sentinel left permanently in cache when `materialize` fails; subsequent Rc-shared encounters return the unforced thunk. Added `cache.remove(&thunk_ptr)` in error paths. (`src/eval.rs:1668-1683`) [Major, eval-engine C52]
- [x] Fix LSP missing `desugar_file` call before typecheck and eval — `DocumentState::new()` pipeline was parse→typecheck→eval without desugaring; user code with `$_` saw un-desugared ASTs in LSP. Added `desugar_file` call. (`src/lsp/document.rs`) [Minor, computer-scientist C32]
- [x] Fix `doc/16-architecture.md` pipeline diagram missing Desugar step — added Desugar step between Parse and Typecheck in the pipeline ASCII diagram. (`doc/16-architecture.md`) [Minor, integration-verifier C69]
- [x] Fix `is_subtype` RowVar/Empty soundness — open record (RowVar rest) was incorrectly considered a subtype of a closed record (Empty rest); a RowVar rest may carry additional fields violating the closed record constraint. Fixed per Rémy (1994). (`src/types.rs:129-136`) [Major, type-theorist C69]
- [x] Fix CALL-POLY arg types applied through `state.subst` — arg types at CALL-POLY sites were not applied through `state.subst` before unification, causing stale TypeVar comparisons. Added `state.subst.apply()` at argument and return-type sites. (`src/typecheck.rs:887-892, 1018-1021`) [Major, type-theorist C69]
- [x] Fix `check_call` `state.subst.apply` missing `is_empty` fast-path — `Substitution::apply` was called unconditionally even when the substitution was empty; added `is_empty` guard at `src/typecheck.rs:916`. (`src/typecheck.rs:916`) [Minor, performance-expert C69]

### misc-nits-a: Source Code Nits (Part 1)

Simple Nit-level source code fixes: comment corrections, dead code removal, pattern consolidation. No behavior changes.

- [x] Fix `typecheck_document` redundant double match on `Expr::Dict` — outer `if matches!(&expr.node, Expr::Dict(_))` immediately followed by inner `if let Expr::Dict(entries) = &expr.node`. Collapse to single `if let`. (`src/typecheck.rs:84-85`) [Nit, integration-verifier C40]
- [x] Document `unify_remainders` fallback `unreachable!()` invariant — `src/types.rs:755` contains `unreachable!("unify_remainders: no matching case")` that is dead by invariant (all 7 pattern cases are exhaustive over `(u1_empty, tail1, u2_empty, tail2)`); add a comment before the arm explaining the exhaustiveness argument to prevent future contributors from adding error recovery there. (`src/types.rs:755`) [Nit, security-expert C52]
- [x] Add `checked_add` to `auto_index` in `eval_dict` for consistency — `builtin_append` uses `checked_add` for same kind of integer key computation; `auto_index += 1` is unchecked. Overflow is unreachable (memory exhaustion first) but inconsistent. (`src/eval.rs:331`) [Nit, computer-scientist]
- [x] Remove redundant `debug_assert!(depth <= MAX_PARSE_DEPTH)` — line 182 is compiled out in release builds; the `if depth >= MAX_PARSE_DEPTH` on line 183 is the actual runtime check. (`src/parser.rs:182-183`) [Nit, grammar-architect]
- [x] Fix `deep_materialize` docstring cross-reference — line 1206 says "see `deep_materialize_impl` for the dual-purpose cache semantics" but the cache protocol (blackhole sentinel + sharing preservation) is implemented in `deep_materialize_thunk`; `deep_materialize_impl` only threads `cache` through as a parameter. (`src/eval.rs:1205-1206`) [Nit, seq-cycle-fix panel]
- [x] Fix cache-semantics docstring placement — "The `cache` serves two purposes: 1. Cycle detection ... 2. Sharing preservation ..." block at lines 1215-1220 is placed on `deep_materialize_impl` but describes behavior that lives entirely in `deep_materialize_thunk`. Move docstring to `deep_materialize_thunk`. (`src/eval.rs:1213-1220`) [Nit, seq-cycle-fix panel]
- [x] Fix redundant `use std::collections::HashMap` inside `deep_materialize` body at `src/eval.rs:1208` — leftover artifact from HashSet→HashMap refactor; already imported at module level (line 6). Remove the local `use`. [Nit, eval-engine C43]
- [x] Add inline comment to `deep_materialize_thunk` explaining depth/depth+1 asymmetry — `materialize(thunk, None, ctx, depth)` uses current depth while `deep_materialize_impl(&v, ctx, depth + 1, cache)` increments; asymmetry is correct because `materialize` guards itself, but confusing next to the `+1`. (`src/eval.rs:1274-1275`) [Nit, eval-engine C43]

### error-infra-nits: Error Nits

Nit-level error infrastructure cleanup.

- [x] Fix `ArityBound::Exact(1)` displaying "1 arguments" — grammatically incorrect singular; add singular/plural logic to ArityBound Display (`src/error.rs:21`) [Minor, computer-scientist]
- [x] Fix materialize depth check message duplicating constant (`src/eval.rs:812-820`) [Nit, eval-engine] — already fixed, no change needed
- [x] Simplify `EvalError::new` parameter from `impl Into<String>` to `String` (`src/error.rs:56-79`) [Nit, span-integrity-checker]
- [x] Standardize error category names (`src/error.rs:56+`) [Nit, span-integrity-checker] — already consistent, no change needed
- [x] Fix `from_json` inconsistent `.into()` usage — some error paths use `.into()` for boxing while adjacent paths use explicit `Box::new()`; standardize for consistency (`src/builtins.rs:984`) [Nit, computer-scientist] — already consistent, no change needed

### error-infra-nits-b: Error Nits (Part 2)

Nit-level error infrastructure cleanup continued.

- [x] Fix `ErrorKind` variant count assertion fragility — added `all_error_kind_variants()` helper to centralize variant list; eliminated hardcoded count assertions. (`src/error.rs`) [Nit, span-integrity-checker T4]
- [x] Standardize all error constructor parameter signatures — converted `new`, `internal`, `with_frame`, `push_frame` and 14+ other constructors from `impl Into<String>` to `String`; updated ~70 call sites. (`src/error.rs:540+, src/builtins.rs`) [Nit, integration-verifier C70 panel]
- [x] Add singular ArityMismatch case to `test_error_kind_display_all_variants` — added `Exact(1)` test case producing "expected 1 argument, got 0". (`src/error.rs`) [Nit, test-crafter C70 panel]
- [x] Review PendingBuiltin error path span handling — reviewed and added clarifying comment; behavior is correct (dedup logic in attach_materialization_context). (`src/eval.rs`) [Nit, span-integrity-checker]
- [x] Fix `checked_f64_to_i64` out-of-range branch still using `EvalError::new` — migrated to `EvalError::integer_overflow` (E032 instead of E099). (`src/builtins.rs:71`) [Nit, span-integrity-checker C31]

### cycle-findings-c31-major: Major Code Fixes (Cycle #31)

- [x] Fix `resolve_type_expr_value` only handles bare type names, not composite types — `src/typecheck.rs:1401-1415` only handles `Expr::Str` and `Expr::VarRef`; any composite type in a `@[type: X default: Y]` annotation (e.g., `@[type: [Fn@Number [Int]] default: 0]` or `@[type: [name: String] default: ...]`) returns an error "invalid type in annotation", silently rejecting expressions doc/05 shows as valid. Fix: replace body with a call to `resolve_type_expr(expr, env, state, ann_mapping)` which already handles `Dict` and `Annotated` nodes. Add tests for the composite-type patterns. (`src/typecheck.rs:1401-1415`) [Major, type-theorist C31]
- [x] Fix `infer_fn` allocates `HashMap<String, String>` for every function literal including unannotated lambdas — `src/typecheck.rs:1090` allocates `HashMap::new()` for `ann_mapping` on every `[fn ...]` expression; the most frequent case (`[fn [x] $x]` passed to `$map`/`$filter`) has no annotations and never populates the map. Fix: add a guard — if all params have `annotation.is_none()` and `return_ann.is_none()`, skip the allocation (use `None` for `ann_mapping_opt`). (`src/typecheck.rs:1090`) [Major, performance-expert C31]
- [x] Fix `resolve_type_dict` loses polymorphic schemes at non-Dict document boundaries — `src/typecheck.rs:118-122` in `typecheck_document` inserts non-last expressions into the environment as `TypeScheme::mono` via `new_env.insert(...)`. The Dict literal path correctly calls `insert_scheme()` to preserve generalized schemes. Any non-literal-dict expression returning a Record (e.g., `[call $make-record args]`) will have its fields deschemified at the document boundary. Fix: call `insert_scheme()` or extract schemes from the record type for non-Dict paths. (`src/typecheck.rs:118-122`) [Major, type-theorist C31]
- [x] Fix `builtin_filter_seq_step` predicate mismatch error using `EvalError::new` → E099 — migrated to `EvalError::type_mismatch_ctx("filter", "Bool", ...)` (E010 instead of E099). (`src/builtins.rs:2082`) [Minor, span-integrity-checker C39]
- [x] Fix `value_to_json` float NaN/Infinity using `EvalError::new` → E099 — migrated Float path to `EvalError::float_not_finite` (E033) and depth path to `EvalError::depth_exceeded` (E040). (`src/lib.rs:148-157`) [Minor, eval-engine C39]
- [x] Fix `builtin_include` wrapping result with `Span::origin()` — now uses `thunk.span` from eval_file result (included file's root expression span). (`src/builtins.rs:1124`) [Minor, eval-engine + span-integrity-checker C39]

### cycle-findings-c70-a: Critical, Major, and Code Findings (Cycle #70)

Critical and major bugs plus code quality issues from Cycle #70 full codebase health review. All items independent unless noted.

- [x] Fix `$join` Seq path has no iteration size cap — `src/builtins.rs:2450-2480`: the loop accumulates element strings into `parts: Vec<String>` without a bound; `[call $join "," [call $range 0]]` loops until OOM with no depth protection (iterative, not recursive). `$collect` has `MAX_COLLECT_SIZE = 1_000_000`; `$join` has neither. Fix: add `MAX_JOIN_PARTS: usize = 1_000_000` constant, check `parts.len() >= MAX_JOIN_PARTS` before each push, return `EvalError::internal` if exceeded. Add corpus test `tests/corpus/eval/errors/join_size_limit.llt-eval`. (`src/builtins.rs:2450-2480`) [Major, security-expert + eval-engine C70] — fixed in cycle-findings-c70-a sprint; unit test added (corpus test impractical at 1M elements)
- [x] Fix `doc/10-errors.md` variant count prose triply inconsistent — line 98 says "26 `ErrorKind` variants", line 776 says "All 26 `ErrorKind` variants", line 809 says "The 27 variants above are exhaustive"; the actual count is 28 (verified by `test_error_kind_code_exhaustiveness` assertion at `src/error.rs:1537`). Fix: update all three lines to "28". Verify the Part 1 variant catalog includes `IncludeForbidden`. (`doc/10-errors.md:98, 776, 809`) [Major, integration-verifier C70] — fixed in cycle-findings-c70-a sprint
- [x] Fix `doc/16-architecture.md` `EvalConfig` sketch missing `no_fs: bool` field — struct block at lines 49-55 shows `base_dir`, `stdlib_env`, and `// future: sandbox_policy` but omits `no_fs: bool` which already exists at `src/eval.rs:28`. The future comment is also stale (`no_fs` IS the sandbox flag). Fix: add `no_fs: bool,` and update comment to `// future: allowed_paths (cap-std include-fd-hardening sprint)`. (`doc/16-architecture.md:49-55`, `src/eval.rs:26-31`) [Major, integration-verifier C70] — fixed in cycle-findings-c70-a sprint
- [x] Fix `eval.rs` constructs 3 error types via raw inline struct literal bypassing named constructors — `eval.rs:626` (`DuplicateKey`), `eval.rs:900` (`NamedArgConflict`), `eval.rs:915` (`UnknownNamedArg`) each use `Box::new(EvalError { kind: ..., definition_span: ..., materialization_span: None, stack: Vec::new() })` directly instead of `EvalError::duplicate_key()`, `EvalError::named_arg_conflict()`, `EvalError::unknown_named_arg()` (named constructors exist in `error.rs:634, 645, 654`). Fix: replace all three inline constructions with named constructor calls. (`src/eval.rs:626, 900, 915`, `src/error.rs:634, 645, 654`) [Major, integration-verifier C70] — fixed in cycle-findings-c70-a sprint
- [x] Fix `.claude/agents/grammar-architect.md` Known Constraint #3 factually wrong — constraint reads "Positional-before-named constraint: parser enforces SPEC Section 5.1" but `grammar.pest:50` defines `call_args = { (named_arg | value)* }` with no ordering constraint; `parser.rs` has zero ordering checks; `doc/15-ast.md:164-170` explicitly documents free interleaving. Fix: rewrite as "No positional-before-named constraint at parse time — the parser allows any ordering; the C-PRIORITY evaluator binding chain (doc/04-functions.md §Call Convention) is the only ordering rule and is enforced at eval time." (`.claude/agents/grammar-architect.md`) [Major, grammar-architect C70] — fixed in cycle-findings-c70-a sprint
- [x] Fix `func_label`/`func_path` allocates owned String on every call — `src/eval.rs:685-694`: `func_label()` unconditionally calls `func_path()` which returns `Cow::Owned(format!(...))` for every VarRef arm (the dominant `[call $f ...]` pattern); the label is then cloned into `CallContext.origin`. Fix: in `func_label`, match `Expr::VarRef(name)` first and return `Cow::Owned(format!("call ${name}"))` to skip `func_path()` for the common case; defer `label.clone()` at `eval.rs:757` to inside the `map_err` closure (only needed on error path). (`src/eval.rs:685-694, 757`) [Major, performance-expert C70] — fixed in cycle-findings-c70-a sprint
- [x] Fix `json_to_value` Array and Object paths missing capacity hints — `let mut map = IndexMap::new()` at `src/builtins.rs:956` (Array path) and line 967 (Object path) allocate without size hints; array/object lengths are known before the loop. Fix: change to `IndexMap::with_capacity(arr.len())` and `IndexMap::with_capacity(obj.len())`. (`src/builtins.rs:956, 967`) [Major, performance-expert C70] — fixed in cycle-findings-c70-a sprint
- [x] Fix `check_call` and `check_call_with_scheme` skip positional arg inference when `func_ty` is `Type::Any` — `src/typecheck.rs:1043, 903`: when the callee is untyped, named args ARE inferred (before the match) but positional args bypass `infer_expr` entirely, leaving the type_map empty for their spans. LSP hover on positional args in calls to untyped functions shows nothing. Fix: add `for a in args { let _ = infer_expr(a, env, state)?; }` before the `match &func_ty` block in both functions. (`src/typecheck.rs:903, 1043`) [Major, computer-scientist C70] — fixed in cycle-findings-c70-a sprint; positional-arg loop now correctly scoped to Type::Any arm only

### computer-scientist-c71: Theoretical Soundness Findings (Cycle #71)

New findings from Cycle #71 full codebase health review (computer-scientist). Focus: Kiselyov (2013) levels invariants, Algorithm W substitution threading, doc/06 spec accuracy.

- [x] Fix `resolve_type_name` unconditionally resets annotation TypeVar level — Kiselyov (2013) monotonicity violation — `src/typecheck.rs:1355` executes `state.levels.insert(fresh_name.clone(), state.level)` on EVERY call for a previously-mapped annotation name, not just on first creation. If unification lowered the level of the TypeVar between two references to the same annotation name (e.g., via U-VAR-LEVEL during lambda checking mode at line 365), the second call resets the level to `state.level`, potentially un-lowering it. This violates Kiselyov's invariant that level lowering is monotone (levels can only decrease, never increase). A level reset could cause a TypeVar to be spuriously generalized, producing a polymorphic scheme where monomorphism is required. Fix: only set level on first creation — split into `if let Some(existing) = mapping.get(name)` (return existing with current level from `state.levels`) vs `else` (call `state.fresh_type_var()` and insert into mapping). (`src/typecheck.rs:1349-1356`) [Major, computer-scientist C71]
- [x] Fix `doc/06-type-inference.md` check_expr pseudocode (lines 57-62) diverges from implementation — pseudocode shows bare `is_subtype(&actual, expected)` but implementation (`src/typecheck.rs:459-461`) applies `state.subst.apply()` to both types before comparison; also omits lambda checking mode dispatch. Partially tracked in C60 and type-theorist-c67 items; consolidated here for completeness. (`doc/06-type-inference.md:57-62`) [Minor, computer-scientist C71]

### proxy-followup: Proxy Deferred Items (Cycle #61 panel overflow)

Low-priority follow-on work from the overridable-ops panel review.

- [x] Add `Type::Proxy` variant so type checker can reject proxy-valued non-access uses rather than returning `Any`; proxy field access will remain `Any` (handler result is opaque) (`src/types.rs`, `src/typecheck.rs`) [Minor, integration-verifier C61]
- [x] Migrate `value_to_json` Proxy error from `EvalError::new` (E099 Internal) to a typed `ErrorKind::ValueNotSerializable { value_type }` (`src/lib.rs:209`, `src/error.rs`) [Minor, integration-verifier C61]
- [x] Add proxy-to-JSON and proxy-display to `deep_materialize_impl` — currently falls through catch-all without traversing `handler` thunk; add `Value::Proxy { handler }` arm that clones the proxy but deep-materializes handler thunk (`src/eval.rs:1635`) [Minor, computer-scientist C61]
- [x] Proxy bracket access: pass original `Value::Int`/`Value::String` to handler instead of always flattening to String — lets handler distinguish `$p[0]` from `$p["0"]` (`src/eval.rs:1081-1085`) [Minor, computer-scientist C61]
- [x] Expand `builtin_aliases_callable.llt-eval` to cover all 12 `builtin-*` aliases — currently covers only 6; add direct calls to `builtin-sub`, `builtin-div`, `builtin-mul`, `builtin-reduce`, `builtin-take`, `builtin-drop` (`tests/corpus/eval/builtins/`) [Nit, test-crafter C61]
- [x] Add proxy 2-arg arity error corpus test — `[call $proxy f1 f2]` should error with arity mismatch (`tests/corpus/eval/errors/`) [Nit, test-crafter C61]
- [x] Fix `test_proxy_invoke_depth_limit` comment — says depth fires in `invoke_proxy_handler` but actually fires earlier in `eval(target, ..., depth+1)` during VarRef resolution (`src/eval.rs:3567-3573`) [Nit, eval-engine C61]
- [x] Empty `IndexMap::new()` in `invoke_proxy_handler` (2 sites) — use `Lazy<IndexMap<...>>` or pass empty slice to avoid heap allocation per proxy field access (`src/eval.rs:1001,1011`) [Nit, performance-expert C61]

### computer-scientist-c31: Row Variable Level Monotonicity (Cycle #31)

Row variable level monotonicity violations in `resolve_type_dict` — exact same bug class as the C71 `resolve_type_name` fix. Both items independent.

- [x] Fix `resolve_type_dict` named row variable level reset in function scope — `src/typecheck.rs:1474` does `state.levels.insert(fresh_name, state.level)` unconditionally after `or_insert_with`, even when the row variable was already mapped. If unification lowered the row variable's level between first and second reference, this resets it upward, violating Kiselyov (2013) monotonicity invariant (levels can only decrease). Fix: split into existing-mapping path (read current level from `state.levels` via `.get().expect()`, no insert) and new-mapping path (insert at `state.level`), matching the C71 fix pattern in `resolve_type_name` lines 1365-1381. (`src/typecheck.rs:1468-1475`) [Major, computer-scientist C31]
- [x] Fix `resolve_type_dict` named row variable level reset outside function scope — `src/typecheck.rs:1477` does `state.levels.insert(n, state.level)` unconditionally. Fix: check `state.levels.get(n)` first; only insert if absent, matching the C71 fix pattern in `resolve_type_name` lines 1385-1390. (`src/typecheck.rs:1476-1479`) [Major, computer-scientist C31]

### error-infra-nits-b: Error Nits (Part 2, Continued)

Fix-later items from the error-infra-nits-b sprint panel review.

- [x] Fix `all_error_kind_variants()` doc comment claiming "compile-time" exhaustiveness — fixed to accurately say "runtime, not compile-time". (`src/error.rs`) [Nit, computer-scientist C70 panel]
- [x] Fix `ArityBound::AtMost(1)` still displays "at most 1 arguments" — already correct (singular form was present). (`src/error.rs:23-24`) [Nit, integration-verifier C70 panel]
- [x] Fix PendingBuiltin comment referencing absolute line numbers — replaced with function names (`materialize()`, `attach_materialization_context()`). (`src/eval.rs:1291-1293`) [Nit, computer-scientist C70 panel]
- [x] Restore float value in `checked_f64_to_i64` overflow error message — formatted float into op string: `format!("{name}: {f} is out of i64 range")`. (`src/builtins.rs:71`) [Nit, computer-scientist C70 panel]

### misc-nits-b: Miscellaneous Nits (Part 2)

Simple code comment/doc nits from codebase reviews.

- [x] Document type alias entries returning empty dict at runtime (`src/eval.rs`) [Minor]
- [x] Fix `eval_source()` relative PathBuf comment (`src/lib.rs`) [Nit]
- [x] Fix `EvalError::Display` mat_span suppression — skip printing when equals definition_span (`src/error.rs`) [Minor]
- [x] Fix `ArityBound::Exact(1)` displaying "1 arguments" (`src/error.rs`) [Nit]
- [x] `unreachable!()` in `unescape` unknown escape fallback (`src/parser.rs`) [Nit]
- [x] Rename `zip-seq`/`zip-dict` → `zip-seq-impl`/`zip-dict-impl` (`stdlib/prelude.llt`) [Nit]
- [x] Rename `quot_div_by_zero.llt-eval` → `division_by_zero.llt-eval` (`tests/corpus/eval/errors/`) [Nit]
- [x] Fix stale DESIGN.md line range in `src/desugar.rs:7` docstring [Nit]
- [x] Fix `grammar.pest` comment capitalization inconsistency [Nit]
- [x] Fix `ident_char` comment at `grammar.pest:79` [Nit]

### test-framework: Test Framework Enhancements (completed items)

- [x] Extend error test framework: support `=== ERROR: substring` for message validation
- [x] Add `tests/corpus/eval/regressions/` directory for regression tests
- [x] Add cross-feature interaction tests (`tests/corpus/eval/cross_feature/`)
- [x] Rename builtin tests — convention is descriptive names without `test_` prefix; 5 outliers renamed
- [x] Add unit tests for `split_test_file()` (11 tests)
- [x] Add unit tests for `has_error_code_prefix()` (10 tests)
- [x] Fix `has_error_code_prefix()` hardcoded 3-digit window — documented with doc comment
- [x] Add `tests/corpus/eval/access/` directory with 3 corpus tests
- [x] Fix `test_corpus_structure` missing required dirs
- [x] Move flat-root eval corpus tests into subdirectories (functions/, underscore/)
- [x] Update valid corpus test format documentation

### misc-nits-d: Miscellaneous Nits (Part 4)

- [x] Fix `resolve_type_assert` pre-substitution return — used `expected_resolved` for default validation; fixed `infer_dict` Pass 3a subst initialization
- [x] Fix `doc/11-stdlib.md` threading code block `$reduce` → `$builtin-reduce`
- [x] Fix prelude header comment: primary operators → `builtin-*` aliases
- [x] Fix `doc/11-stdlib.md` `$merge` "Lazy overlay" → "Materializing"
- [x] Removed hardcoded variant count assertions in error.rs tests
- [x] Fix `check_dot_access` debug_assert redundant `state.levels` lookup
- [x] Add `test_value_matches_type_proxy` unit test
- [x] Add `test_deep_materialize_proxy` unit test
- [x] Document `invoke_proxy_handler` Builtin path `empty.clone()` necessity

### cycle-findings-c32-a: Major Findings (Cycle #32)

Major findings from Cycle #32 full codebase health review. All items independent.

- [x] Fix `$replace` output size amplification — `builtin_replace` at `src/builtins.rs:577` calls `input.replace(pattern, &replacement)` with no output size guard. An empty-string pattern inserts `replacement` between every character: for a 10MB input and 10MB replacement string, the requested allocation is ~100 TB. Fix: add `const MAX_STRING_SIZE: usize = 64 * 1024 * 1024` alongside `MAX_COLLECT_SIZE`; compute `match_count` (via `str::matches` or `input.chars().count() + 1` for empty pattern), check that output_len ≤ MAX_STRING_SIZE before calling `str::replace`. Return `EvalError::internal(...)` if exceeded. (`src/builtins.rs:557-578`) [Major, security-expert C32]
- [x] Fix `eval_range_access` doesn't handle `Proxy` values — `eval_dot_access` and `eval_bracket_access` both call `invoke_proxy_handler` when the target materializes to `Value::Proxy`. `eval_range_access` calls `eval_as_dict` instead, producing a confusing "type mismatch: expected Dict, got Proxy" error with no indication that range access is unsupported on proxies. Fix: add a `Value::Proxy` arm in `eval_range_access` that either invokes the proxy handler with a range representation, or returns a clear "range access is not supported on Proxy" error. (`src/eval.rs:1121-1125`) [Major, integration-verifier C32]
- [x] Add corpus test for `MissingRequiredParam` (E024) — added `tests/corpus/eval/errors/missing_required_param.llt-eval` asserting `[E024]`. (`tests/corpus/eval/errors/`) [Major, test-crafter C32]
- [x] Add corpus test for `ValueNotSerializable` (E035) — added `tests/corpus/eval/errors/proxy_handler_error.llt-eval` with architectural limitation documented: E035 only emitted via CLI JSON path. (`tests/corpus/eval/errors/`) [Major, test-crafter C32]
- [x] Fuse `unify()` U-VAR arms to use single tree walk — added `collect_all_vars` helper; replaced both call pairs in U-VAR-L and U-VAR-SYM with single traversal; also used in `lower_row_var_levels`. (`src/types.rs:945-984`) [Major, performance-expert C32]
- [x] Fix `infer_dict` allocates fresh `Substitution` — investigated: false premise; `IndexMap::new()` already has zero-cap; added doc comment to `Substitution::new()` confirming this. (`src/types.rs`) [Major, performance-expert C32]
- [x] Fix `doc/08-evaluation.md` `$apply` Laziness Design table — updated row to match strictness table ("Materializes function + arg dict; splits by key type; invokes"). (`doc/08-evaluation.md:828`) [Major, eval-engine C32]

### docs-vs-code-syntax: doc/02-syntax.md vs Code Accuracy

Split from docs-vs-code. All items target doc/02-syntax.md. Docs-only sprint (no build gate or panel review).

- [x] **doc/02-syntax.md `bare_word_char` prose terminator list missing `$`** [Nit, grammar-architect]
- [x] **doc/02-syntax.md §access chains missing dot exclusion clarity** [Major, grammar-architect]
- [x] **doc/02-syntax.md `annotation_value` comment doesn't reference parent rule** [Nit, grammar-architect]
- [x] **doc/02-syntax.md intro describes parser only, not eval semantics** [Nit, integration-verifier]
- [x] **doc/02-syntax.md call/dict disambiguation colon lookahead only matches horizontal whitespace** [Major, grammar-architect]
- [x] **doc/02-syntax.md or doc/03-data-model.md duplicate key detection contradicts parser** — already accurate [Major, grammar-architect]
- [x] **doc/02-syntax.md tree-sitter divergence note references nonexistent grammar** — tree-sitter-llt/grammar.js exists; finding was incorrect [Major, grammar-architect]
- [x] **doc/02-syntax.md token precedence section missing structural punctuation** [Minor, grammar-architect]
- [x] **doc/02-syntax.md whitespace significance section lacks lexer cross-reference** [Minor, grammar-architect]
- [x] **doc/02-syntax.md access chains and complete grammar sections need dual parser/lexer implementation notes** [Minor, grammar-architect]
- [x] **doc/02-syntax.md `---` syntax section missing cross-reference to evaluation semantics** [Minor, integration-verifier]
- [x] **doc/02-syntax.md dot-in-bare-word pest/tree-sitter divergence lacks rationale** [Critical, grammar-architect]
- [x] **doc/02-syntax.md unrecorded decision: dot excluded from `bare_word_char` in tree-sitter** [Critical, grammar-architect]
- [x] **doc/02-syntax.md Tokenization Rules section missing `---` document separator** — already present [Major, grammar-architect]
- [x] **doc/02-syntax.md Tokenization Rules tables have inconsistent column headers** [Nit, grammar-architect]
- [x] **doc/02-syntax.md "Bare Word Character Set" header doesn't follow section naming pattern** — already correct [Nit, grammar-architect]
- [x] **doc/02-syntax.md §Literal Recognition references "tokenizer" but should reference "lexer"** [Major, grammar-architect]

## doc-type-polish: Type Inference Documentation Accuracy (C47)

Missing rules, misleading claims, and undocumented behaviors in type inference docs. Found by type-theorist, span-integrity-checker, and computer-scientist C47. Full workflow (build gate + panel review).

- [x] Add [CHECK-FN] rule to `doc/06-type-inference.md` — fourth checking position now documented; checking positions table updated [Major, type-theorist C47]
- [x] Document `TypeVar` in TypeAssert expected type always failing `is_subtype` — added as Limitation in doc/06 [Major, type-theorist C47]
- [x] Fix `doc/16-architecture.md` REPL EvalContext description — single context per session, include state persists [Major, integration-verifier C47]
- [x] Fix `doc/06-type-inference.md` "pure Robinson" claim — "extended with pragmatic promotion rules"; line 317 contradiction also fixed [Minor, computer-scientist C47]
- [x] Fix stale `TypeScheme` struct block — updated to type_vars/row_vars split throughout doc/06 [Major, type-theorist + computer-scientist C49]
- [x] Fix variadic param type inconsistency in `infer_fn` — param_types[i] now matches env binding (Record({}, Empty)) [Major, type-theorist C49]
- [x] Add precedence note to `[U-SUBSUME]` rule — fires after structural rules, not as catch-all [Nit, type-theorist C52]
- [x] Fix `doc/15-ast.md` Node Semantics table missing `Rest` variant [Major, grammar-architect C49]
- [x] Document `semicolon` rule discrepancy — phantom named rule removed from doc/02-syntax.md [Major, grammar-architect C49]
- [x] Document `colon_ahead` horizontal-whitespace-only behavior — already done in docs-vs-code-syntax sprint [Minor, grammar-architect C49]
- [x] Fix `doc/04-functions.md` incorrect claim defaults evaluated eagerly — corrected to lazy thunks [Minor, eval-engine C49]
- [x] Rename `ret` to `inst_ret` in `check_call_with_scheme` [Nit, type-theorist C49]
- [x] Fix `typecheck_document` non-dict path — added LIMITATION comment [Minor, type-theorist C49]

## doc-11-stdlib: doc/11-stdlib.md Accuracy Overhaul (C47)

Stale snippets, misclassifications, and missing documentation in doc/11-stdlib.md. Found by stdlib-author C47. Full workflow (build gate + panel review, 5×APPROVE).

- [x] Fix `doc/11-stdlib.md` stale `empty?` implementation snippet [Minor, stdlib-author C47]
- [x] Fix `doc/11-stdlib.md:96` `length` and `empty?` misclassified as "Structural" [Nit, stdlib-author C47]
- [x] Fix `doc/11-stdlib.md:277` `words` description omits that it returns Seq [Nit, stdlib-author C47]
- [x] Fix `doc/11-stdlib.md` `reduce` and `fold` misclassified as Lazy — reclassified as "Selective" (Dict path lazy, Seq path materializing) [Nit, stdlib-author C49]
- [x] Fix `doc/08-evaluation.md:768` `$merge` Laziness Design table — restored accurate eager-materialization description with note re merge-lazy-overlay sprint [Major, laziness-auditor C47]
- [x] Add `any?`/`all?` Seq guard at `stdlib/prelude.llt:60,71` + corpus error tests with [E080] [Minor, stdlib-author C47]
- [x] Add `flatten_mixed.llt-eval` corpus test for mixed scalar/nested flatten [Nit, stdlib-author C47]
- [x] Document `from-entries` accepting Seq inputs + `from_entries_seq.llt-eval` corpus test [Nit, stdlib-author C47]

### doc-rowunification-retrospective: Row Unification Doc Retrospective (Parts 1–2)

Consolidated from doc-rowunification-retrospective and doc-rowunification-retrospective-b. Docs-only sprint (no build gate or panel review).

- [x] Fix doc/07 Part 4 stale "must be split" — already correct [Major, grammar-architect C53]
- [x] Rewrite doc/07 Part 8 as "Migration Reference (Complete)" in past tense [Major, grammar-architect C53]
- [x] Fix doc/07 Roadmap stale binding claim → "complete as of row-unification-e" [Major, grammar-architect C53]
- [x] Fix README.md RowRest → RowTail, updated types.rs description [Major, grammar-architect C53]
- [x] Fix doc/07 Part 9 §P8 disjointness qualified with "at unification time" [Minor, type-theorist C53]
- [x] Fix $or doc return-value description — pass-through, not literal true [Minor, stdlib-author C53]
- [x] Add Wand (1987) to doc/17 — already present [Minor, grammar-architect C53]
- [x] Reduce doc/07 Part 10 to cross-references to doc/17 [Nit, grammar-architect C57]
- [x] Fix doc/07 Cases 2/3 tautological when guards removed [Nit, computer-scientist C63]
- [x] Add Bernstein (2024) to doc/17; Gaster & Jones, Harper & Pierce already present [Major, grammar-architect C54]
- [x] Fix doc/11-stdlib.md overridable-ops: builtin-* prefixes, count 46, proxy entry, ~98 total functions [Major, stdlib-author C54]
- [x] Fix doc/07 unify_rows pseudocode — added Steps 3.5/3.6 [Minor, integration-verifier C54]
- [x] Fix doc/06 S-REC — rewrote using RowTail::Empty/RowVar, removed Open variant [Minor, computer-scientist C54]
- [x] Fix Moggi (1991) DOI in doc/17 [Minor, sprint-reviewer C57]
- [x] Fix doc/07 Part 5 qualifier → "complete as of row-unification-e" [Minor, grammar-architect C57]

### parser-doc-fixes: Parser and Syntax Doc Accuracy Fixes

Consolidated from parser-spec-fixes, parser-docs, doc-treesitter-fixes, doc-syntax-fixes. Full workflow (4-agent panel, 3 fix cycles on annotated_bare clarification).

- [x] Fix doc/02-syntax.md semicolon named-rule drift — already resolved by doc-type-polish [Minor, grammar-architect C52]
- [x] Document and test `$var`-prefixed named arg key stripping — doc/04 note, test_named_arg_with_dollar_key (equivalence + numeric), corpus test [Minor, grammar-architect C52]
- [x] Fix doc/04-functions.md self-reference — already correct [Nit, grammar-architect C60]
- [x] Fix tree-sitter fn_annotation to use token.immediate("@") [Major, grammar-architect C39]
- [x] Document tree-sitter bare_word `-` exclusion with comment [Minor, grammar-architect C39]
- [x] Fix doc/02-syntax.md semicolon rule divergence — already resolved by doc-type-polish [Major, grammar-architect C42]
- [x] Add annotated_bare to Token Precedence section — with atom-level context note [Minor, grammar-architect C42]
- [x] Fix parse_expression docstring — no scope chain built, parse-level only [Minor, computer-scientist]
- [x] Document key_to_string computed key detection as literal-keys-only [Minor, computer-scientist]

### docs-restructuring-refs: Documentation Cross-Reference Update

Bulk update of stale DESIGN.md/SPEC.md references after doc split. Full workflow (sprint-reviewer approved cycle 2).

- [x] Update systemic DESIGN.md/SPEC.md cross-references — bulk update across TODO.md (~50 refs), doc/whatif/ (43 refs), source comments (4 files) — verified complete [Major]
- [x] Fix doc/02-syntax.md:3 broken links [Minor, computer-scientist]
- [x] Fix doc/16-architecture.md EvalContext threading description [Major, computer-scientist + integration-verifier]
- [x] Fix doc/16-architecture.md Value sketch LinkedHashMap → IndexMap [Nit, computer-scientist]
- [x] Fix materialize() _ctx doc comment — Launchbury 1993 thunk-context invariant explained [Minor, computer-scientist]

### doc-eval-gaps: doc/*.md documentation gaps (eval-engine review)

Docs-only sprint (no build gate or panel review).

- [x] Letrec key parent scope justification — documented two-environment pattern, parent_env for keys, effectful expression context [Minor, eval-engine]
- [x] Cycle detection recovery strategy — documented InProgress→Failed transition, cache_failure() before propagation, thunk not restored [Minor, eval-engine]
- [x] deep_materialize cache semantics — documented dual-purpose HashMap (None=blackhole, Some=sharing), stack-local lifecycle, global per-call scope [Minor, eval-engine]
- [x] Materialization span semantics for PendingCall func error — documented call_span as mat_span for nested forcing, consistent with PendingBuiltin [Minor, eval-engine]

### stdlib-doc-a: Stdlib Documentation and Missing Reference Chapter

18 items (most already done by prior sprints in this session). Key new work: doc/11a-builtins.md, $deep-eq removal, any?/all? table, operator_wrappers corpus test.

- [x] Create doc/11a-builtins.md — new builtin reference chapter (46 builtins + 12 stable aliases) [Major]
- [x] Fix $deep-eq false claim — removed from doc/11-stdlib.md [Major]
- [x] Add any?/all? to Logic reference table in doc/11-stdlib.md [Major]
- [x] Fix concat description — dual dispatch documented (lazy Seq vs eager Dict) [Minor]
- [x] Add operator_wrappers.llt-eval corpus test covering all 12 wrapper functions [Major]
- [x] 13 items already done by prior sprints (function count, builtin count, alias names, proxy, concat, join, zip rename, comments, phantom names, wrapper table entries)

### docs-vs-code-functions-eval: doc/04, doc/08, doc/09, doc/10, doc/16 vs Code Accuracy

Split from docs-vs-code. Docs-only sprint (no build gate or panel review). 15 items.

- [x] Fix doc/16 pipeline diagram `desugar_underscores` → `desugar` — already correct [Minor]
- [x] Fix doc/10 error kind list exhaustiveness — qualified claim, added missing variants [Major]
- [x] Fix doc/04 call arity checking — noted as eval-time [Minor]
- [x] Fix doc/16 EvalContext section — thread-local INCLUDE_CTX status updated [Critical]
- [x] Fix doc/09 pipeline model code fence → indented block [Nit]
- [x] Fix doc/08 laziness tables future tense — already resolved by prior sprint [Minor]
- [x] Fix doc/16 BuiltinFn signature — BuiltinArgs struct documented [Minor]
- [x] Fix doc/09 include caching — expanded (PathBuf key, thread-local scope, error non-caching, lifetime) [Minor]
- [x] Fix doc/04 try_wrap sketch — updated to bool return, no depth [Major]
- [x] Fix doc/04 desugar_file sketch — Spanned<File> → File [Nit]
- [x] Fix doc/08 FORCE-BUILTIN/FORCE-CALL — added Σ_θ to all forcing rules [Minor]
- [x] Fix doc/04 WRAP-DICT pseudocode — unconditional recursion [Minor]
- [x] Fix doc/10 Part 9 is_cacheable() — "integration deferred" → "integrated" [Major]
- [x] Fix doc/10 Part 9 PROP-DEPTH — "integration deferred" → "integrated" [Major]
- [x] Fix doc/10 Part 9 EvalError line reference — updated to current line [Major]

### misc-nits-d: Miscellaneous Nits (Part 4)

9 items, all nits. 3 new fixes, 6 already done by prior sprints.

- [x] Fix resolve_type_assert pre-substitution expected — state.subst.apply added [Nit, type-theorist C68]
- [x] Fix doc/11 -> Threading code block — already done [Nit, stdlib-author C68]
- [x] Fix prelude header builtin-* aliases — already done [Nit, stdlib-author C66]
- [x] Fix doc/11 $merge lazy claim — already done [Nit, stdlib-author C65]
- [x] Fix hardcoded variant count in error tests — already done [Nit, integration-verifier C64]
- [x] Fix check_dot_access debug_assert redundant lookup — fixed [Nit, integration-verifier C64]
- [x] Add value_matches_type Proxy test — already done [Nit, test-crafter C70]
- [x] Add deep_materialize_impl proxy test — already done [Nit, test-crafter C70]
- [x] Optimize invoke_proxy_handler Builtin path — empty.clone() → IndexMap::new() [Nit, performance-expert C70]

### cycle-findings-c34-b: Minor Findings (Cycle #34)

Minor findings from Cycle #34 full codebase health review. All items independent.

- [x] Fix `stdlib/prelude.llt:7-17` header comment outdated — replaced with accurate 3-category taxonomy: shadowable wrappers (lines 527-544), stable builtin-* aliases, non-shadowed Rust builtins. (`stdlib/prelude.llt:7-21`) [Minor, stdlib-author C34]
- [x] Add `doc/02-syntax.md:714` canonical source declaration for Complete Grammar — added bidirectional cross-reference with `src/grammar.pest:2`. (`doc/02-syntax.md:717`, `src/grammar.pest:2`) [Minor, grammar-architect C34]
- [x] Add missing corpus tests for `concat`, `words`, `flatten` edge cases — added 8 new test files: `concat_dict_both_empty`, `concat_dict_empty`, `concat_seq_empty`, `flatten_deep_nesting`, `words_empty`, `words_only_spaces`, `words_leading_trailing`, `words_multiple_spaces`. (`tests/corpus/eval/stdlib/`) [Minor, stdlib-author C34]
- [x] Add `tests/corpus/eval/typecheck_advisory.llt-eval` proving type errors are advisory — demonstrates Int-annotated function called with String still evaluates successfully. (`tests/corpus/eval/`) [Minor, test-crafter C34]
- [x] Document CALL-MONO/CALL-POLY literal type divergence in `doc/06-type-inference.md` — added table of 7 divergent type pairs, explanation of bidirectional unify vs directional is_subtype, forward reference to U-SUBSUME migration. (`doc/06-type-inference.md:213-225`) [Minor, computer-scientist C34]

## readme-polish: README and CLAUDE.md Accuracy Fixes (C47)

- [x] Fix src/parser.rs:15 "Phase 6" → "Parser Rewrite milestone (iterative-parser sprint)" [Major]
- [x] Fix doc/09-documents.md typo "An Tinct" → "A Tinct" [Minor]
- [x] Fix src/lib.rs:29-35 orphaned phase comments removed [Minor]
- [x] Add architecture redirect to CLAUDE.md [Minor]
- [x] Add pest recursion caveat to doc/15-ast.md [Major]
- [x] Fix parse_expression docstring — already done by parser-doc-fixes [Nit]
- [x] Stage doc/whatif/tls.md [Nit]
- [x] Document parse_expression in doc/15-ast.md [Nit]
- [x] Add (dev, ino) migration note to doc/16-architecture.md EvalState sketch [Nit]

### cycle-findings-c31-panel: Panel Fix-Later Items (Cycle #31)

- [x] Consider inlining `resolve_type_expr_value` at its single call site (`src/typecheck.rs:1426`) — it is now a trivial one-line wrapper with no semantic distinction from `resolve_type_expr`; the wrapper name implies a semantic distinction that no longer exists. (`src/typecheck.rs:1547-1554`) [Nit, integration-verifier C40 panel]
- [x] Fix error message from composite type annotation failures loses annotation context — "invalid type expression" (from `resolve_type_expr` fallback) is less specific than the prior "invalid type in annotation"; consider "invalid type expression in annotation". (`src/typecheck.rs:1559-1562`) [Nit, integration-verifier C40 panel]
- [x] Add level-restoration-on-error test — no test proves `state.level` is correctly restored when `infer_expr` fails during non-Dict Record processing in `typecheck_document`; a non-last Record expression that fails inference should leave subsequent expressions seeing the correct level. (`src/typecheck.rs`) [Minor, test-crafter C40 panel]
- [x] Add negative test cases for composite type annotation error messages — `test_annotation_composite_function_type` verifies the success path but no test verifies that malformed composite types (e.g., `[type: [Fn@]]`) produce clear error messages. (`src/typecheck.rs`) [Nit, test-crafter C40 panel]
- [x] Add unit test for `resolve_type_name` outside-function-scope (`ann_mapping` is `None`) monotonicity — the `None` path (lines 1369-1377) was also fixed but has no test; a top-level `[@a $x]` annotation used twice in a single expression exercises this path. Add a test that creates a scenario where U-VAR-LEVEL lowers the annotation variable's level and the second reference returns the lowered level. (`src/typecheck.rs:1369-1377`) [Minor, test-crafter + computer-scientist C71 panel]
- [x] Add corpus test for shared annotation identity — `[f: [fn [x@a y@a] $x]]` should produce a polymorphic function; no corpus-level regression test for repeated annotation identity exists. Add `tests/corpus/eval/typecheck_shared_annotation_identity.llt-eval`. (`tests/corpus/eval/`) [Minor, test-crafter C71 panel]
- [x] Add named-arg explanation to type checker arity mismatch errors — when `typecheck::typecheck_file()` produces an arity mismatch for a call that uses named args to satisfy required positional parameters, the error gives no hint that named arguments are not understood by the type checker. Evaluation may succeed at runtime while the type checker rejects the call. Fix: locate arity mismatch error construction in `src/typecheck.rs` and append: "Note: the type checker does not yet support named arguments. If named arguments satisfy this requirement, evaluation may still succeed." (`src/typecheck.rs`) [Major, integration-verifier C71]
- [x] Add exhaustive match enforcement to `ErrorKind::PartialEq` impl — the `PartialEq` impl at `src/error.rs:293-420` uses a catch-all `_ => false` arm; adding a new `ErrorKind` variant without adding a match arm silently falls through to `false`, making equality tests between identical variants incorrectly return `false`. Fix: add `#[deny(clippy::match_wildcard_for_single_variants)]` attribute on the impl block, or restructure to use an exhaustive match without a wildcard arm. (`src/error.rs:293`) [Minor, integration-verifier C71]
- [x] Fix `builtin_take` Seq recursive tail stores `depth` not `depth+1` — `builtin_take`'s Seq path at `src/builtins.rs:2144` constructs a recursive `PendingBuiltin(builtin_take, ...)` whose stored `depth` is the current depth, not `depth+1`. Every recursive take step runs with the same internal budget, allowing a large `$take` to bypass the depth budget across all tail steps (same class as tracked `filter_dict_step` depth bug at TODO.md line 1065). Fix: change `depth` to `depth+1` at line 2144. Add unit test verifying `take(MAX_EVAL_DEPTH + 5, range(0))` produces `DepthExceeded`. (`src/builtins.rs:2144`) [Minor, eval-engine C68]
- [x] Add corpus test for `is_subtype` open-record soundness — no corpus-level end-to-end test demonstrates that a real LLT program with an open-record-as-closed-record type mismatch is caught by the type checker; unit coverage is complete. Add `tests/corpus/eval/typecheck/open_record_not_subtype_of_closed.llt` that typechecks a function accepting an open record applied to a closed-record position. (`tests/corpus/eval/typecheck/`) [Minor, test-crafter C69 panel]
- [x] Verify Cargo.lock for anomalous `serde_core` and `zmij` dependencies — `Cargo.lock:487-528, 772-775` shows `serde 1.0.228` depending on `serde_core` and `serde_json 1.0.149` depending on `zmij`; neither dependency exists in the legitimate crate graph for these versions; no `[patch]` in `Cargo.toml` explains the entries. Action: delete `Cargo.lock`, run `cargo update` in a clean environment, compare new checksums against crates.io registry; add `cargo audit` to CI as a blocking gate. (`Cargo.lock:487-528, 772-775`) [Critical, security-expert C70]

### cycle-findings-c70-b-docs-tests: Doc and Test Fixes (Cycle #70)

14 items: 8 doc fixes + 6 corpus tests.

- [x] Fix doc/16 Environment HashMap→IndexMap inline comment [Minor]
- [x] Fix doc/11 derivation table ceil $builtin-sub [Minor]
- [x] Fix doc/11 $or "returns true" → "returns $a" — already done by prior sprint [Nit]
- [x] Fix doc/11 $merge Part 6 line numbers [Nit]
- [x] Fix doc/08 depth 0 → depth+1 for repeat/iterate/unfold [Minor]
- [x] Fix doc/08 DELTA-ITERATE rule d → d+1 [Minor]
- [x] Fix doc/06 [U-ANY] level-zeroing note added [Minor]
- [x] Fix doc/15 parser.rs:431→430 line reference [Nit]
- [x] Add type_alias_eval.llt-eval corpus test [Minor]
- [x] Add annotated_bare_eval.llt-eval corpus test [Minor]
- [x] Add three_document_pipeline.llt-eval corpus test [Minor]
- [x] Add range_arity_mismatch.llt-eval error corpus test [Minor]
- [x] Add proxy_type_of.llt-eval corpus test [Minor]
- [x] Add scope_chain_int_keys_not_bound.llt-eval corpus test [Nit]

### doc-rowunification-retrospective-c: Inference Rule Doc Correctness (C55 Overflow)

Docs-only sprint (no build gate or panel review).

- [x] Add occurs-check/level-lowering note to [DOT-ROWVAR] — preconditions + side-effects documented [Minor, grammar-architect C55]
- [x] Disambiguate α in [DOT-VAR] — S(α) resolution note added [Minor, grammar-architect C55]
- [x] Align notation between [DOT-VAR] and [DOT-ROWVAR] — asymmetry prose added [Minor, grammar-architect C55]
- [x] Document InferState.subst merge strategy — or_insert semantics + known limitation noted [Minor, grammar-architect C55]
- [x] Note [U-SUBSUME] divergence from unify() promotion arms — dual-path design documented [Minor, computer-scientist C55]
- [x] Fix doc/11-stdlib.md $sort compile-time claim → runtime detection [Minor, stdlib-author C55]
- [x] Fix phantom semicolon rule — already resolved by doc-type-polish (opposite direction: removed from doc) [Minor, grammar-architect C59]
- [x] Add manual PartialEq note to RowTail in doc/07 [Nit, integration-verifier C59]
- [x] Add Failed and Guarded variants to ThunkState sketch in doc/16 [Nit, grammar-architect C56]
- [x] Label Type Check as advisory in doc/16 pipeline diagram [Nit, integration-verifier C56]
- [x] Update check_expr pseudocode — already done by doc-type-polish sprint [Minor, computer-scientist C60]

### cycle-findings-c33-b: Minor Findings (Cycle #33)

Minor findings from Cycle #33 codebase review. All items independent.

- [x] Rename `proxy_to_json.llt-eval` to a non-misleading name — the file tests E080 (proxy handler error), not E035 (ValueNotSerializable), but the filename implies E035 corpus coverage. Fix: rename to `proxy_handler_error.llt-eval` or `proxy_field_access_error.llt-eval`. (`tests/corpus/eval/errors/proxy_handler_error.llt-eval`) [Minor, grammar-architect C33]
- [x] Add `MAX_STRING_SIZE` check to `$upper` and `$lower` — `src/builtins.rs:624` (`$upper`) and `src/builtins.rs:639` (`$lower`) call `s.to_uppercase()`/`s.to_lowercase()` with no output size guard; Unicode case conversion can produce longer UTF-8 than the input. Fix: after conversion, check `result.len() > MAX_STRING_SIZE` and return resource limit error. (`src/builtins.rs:624, 639`) [Minor, security-expert C33]
- [x] Fix `doc/09-documents.md:586-588` "Known defect" paragraph is stale — states that the include guard and `base_dir` are not restored on materialization failure; the implementation at `builtins.rs:1144-1158` already handles both branches via an explicit `match` with `cleanup()` in both arms. Fix: rewrite to "Previously known defect: resolved — `cleanup()` is called in both Ok and Err branches; see `builtins.rs:1144-1158`." (`doc/09-documents.md:586-588`) [Minor, security-expert C33]
- [x] Add `const` and `until` to `doc/11-stdlib.md` reference table — both are public prelude functions (`prelude.llt:44` and `prelude.llt:155`) accessible to user code but absent from the reference table (line 231 claims "62 functions"). Fix: add `const` to the Identity section and `until` to the Control Flow section; update function count. (`doc/11-stdlib.md:231,247,292`) [Minor, integration-verifier C33]
- [x] Add corpus tests for Proxy dot and bracket access — `eval_dot_access` and `eval_bracket_access` dispatch to `invoke_proxy_handler` for Proxy values but there are no end-to-end corpus tests verifying handler receives the correct key type. Fix: add `tests/corpus/eval/builtins/proxy_access_dot.llt-eval` (String key from dot access) and `proxy_access_bracket.llt-eval` (Int key from bracket access). (`tests/corpus/eval/builtins/`) [Minor, eval-engine C33]
- [x] Add `check_dot_access` / `lower_row_var_levels_pub` callsite unit test — the public wrapper `lower_row_var_levels_pub` at `src/types.rs:700-702` is called from `check_dot_access` at `typecheck.rs:718` in the RowVar arm; a regression in the callsite (wrong `max_level` arg) would not be caught by the types.rs unit tests. Fix: add `test_check_dot_access_lowers_row_var_levels` verifying inner variable levels are lowered to `min(inner, rho_level)`. (`src/typecheck.rs:718`) [Minor, test-crafter C33]
- [x] Expand `tests/corpus/eval/access/` with range and bracket-int-key tests — 3 files exist (dot access, bracket string key, bracket access); still absent: range access and bracket access with integer key. Fix: add `range_access_simple.llt-eval` and `bracket_access_int_key.llt-eval`. (`tests/corpus/eval/access/`) [Minor, test-crafter C33]

### cycle-findings-c34-a: Major Findings (Cycle #34)

Major findings from Cycle #34 full codebase health review. All items independent.

- [x] Fix Guarded thunk DepthExceeded restoration — when `materialize(&inner, ...)` at `src/eval.rs:1541` fails with DepthExceeded (non-cacheable), the Guarded state is not restored; thunk remains stuck in InProgress. Fix: before the existing `match result` block, add an arm `if let Err(ref e) = result && !e.kind.is_cacheable() { thunk.set_state(ThunkState::Guarded { inner, expected, field_path, guard_span }); return Err(...); }`. (`src/eval.rs:1541-1611`) [Major, eval-engine C34]
- [x] Fix `doc/11-stdlib.md:106` false claim that `$deep-eq` exists — line 106 states "Structural equality is available via `$deep-eq`" but this function does not exist in `stdlib/prelude.llt` or `src/builtins.rs`. Fix: remove the sentence. (`doc/11-stdlib.md:106`) [Major, stdlib-author C34]
- [x] Fix `doc/11-stdlib.md:231` function count understated — corrected to "~110 total: 46 Rust builtins + 64 LLT functions (52 public API + 12 shadowable wrappers)" based on actual verification. (`doc/11-stdlib.md:231`) [Major, stdlib-author C34]
- [x] Add `state.subst.apply(ty)` at the start of `generalize()` for defense-in-depth — Damas & Milner (1982) gen() requires generalization over the image of the current substitution, not the raw type. Fix: add `let ty = &state.subst.apply(ty);` as first line of `generalize()`. (`src/types.rs:1224`) [Major, computer-scientist C34]
- [x] Audit desugar ordering across all eval entry points — added desugar call to `$include` builtin in `src/builtins.rs` and to all `typecheck.rs` test helpers; all entry points now correctly call `desugar_file` before `typecheck_file`/`eval_file`. (`src/builtins.rs:1180`, `src/typecheck.rs`) [Major, integration-verifier C34]

### cycle-findings-c36-a: Major Findings (Cycle #36)

Major findings from Cycle #36 full codebase health review. All items independent.

- [x] Fix `PendingCall` materialization pre-clones 4 values unconditionally on hot path — moved all four clone calls inside non-cacheable error branches; hot path (99%+ successful calls) now pays zero clone cost. (`src/eval.rs:1376-1380`) [Major, performance-expert C36]
- [x] Delete unreachable CALL-MONO branch in `check_call_with_scheme` — `!func_ty.has_type_vars()` guard was always false after `instantiate_scheme()`; deleted dead code, added invariant comment. (`src/typecheck.rs:928-935`) [Major, performance-expert C36]
- [x] Fix LSP eval errors shown as `INFORMATION` severity — changed `DiagnosticSeverity::INFORMATION` to `DiagnosticSeverity::ERROR`; updated test assertion to match. (`src/lsp/analysis.rs:244, 364`) [Major, integration-verifier C36]
- [x] Fix `doc/11-stdlib.md:231` total function count wrong — updated from "~110" to "~122" (46 Rust builtins + 12 stable aliases + 64 LLT functions). (`doc/11-stdlib.md:231`) [Major, stdlib-author C36]
- [x] Fix `doc/11-stdlib.md` `$concat` and `$merge` lazy-overlay descriptions — updated to show eager materialization as current; marked lazy-overlay as "(Planned Future Design)". (`doc/11-stdlib.md:74, 84, 632, 717-721`) [Major, stdlib-author C36]
- [x] Fix `doc/11-stdlib.md:107` structural equality phrasing — changed from "not currently implemented" to "intentionally not provided" with rationale (lazy evaluation + value semantics). (`doc/11-stdlib.md:107`) [Major, stdlib-author C36]

### cycle-findings-c36-b: Minor Findings (Cycle #36)

Minor findings from Cycle #36 full codebase health review. All items independent.

- [x] Fix `doc/08-evaluation.md:186` stale O(n²) comment — removed stale "compared to the current O(n²) eager implementation"; updated to "This gives `[call $map $f $big-dict]` O(n) construction and O(1) per-element access". (`doc/08-evaluation.md:186`) [Minor, eval-engine C36]
- [x] Fix `doc/11-stdlib.md:286` `$join` fake LLT-style signature — removed `[fn ...]` expression; added "Rust native builtin — no LLT wrapper" note. (`doc/11-stdlib.md:287`) [Minor, stdlib-author C36]
- [x] Fix `src/types.rs:96` `is_subtype` depth-safety comment — updated to cite structural recursion on finite ADT as direct termination argument and occurs-check (Robinson 1965) as supporting invariant for acyclicity. (`src/types.rs:96-99`) [Minor, computer-scientist C36]
- [x] Fix `src/eval.rs:914,925` bind_args_thunks double scan — replaced `position()` + `any()` with single `position()` + match; eliminates redundant O(params) scan per named arg. (`src/eval.rs:912-934`) [Minor, performance-expert C36]
- [x] Fix `src/main.rs:236` desugar comment — updated from misleading "pre-typecheck" to "mandatory pre-eval transformation; typecheck intentionally skipped in CLI". (`src/main.rs:236`) [Minor, integration-verifier C36]

### cycle-findings-c34-b-deferred: Deferred from cycle-findings-c34-b

- [x] Fix U-SUBSUME migration claim imprecision in `doc/06-type-inference.md:225` — updated forward reference to acknowledge design tension: bidirectional U-SUBSUME preserves same permissiveness; full divergence elimination requires directional U-SUBSUME (Pierce & Turner 2000). (`doc/06-type-inference.md:225`) [Minor, computer-scientist C34 panel]

### cycle-findings-c33-b-deferred: Deferred from cycle-findings-c33-b

- [x] Add doc note to `doc/06-type-inference.md` [DICT-GEN] rule explaining Pass 3b/3c — added implementation note at lines 561-564 explaining the three sub-passes (3a: clone state.subst, 3: unify into local, 3b: merge state.subst updates, 3c: apply merged subst to field types). (`doc/06-type-inference.md:561-564`) [Minor, type-theorist C33]

### cycle-findings-c32-code: Code Fixes (Cycle #32)

- [x] Add `ErrorKind::ResourceLimitExceeded` variant (E043, non-catchable) — replaces catchable `Internal` for 9 resource limit sites in builtins.rs ($replace, $upper, $lower, $collect, $join). (`src/error.rs`, `src/builtins.rs`) [Minor, security-expert + computer-scientist C32]
- [x] Add `test_collect_all_vars` unit test — covers TypeVar, Record+RowVar, Function, Seq, ground types. (`src/types.rs`) [Minor, test-crafter C32]
- [x] Migrate `instantiate_at_level`, `instantiate`, `generalize` to `collect_all_vars` — fused single-pass replaces two separate tree walks + two BTreeSet allocations. (`src/types.rs`) [Minor, performance-expert C32]
- [x] Fix REPL `eval_input` never calls typecheck — added `typecheck_file` call after desugar in repl.rs. (`src/repl.rs`) [Minor, integration-verifier C32]
- [x] Fix `src/ast.rs:128` stale line reference — updated to `typecheck.rs:1224`. (`src/ast.rs`) [Minor, grammar-architect C32]
- [x] Fix `check_expr` ann_mapping unconditional HashMap allocation — added annotation-presence guard. (`src/typecheck.rs`) [Minor, performance-expert C32]
- [x] Add capacity hint to `infer_dict` Pass 4 `schemes` map — `IndexMap::with_capacity(field_types.len())`. (`src/typecheck.rs`) [Minor, performance-expert C32]

### cycle-findings-c41-a: Major Findings (Cycle #41)

- [x] Fix `check_call_with_scheme` leaks local substitution — seeded from state.subst and merged back after unification loop (Algorithm W threading). (`src/typecheck.rs:945-970`) [Critical, computer-scientist C41]
- [x] Fix `check_call_with_scheme` local substitution not seeded from `state.subst` — combined with above. (`src/typecheck.rs:945`) [Major, computer-scientist C41]
- [x] Fix `materialize()` depth check fires before `Materialized` early-return — moved into deferred-state arms. (`src/eval.rs`) [Major, eval-engine C41]
- [x] Fix `Guarded` thunk error decoration — inner origin threaded through all 5 error paths. (`src/eval.rs`) [Major, eval-engine C41]
- [x] Fix `doc/11-stdlib.md:231` function count (122→117). (`doc/11-stdlib.md:231`) [Major, stdlib-author C41]
- [x] Fix parser error messages expose internal Rule enum — added `rule_to_display()` helper. (`src/parser.rs`) [Major, grammar-architect C41]

### cycle-findings-c41-b: Deferred from cycle-findings-c41-a panel

- [x] Fix `check_call` non-scheme CALL-POLY substitution leak — applied same seed+merge pattern as `check_call_with_scheme`. (`src/typecheck.rs:1112-1138`) [Major, computer-scientist C41 panel]

### cycle-findings-c32-docs: Doc Fixes (Cycle #32)

- [x] Fix `doc/10-errors.md` variant catalog missing `IncludeForbidden` — added to Limit errors (E040-E049) section. (`doc/10-errors.md:425`) [Minor, integration-verifier C32]
- [x] Fix `doc/06-type-inference.md` [U-VAR-LEVEL] rule omits row variable lowering — updated to "FTV(τ) ∪ FRV(τ)". (`doc/06-type-inference.md:492-506`) [Minor, type-theorist C32]
- [x] Fix `doc/08-evaluation.md` `$reduce`/`$fold` laziness table — split into Dict (fully lazy PendingCall chain) vs Seq (tail materialized per step). (`doc/08-evaluation.md:798-799`) [Minor, eval-engine C32]

### cycle-findings-c31-critical: Critical Fixes (Cycle #31)

- [x] Fix CLI `run_eval` never calls typecheck — added `typecheck_file` call in `src/main.rs`. (`src/main.rs`) [Critical, integration-verifier C31]
- [x] Fix `doc/10-errors.md` omits E035/E043 — inserted ValueNotSerializable and ResourceLimitExceeded in codes table and variant table; updated count to 30. (`doc/10-errors.md`) [Critical, integration-verifier C31]
- [x] Add corpus regression test for Guarded thunk DepthExceeded — `tests/corpus/eval/errors/typeassert_depth_exceeded_not_circular.llt-eval`. [Critical, test-crafter C31]
- [x] Fix lambda checking mode backwards type error — swapped arguments in two `type_mismatch` calls. (`src/typecheck.rs:399,403`) [Major, type-theorist C31]
- [x] Fix `value_to_display_string` depth-limit catchable — replaced with `resource_limit_exceeded` (E043, uncatchable). (`src/lib.rs`) [Major, integration-verifier C31]
- [x] Fix MEMO-REACCESS missing `is_cacheable()` guard — added guard before `set_state(Failed)`. (`src/eval.rs:1252`) [Major, integration-verifier C31]
- [x] Add element count limit in `json_to_value` — added `MAX_COLLECT_SIZE` check for both Array and Object arms. (`src/builtins.rs`) [Major, security-expert C31]

### cycle-findings-c46-a: Major Findings (Cycle #46)

- [x] Fix [U-SUBSUME] doc/code divergence — `doc/06-type-inference.md` references `[U-SUBSUME]` **14 times** as a live unification rule (lines 76, 87, 91, 103, etc.) but `src/types.rs:1136` uses a catch-all `_ => Err(type_mismatch)` with no `is_subtype` fallback. The 8 explicit bidirectional promotion arms (lines 1075-1101) are NOT [U-SUBSUME] — they're hardcoded match arms. Fix: implemented the fallback (`if is_subtype(a, b) || is_subtype(b, a) { Ok(()) }` before the catch-all). Also removed unsound IntLiteral-Float arm. [Major, computer-scientist C46]
- [x] Fix `EvalError::Display` prints `Span::origin()` frames (0:0-0:0) in stack traces — the Display loop at `src/error.rs:827` prints ALL stack frames including synthetic `Span::origin()` entries from stdlib/builtin calls; add `if frame.span == Span::origin() { continue; }` guard to avoid internal call frames polluting user-facing stack traces. (`src/error.rs:827`) [Major, integration-verifier C46]
- [x] Fix doc/12-tooling.md §Sandboxing describes unimplemented features as current — the section (lines 59-149) describes Landlock, seccomp-bpf, import integrity hashes, `--allow-path`, and `llt hash` subcommand as if implemented; only `--no-fs` and `--timeout` actually exist. Add `**ASPIRATIONAL — NOT YET IMPLEMENTED**` header to the §Sandboxing section and distinguish implemented (`--no-fs`, `--timeout`) from planned features. (`doc/12-tooling.md:59`) [Major, security-expert C46]
- [x] Fix `stdlib/prelude.llt:10-15` header comment describes shadowable wrappers backwards — comment says "defined in this file for user override" but the wrappers exist so domain modules can shadow them via `$include` while the prelude's internal functions use stable `builtin-*` aliases; users don't override these, included modules do. Rewrite to explain the correct mental model: "Primary-name operators defined as shadowable wrappers; internal prelude code uses `$builtin-*` aliases so it remains correct when primary names are shadowed." (`stdlib/prelude.llt:10-15`) [Major, stdlib-author C46]

### cycle-findings-c41-a: Major Findings (Cycle #41)

- [x] Fix parser DoS: add `pest::set_call_limit(500_000)` before `LltParser::parse()` — without this, adversarial deeply nested input (500+ levels) can overflow the Rust call stack before MAX_PARSE_DEPTH=256 fires; pest provides a global call-limit API for exactly this purpose. Limit is 500,000 (not 8,000) because prelude.llt requires ~55k calls; see comment in src/parser.rs. (`src/parser.rs:18`) [Major, security-expert C41]
- [x] Fix LSP evaluates `$include` on document open — `EvalContext::new(..., false)` at `src/lsp/document.rs:100` should be `true` (enable no_fs flag); opening a malicious .llt file in an editor currently triggers `eval_file()` → `builtin_include()` with user-controlled paths, reading arbitrary system files. ONE-LINE FIX: change `false` → `true`. (`src/lsp/document.rs:100`) [Major, security-expert C41]
- [x] Add MAX_SUBST_SIZE cap to type inference — N open-record dot-accesses create O(N²) substitution growth via constraint accumulation; no size cap exists; a crafted type annotation can exhaust heap memory during inference. Add `const MAX_SUBST_SIZE: usize = 10_000` and check before each substitution insertion in `unify()`. (`src/types.rs`) [Major, security-expert C41]
- [x] Fix `doc/15-ast.md` Fn node missing `desugared` field — AST spec shows Fn without the `desugared: bool` field added during the underscore-desugar sprint; spec is out of sync with `src/ast.rs`. (`doc/15-ast.md`, `src/ast.rs`) [Major, grammar-architect C41]
- [x] Fix `doc/15-ast.md` TypeAssert missing RefCell wrapper — spec shows `resolved_type: Option<Type>` but actual field is `resolved_type: RefCell<Option<Type>>`; the interior mutability is semantically significant (typecheck writes post-parse). (`doc/15-ast.md`, `src/ast.rs`) [Major, grammar-architect C41]
- [x] Fix README.md severely stale counts — says "28 Rust-native builtins, 975 tests: 921 unit + 49 CLI + 5 corpus" but actual counts are 46 Rust builtins, 1425 tests (verified by test runner). Update all numeric claims to current values. (`README.md`) [Major, stdlib-author C41]

### cycle-findings-c41-c: Doc and Test Fixes (Cycle #41)

- [x] Add corpus tests for resource limit boundaries — no corpus tests exercise MAX_EVAL_DEPTH, MAX_PARSE_DEPTH, or MAX_COLLECT_SIZE boundaries; add: `tests/corpus/eval/errors/depth_exceeded_eval.llt-eval` (257-level nesting triggers E040), `tests/corpus/valid/edge_cases/parse_depth_max.llt-eval` (256 levels succeeds), `tests/corpus/invalid/syntax_errors/parse_depth_exceeded.llt-eval` (257 levels fails). (`tests/corpus/`) [Major, test-crafter C41]
- [x] Add laziness proof corpus tests for selective materialization contracts — add: `tests/corpus/eval/laziness/map_dict_lazy_values.llt-eval` (prove $map on Dict returns PendingCall thunks — access only one key), `tests/corpus/eval/laziness/filter_selective_materialization.llt-eval` (prove $filter only forces predicate), `tests/corpus/eval/laziness/and_short_circuit.llt-eval` (prove $and/$or don't evaluate second arg when first determines result, use $error in unused branch). (`tests/corpus/eval/laziness/`) [Major, test-crafter C41]
- [x] Fix `doc/10-errors.md:820-830` Span Assignment Corrections table is aspirational, not implemented — table documents five span assignment bugs with "Correct behavior" specs but no corresponding code changes; either implement fixes or retitle section "Known Issues". (`doc/10-errors.md:820-830`, `src/eval.rs`, `src/builtins.rs`) [Major, integration-verifier C41]
- [x] Document Display suppression of duplicate materialization span in spec — `error.rs:817-820` omits `(materialized at ...)` when `mat_span == definition_span`, but `doc/10-errors.md` §Part 4 never specifies this suppression rule. Add spec: "When materialization_span == definition_span, Display omits the (materialized at ...) clause". (`doc/10-errors.md`, `src/error.rs:817-820`) [Major, integration-verifier C41]
- [x] Fix `doc/11-stdlib.md:232` public API count — says "47 public API" functions but manual count of prelude.llt gives 52 (excluding -impl/-step/-check/-try helpers). Update to "52 public API + 12 shadowable wrappers = 64 LLT functions". (`doc/11-stdlib.md:232`, `stdlib/prelude.llt`) [Major, stdlib-author C41]
- [x] Fix `doc/11-stdlib.md:211` `words` derivation shows shadowable names but implementation uses builtin-* aliases — table shows `[call $filter ...]` but `prelude.llt:132-133` uses `$builtin-filter` and `$builtin-eq`; add note distinguishing "conceptual derivation (shadowable names)" from "actual implementation (stable aliases)". (`doc/11-stdlib.md:211`, `stdlib/prelude.llt:132-133`) [Major, stdlib-author C41]

### cycle-findings-c41-b: Minor Findings (Cycle #41)

- [x] Add `$split` parts-count limit — splitting a 10MB string by empty separator produces ~10M Thunk allocations (~400MB, 40× amplification); add `const MAX_SPLIT_PARTS: usize = 1_000_000` check before result map construction; also change `expect("collection too large")` at line 546 to `EvalError::resource_limit_exceeded`. (`src/builtins.rs:542-553`) [Minor, security-expert C41]
- [x] Add LSP method name length cap — malicious LSP client can send arbitrarily long method name; add `if req.method.len() > 256 { return error }` guard before `format!("method not found: {}", req.method)`. (`src/lsp/server.rs:111`) [Minor, security-expert C41]
- [x] Document TypeAssert eager timing in doc/08 Laziness Design table — TypeAssert forces immediately during `eval()` (annotation-time), not deferred to `materialize()` (access-time); add note to `doc/08-evaluation.md` §Laziness Design table for TypeAssert: "Materializes during eval() (annotation-time), not materialize() (access-time)". (`doc/08-evaluation.md`) [Minor, eval-engine C41]
- [x] Document eval_call strictness in doc/08 §Selective Materialization — function expression in `[call ...]` is materialized at call-site (eager function dispatch, lazy arguments); doc/08 §Selective Materialization Part 6 does not document this. Add: "eval_call strictness: function expression materialized at call-site; arguments are wrapped as Unevaluated thunks (call-by-need per Launchbury 1993)". (`doc/08-evaluation.md`) [Minor, eval-engine C41]
- [x] Add pipeline invariant cross-reference comments — `src/main.rs:236` and `src/lib.rs:88` both run parse→desugar→typecheck→eval but have no cross-reference; add comment: "PIPELINE INVARIANT: Desugar must run after parse and before typecheck. Update main.rs:236 and lib.rs:88 together". (`src/main.rs:236`, `src/lib.rs:88`) [Minor, integration-verifier C41]
- [x] Fix `check_expr` lambda checking mode not applying `state.subst` to expected return type before body check — at `src/typecheck.rs:516-520`, when there is no return annotation, `check_expr(body, expected_ret, ...)` uses a raw `expected_ret` pointer; if access-chain constraints during parameter inference bound TypeVars in `state.subst` appearing in `expected_ret`, those bindings are not applied. Fix: apply `state.subst.apply(expected_ret)` at line 520. (`src/typecheck.rs:516-520`) [Minor, computer-scientist C41]
- [x] Fix `doc/11-stdlib.md:52-56` category system missing "structure-materializing" label — `$length` and `$empty?` materialize dict structure without materializing values (thunks stay as Rc clones); neither "Structural" nor "Materializing" accurately describes this; either add "Structure-materializing" category or add a clarifying note to the "Materializing" definition. (`doc/11-stdlib.md:52-56`) [Minor, stdlib-author C41]

### cycle-findings-c41-panel: Panel Fix-Later Items (Cycle #41)

- [x] Improve MAX_SUBST_SIZE error message — "type inference exceeded maximum substitution size (N > 10000)" reports an internal implementation detail; change to actionable message like "type inference limit reached: too many open record constraints — use fewer chained dot-accesses or add explicit type annotations to break constraint chains". (`src/types.rs:382-386`) [Minor, integration-verifier C41 panel]
- [x] Thread span parameter to `infer_dict` to improve MAX_SUBST_SIZE error location — Pass 3d check uses `Span::origin()` (zero span 0:0-0:0) because `infer_dict` takes no span; threading the dict's span would produce a precise error location in LSP. (`src/typecheck.rs:694`) [Minor, integration-verifier C41 panel]
- [x] Fix README.md line 7 "Phase 6b (LSP) is next" claim — LSP is now actively developed; update status line to reflect current state. (`README.md:7`) [Nit, grammar-architect C41 panel]
- [x] Replace `NonZeroUsize::new(500_000).unwrap()` in parser.rs with a `const` — `const CALL_LIMIT: NonZeroUsize = ...` is cleaner and the `unwrap()` on a literal is safe but style-inconsistent. (`src/parser.rs:92`) [Nit, security-expert C41 panel]
- [x] Add MAX_SUBST_SIZE corpus end-to-end test — no `tests/corpus/eval/errors/` test exercises the substitution size limit through `eval_source`; add one so error code plumbing (error code, surfacing through typecheck_file) is regression-tested. (`tests/corpus/eval/errors/`) [Minor, test-crafter C41 panel] (completed: corpus test infeasible — typecheck errors are advisory in eval_source; existing unit tests in types.rs cover all insertion sites)
- [x] Add LSP-layer $include corpus test for no_fs — add a document.rs test that passes `[call $include "some_file.llt"]` as document text and asserts an eval error is produced, so a future `true → false` revert is caught. (`src/lsp/document.rs`) [Minor, test-crafter C41 panel]
- [x] Fix test infrastructure to support depth limit corpus tests — the eval error corpus test runner (`test_eval_error_corpus_has_error_codes`) triggers Rust stack overflow when evaluating depth-exceeded tests; fix by increasing runner stack size (`RUST_MIN_STACK` env) or using a worker thread with larger stack. Blocked `tests/corpus/eval/errors/depth_exceeded_eval.llt-eval` creation. (`tests/corpus_tests.rs`) [Minor, test-crafter C41 panel]
- [x] Document ErrorKind match exhaustiveness pattern — `#[deny(non_exhaustive_omitted_patterns)]` doesn't apply to same-crate enums; matches are verified exhaustive and enforced by `all_error_kind_variants()` runtime test. Document this in a comment near `all_error_kind_variants()` at `src/error.rs:1018`. [Minor, integration-verifier C41] — verified not needed — documented instead
- [x] Relax `!has_type_vars()` guard in `check_expr` lambda checking mode to support partial checking — the guard at `src/typecheck.rs:398` currently requires the expected type to be fully concrete before entering lambda checking mode; relaxing it (e.g., to allow TypeVars that are already bound in `state.subst`) is the prerequisite for the `state.subst.apply` is_empty guard added in cycle-findings-c41-b panel to become semantically load-bearing. Until then, the guard ensures `expected_ret` is always concrete and the apply is always a no-op. (`src/typecheck.rs:398`) [Minor, computer-scientist C41 panel]
- [x] Fix `doc/10-errors.md` `ArityBound::Display` code sample missing `Exact(1)` singular arm — the spec sample shows only `Self::Exact(n) => write!(f, "{n} arguments")`, which would render `ArityBound::Exact(1)` as "1 arguments". The real implementation at `error.rs:20-33` correctly includes `Self::Exact(1) => write!(f, "1 argument")` before the general case. Fix: update the code sample to add `Self::Exact(1) => write!(f, "1 argument"),` before the general `Exact(n)` arm. (`doc/10-errors.md:656-663`) [Major, integration-verifier C31]
- [x] Fix `doc/08-evaluation.md` `$merge` Laziness Design table describes aspirational future behavior as current — the table row at doc/08 line 768 reads "Lazy overlay: right shadows left, O(1) construction, O(k) per key for k chained merges" but actual `builtin_merge` at `src/builtins.rs:429-445` eagerly materializes both dicts and clones all entries (strictness signature: `S × S → D`). The Laziness Design table contradicts the strictness table in the same document. Fix: update the `$merge` row to reflect current eager behavior ("Materializes both dicts; clones entries; values stay as thunks"); add a future-work annotation for the planned lazy-overlay implementation. (`doc/08-evaluation.md:768`) [Major, eval-engine C31]
- [x] Fix `doc/16-architecture.md` `BuiltinArgs` sketch stale — comment at line 108 shows `BuiltinArgs { positional: Vec<Rc<Thunk>>, named: IndexMap<String, Rc<Thunk>> }` but the actual struct at `value.rs:16-22` has fields `args: &'a [Rc<Thunk>]`, `named`, `depth`, `call_span`, and `ctx: Rc<EvalContext>`. Three fields are missing and one has the wrong name. Fix: update comment to reflect actual struct shape. (`doc/16-architecture.md:108`, `src/value.rs:16-22`) [Major, integration-verifier C31]
- [x] Fuse `instantiate_at_level` triple type-tree walk into two — completed in cycle-findings-c32-code (Cycle #40): `collect_all_vars` created and all three functions migrated. (`src/types.rs`) [Minor, performance-expert C31]
- [x] Fix `resolve_row` unconditionally clones `Row` for the `RowTail::Empty` case — `src/types.rs:582` does `RowTail::Empty => row.clone()` even for the common closed-record case that requires no field merging. `resolve_row` is called twice per `unify_rows` invocation plus after re-resolution steps. Fix: return `Cow<'_, Row>` — borrow in the `Empty` case (`Cow::Borrowed(row)`), own only when merging resolved fields. (`src/types.rs:582`) [Minor, performance-expert C31]
- [x] Add capacity hint to `builtin_collect` initial dict — `src/builtins.rs:1256` uses `IndexMap::new()` for collecting a sequence; sequence length is not known in advance (lazy), but a reasonable starting capacity (e.g., `IndexMap::with_capacity(64)`) reduces reallocation frequency for common finite sequences. (`src/builtins.rs:1256`) [Minor, performance-expert C31]
- [x] Rewrite `tests/corpus/README.md` — the file documents only `valid/simple/`, `valid/complex/`, `valid/edge_cases/`, and `invalid/syntax_errors/`; missing all directories added since Cycle 4 (`valid/access/`, `valid/annotations/`, `valid/documents/`, `eval/builtins/`, `eval/stdlib/`, `eval/errors/`, `eval/laziness/`, `eval/typecheck/`, etc.); the "Test Output" section documents a now-obsolete emoji format (`✅`/`❌`) no longer matching actual test runner output. Fix: rewrite the directory structure table, remove obsolete output format, add `Directives` section for `# no_fs`. (`tests/corpus/README.md`) [Minor, test-crafter C31]

### cycle-findings-c46-b: Minor Findings (Cycle #46)

- [x] Fix doc/06-type-inference.md missing FTV/FRV disjointness specification — §Let-Generalization at line ~520 says "generalize ∀{α | α ∈ FTV(τ), ℓ(α) > ℓ}. τ" but doesn't specify that FTV collects ONLY TypeVars (not RowVars); add: "FTV(τ) collects type variables only (TypeVar nodes). FRV(τ) collects row variables only (RowVar nodes in RowTail positions). The two sets are disjoint by construction." (`doc/06-type-inference.md:520`) [Minor, type-theorist C46]
- [x] Fix doc/06-type-inference.md CHECK-FN rule missing substitution application note — the rule at lines 160-178 doesn't mention that both σᵣ and σ_exp are substitution-applied (S(σᵣ) and S(σ_exp)) before checks, per Algorithm W; add this to the rule description. (`doc/06-type-inference.md:177`) [Minor, type-theorist C46]
- [x] Fix doc/07-type-extensions.md "separate namespaces" claims could be misread — line 574 says type and row vars use "separate namespaces" which could be misread as "separate naming counters"; clarify that both share the `_t{n}` counter but are separated by the kinded `type_map` vs `row_map` in `Substitution`. (`doc/07-type-extensions.md:574`) [Minor, type-theorist C46]
- [x] Add test for `unify_remainders` Case 7 (same row var with incompatible unique fields) — `{x: Int, ...rho} ~ {y: Str, ...rho}` should error because rho cannot simultaneously provide both x and y; no test currently exercises this path. Add `test_unify_rows_case7_same_rowvar_incompatible_unique_fields`. (`src/types.rs`) [Minor, type-theorist C46]
- [x] Fix doc/08-evaluation.md `$update` laziness description wrong — the Laziness Design table row for `$update` says "Returns dict with PendingCall thunk on updated value" but current stdlib at `stdlib/prelude.llt` calls `$set` which calls `$merge` (eager); add `*Planned:*` qualifier. (`doc/08-evaluation.md:790`) [Minor, eval-engine C46]
- [x] Fix doc/11-stdlib.md function reference table missing `any?` and `all?` — implemented at `stdlib/prelude.llt:65, 79` but absent from the reference table at doc/11-stdlib.md; add to Logic section. (`doc/11-stdlib.md:258`) [Minor, stdlib-author C46] (already complete from stdlib-doc-a sprint)

#### cycle-findings-c70-b: Code Fixes (Cycle #70)

Code fix findings from Cycle #70 full codebase health review. All items independent.

- [x] Fix LSP `base_dir` uses `PathBuf::from(".")` instead of document directory — `src/lsp/document.rs:97-101`: `EvalContext::new` receives `PathBuf::from(".")`, so `$include` paths resolve against editor cwd rather than the document's directory. Fix: extract `uri.to_file_path().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())).unwrap_or_else(|| PathBuf::from("."))` and pass as `base_dir`. (`src/lsp/document.rs:97-101`) [Minor, security-expert C70]
- [x] Fix `builtin_split` result map missing capacity hint — `let mut map = IndexMap::new()` at `src/builtins.rs:527` iterates `parts` (a `Vec<&str>` with known length). Fix: `IndexMap::with_capacity(parts.len())`. (`src/builtins.rs:527`) [Minor, performance-expert C70]
- [x] Add `EvalError::missing_required_param` named constructor — `MissingRequiredParam` is the only ErrorKind with structured data lacking a named constructor in `error.rs`; `eval.rs:855-864` uses a raw inline struct literal. Fix: add `pub fn missing_required_param(param: impl Into<String>, span: Span) -> Self` to `src/error.rs` after `arity_mismatch`; replace raw literal at `eval.rs:855-864`. (`src/error.rs`, `src/eval.rs:855-864`) [Nit, integration-verifier C70]
- [x] Fix Pass 1 of `infer_dict` manually constructs TypeVar instead of calling `state.fresh_type_var()` — `src/typecheck.rs:500-504` manually does `Type::TypeVar(format!("_t{}", state.name_counter), state.level)` then separately `state.levels.insert(...)` and `state.name_counter += 1`, duplicating `fresh_type_var()`. Fix: replace with `let fresh_var = state.fresh_type_var();`. (`src/typecheck.rs:500-504`) [Nit, type-theorist C70]
- [x] Fix `i as i64` truncation in `builtin_filter` dict step — `src/builtins.rs:1807` uses `Key::Int(i as i64)` instead of `Key::Int(i64::try_from(i).expect("collection too large"))` pattern used at lines 379, 530, 960. Fix: replace `as` cast with `try_from().expect(...)`. (`src/builtins.rs:1807`) [Nit, security-expert C70]
- [x] Fix conflicting semicolon TODO entries — C52 item (line ~702) proposes updating `doc/02-syntax.md` to remove the phantom `semicolon = _{ ";" }` named rule; C59 item (line ~386) proposes the opposite: adding it to `grammar.pest`. Since `doc/02-syntax.md:713` says §6 Complete Grammar is "normative", the C59 direction (code follows spec) is correct. Fix: close the C52 entry as superseded by C59 with note "closed: C59 direction (add named rule to grammar.pest) is correct per normative spec". (`TODO.md`) [Minor, grammar-architect C70]
- [x] Close TODO.md line 637 as invalid — item claims `filter_seq_step`, `drop_seq_step`, `reduce_seq_step` "use flat `depth`" but all three use `depth+1` (verified at `builtins.rs:2059, 2075, 2269, 2402`). The inconsistency does not exist. Only `builtin_take` (line 2145) and `builtin_filter_dict_step` (line 1987) use flat `depth`, both tracked in eval-engine-c68. Fix: mark the item `[x]` with note "verified incorrect premise — step functions use depth+1; only take/filter_dict_step use flat depth (tracked separately)". (`TODO.md:637`) [Nit, eval-engine C70] — **Note: verified incorrect premise — step functions use depth+1; only take/filter_dict_step use flat depth (tracked separately).**
- [x] Fix TODO.md line ~528 claim about `ArityBound::Range` — item says "only `Exact` is constructed anywhere; `AtMost` and `Range` are never used" but `ArityBound::Range(1, 2)` is already used at `src/builtins.rs:1305` in `builtin_range`. Fix: update to "only `ArityBound::AtMost` is unused in non-test code; `Range` is already used in `builtin_range`". (`TODO.md`, `src/builtins.rs:1305`) [Nit, test-crafter C70] — **Completed: updated item at line 412 with note.**
- [x] Fix `doc/06-type-inference.md:66` "Checking positions" table stale after C65 — line 66 says "`check_expr` is used only at positions where the expected type is fully concrete (no type variables): CALL-MONO arguments, **return annotations**, and TypeAssert." This is stale: C65 added conditional dispatch in `infer_fn` — when `declared.has_type_vars()`, the body is synthesized and unified, not checked via `check_expr`. Also update table row at line 73: split "Function body with return annotation → `check_expr`" into two rows: (1) "Fn body + concrete return annotation → `check_expr`" and (2) "Fn body + TypeVar return annotation → synthesis + unify". Fix: update prose at line 66 to say "concrete return annotations (no type variables)"; split table row 73. (`doc/06-type-inference.md:66, 72-74`) [Minor, type-theorist C67]
- [x] Fix `doc/06-type-inference.md:76-80` "Unification positions" table missing three new TypeVar-annotation cases from C65/C66 — table lists only CALL-POLY. After C65 (`af13be0`) and C66 (`c9bb6b5`), three additional unification positions exist: (a) `infer_fn` return-ann-with-TypeVar at `src/typecheck.rs:1088`; (b) `check_expr` lambda-checking mode param-ann-with-TypeVar at `src/typecheck.rs:368`; (c) `check_expr` lambda-checking mode return-ann-with-TypeVar at `src/typecheck.rs:423`. Fix: add three rows to the "Unification positions" table with the typecheck.rs line references. (`doc/06-type-inference.md:76-80`) [Minor, type-theorist C67]
- [x] Fix `check_expr` docstring says "return annotations" unconditionally, stale after C65 — lines 302-303 say "This function is used at checking positions where the expected type is fully concrete (no type variables): CALL-MONO arguments, **return annotations**, and TypeAssert." After C65, `check_expr` is only called for return annotations when the declared type is fully concrete. Fix: change "return annotations" to "concrete return annotations (no TypeVars)". (`src/typecheck.rs:302-303`) [Nit, type-theorist C67]
- [x] Document overridable-ops Seq corecursion bypass — prelude wrapper overrides silently skipped for Seq tail steps — `builtin_map`, `builtin_filter_seq_step`, `builtin_take`, `builtin_drop_seq_step`, and `builtin_reduce_seq_step` create recursive `PendingBuiltin` tails that capture Rust function pointers directly (e.g., `builtins.rs:1743, 2056, 2072, 2142, 2266, 2399`). Overrides of `$map`, `$filter`, etc. via `$include` apply only to the initial dispatch; all subsequent Seq tail steps call `builtin_*` directly, bypassing the override silently. `doc/11-stdlib.md:233` says these operators are "shadowable by `$include`d modules" with no caveat. Fix: add documentation note to `doc/11-stdlib.md:233` and `stdlib/prelude.llt:509-515` stating: "Overrides apply to the initial dispatch only; Seq corecursion steps always call the underlying Rust implementation directly." (`src/builtins.rs:1743, 2056`, `doc/11-stdlib.md:233`, `stdlib/prelude.llt:509-515`) [Minor, eval-engine C67]
- [x] Fix TODO.md proxy strictness signature proposal `S → D` should be `L → D` — the open item in computer-scientist-c63 (line 117 of TODO.md) proposes adding `$proxy` to `doc/08-evaluation.md` strictness table with "Signature: `S → D` (strict in handler arg)". But `builtin_proxy` at `src/builtins.rs:2637` does `Rc::clone(&args[0])` — never calls `materialize()` on the handler. The correct signature is `L → D` (lazy in handler, returns Proxy container). The item's own Category note "returns the handler thunk wrapped in a new value variant **without computation**" contradicts the `S` annotation. Fix: when implementing that TODO item, use `L → D` as the signature and "Structural" as the category. (`TODO.md:117`, `src/builtins.rs:2637`) [Nit, eval-engine C67] — **Note: item at line 72 already has the correct `L → D` signature.**
- [x] Fix `doc/11-stdlib.md` stable alias names wrong after overridable-ops sprint — table at lines 169-170 shows aliases as `lt`, `eq`, `add`, `sub`, `mul`, `div` (no `builtin-` prefix). Actual registered names in `src/builtins.rs:2727-2732` are `builtin-lt`, `builtin-eq`, `builtin-add`, etc. Wrapper derivation column at lines 188-193 also shows `[call $lt $a $b]` — wrong. ASCII diagram at line 225 lists `$lt, $eq, $add` as Rust primitives — wrong. A DSL author following doc/11 who writes `[call $lt $a $b]` gets "undefined variable: lt". Fix: update lines 169-170, 188-193, 225 to use `builtin-lt`, `builtin-eq`, `builtin-add`, `builtin-sub`, `builtin-mul`, `builtin-div`. (`doc/11-stdlib.md:169-170, 188-193, 225`) [Major, test-crafter C67]
- [x] Add `builtin_proxy` unit tests — `builtin_proxy` (added in overridable-ops) has zero unit tests. Three distinct code paths: named-arg rejection (line 2633), arity ≠ 1 (lines 2634-2636), and success (lines 2637-2643). Corpus tests exist for error paths but unit tests verify Rust-level return type (`Value::Proxy { handler }` with the correct `Rc<Thunk>`). Fix: add `test_proxy_returns_proxy_value`, `test_proxy_arity_error`, `test_proxy_named_arg_error` in the `builtins.rs` `#[cfg(test)]` module. (`src/builtins.rs:2626-2643`) [Major, test-crafter C67]
- [x] Add proxy laziness proof corpus test — `tests/corpus/eval/laziness/` has no test proving the proxy handler is not called when unused. A proxy value in an unused dict entry should not trigger its handler. Add `tests/corpus/eval/laziness/proxy_handler_not_called_when_unused.llt-eval` with a handler that calls `$error` (would fire if evaluated), verify the proxy entry exists in the dict but no error occurs. (`tests/corpus/eval/laziness/`) [Minor, test-crafter C67]
- [x] Add `Value::Proxy` `type_name`, Debug, and Display unit tests in `value.rs` — `Value::Seq` has three unit tests covering `type_name()` → `"Seq"`, Debug format, Display format; `Value::Proxy` (added in overridable-ops) has none. `type_name()` returns `"Proxy"` (line 104), Debug returns `"Proxy"` (line 126), Display returns `"<proxy>"` (line 160) — all untested at unit level. Fix: add `test_proxy_type_name`, `test_proxy_debug`, `test_proxy_display` in `src/value.rs` test module. (`src/value.rs:104, 126, 160`) [Nit, test-crafter C67]
- [x] Fix `value_to_json` reports "cannot serialize Function to JSON" when value is a `Builtin` — the match arm `Value::Function { .. } | Value::Builtin { .. }` at `src/lib.rs:198-202` produces a single hardcoded message "cannot serialize Function to JSON" for both variants. The companion test `test_json_builtin_error` at `src/lib.rs:517-529` asserts this wrong message. Fix: split into two arms — Function arm keeps the existing message; Builtin arm uses `format!("cannot serialize Builtin ({name}) to JSON")`. Update the unit test accordingly. (`src/lib.rs:198-202, 517-529`) [Minor, integration-verifier C67]
- [x] Document operator wrapper call overhead in `doc/16-architecture.md` — the overridable-ops sprint replaced 12 direct builtin bindings with LLT wrapper functions. Every user-code use of `$+`, `$-`, `$*`, `$/`, `$<`, `$=`, `$if`, `$filter`, `$map`, `$reduce`, `$take`, `$drop` now pays: +1 `Rc<RefCell<Environment>>` allocation, +2–3 environment insertions, +1 eval depth level per invocation. The sprint panel documented this as "Measurement needed / fix-later" but no TODO entry or doc note was created. Fix: add a performance note to `doc/16-architecture.md` documenting the per-call overhead trade-off against operator shadowability, and note that prelude internals are shielded via `$builtin-*` aliases. (`doc/16-architecture.md`) [Minor, performance-expert C67]
- [x] Fix `Thunk::new_guarded` allocates `Cow::Owned(format!(...))` per record field — `new_guarded` at `src/value.rs:298-302` constructs `Cow::Owned(format!("type guard: {}", field_path.join(".")))` for every non-empty field_path. Called from `validate_and_wrap_record` for every field in a TypeAssert annotation. For a 5-field record, 5 `String::join` + 5 `format!` allocations on every evaluation, even on success paths where the origin string is never used. Fix: change to `Cow::Borrowed("type guard")` unconditionally — the detailed field path is already stored in `ThunkState::Guarded { field_path }` and is only formatted in error arms. (`src/value.rs:298-302`) [Nit, performance-expert C67]
- [x] Fix `resolved_type.borrow().clone()` clones full Type tree on every TypeAssert evaluation — `src/eval.rs:328` clones the `Option<Type>` from the AST node's RefCell. For a 10-field record annotation, this clones 10 strings + 10 types per TypeAssert evaluation. If TypeAssert appears in a `$map` body it fires on every element. Fix: change `resolved_type: RefCell<Option<Type>>` to `RefCell<Option<Rc<Type>>>` in `src/ast.rs` and the single write site in `src/typecheck.rs:1131`. The `borrow().clone()` becomes an `Rc` reference count bump instead of a full type-tree copy. (`src/eval.rs:328`, `src/ast.rs`, `src/typecheck.rs:1131`) [Nit, performance-expert C67]
