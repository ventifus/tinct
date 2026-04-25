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
