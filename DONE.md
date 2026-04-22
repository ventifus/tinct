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

Depends on eval-functions + types-polymorphism. The 28 true primitives that MUST be Rust: operations LLT cannot express. Everything else is derived in LLT in `stdlib/prelude.llt`.

- [x] Builtin registration: populate root environment with `Value::Builtin` entries
- [x] Arithmetic: `+`, `-`, `*`, `/` with auto-promotion (Int+Int=Int, mixed=Float)
- [x] Comparison: `<`, `=`
- [x] Control: `if` (selective materialization: only chosen branch evaluated)
- [x] Dict primitives: `keys`, `length`, `merge` (right-biased), `append`
- [x] String: `str` (concat/toString), `split`, `replace`, `upper`, `lower`, `trim`
- [x] Numeric: `floor`, `round` (Rust's f64::round, half-away-from-zero)
- [x] Parsing: `to-int`, `to-float` (string-to-number only; numeric conversion is LLT)
- [x] Evaluation control: `eval`, `error`, `try`, `apply`
- [x] Type introspection: `type-of`
- [x] I/O: `from-json`

### stdlib-loading: LLT Stdlib Loading

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
- [x] `--format` flag: output as JSON (default) or LLT (YAML deferred — would require `serde_yaml` dependency)
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
- [x] Identify which stdlib functions CAN be implemented as LLT code (all control flow, collection ops, composition, list ops, sorting, sequences, assertions — implemented in `stdlib/prelude.llt`)
- [x] Document the boundary in DESIGN.md with rationale for each Rust-native builtin (see "Rust-Native vs LLT-Implemented Boundary" section)
- [x] Design the stdlib loading mechanism (`include_str!` prelude, Rust builtins → LLT stdlib → user code environment chain)
- [x] Update task list to reflect the split: Rust-native builtins vs LLT stdlib (builtins-core = Rust builtins, stdlib = LLT already in prelude.llt)

## Stdlib Validation & Expansion

The LLT stdlib is implemented in `stdlib/prelude.llt` (already working; 79 corpus test files cover stdlib functions). This milestone validates and expands it. Rust-native builtins (strings, numeric conversion) were registered in builtins-core. LLT-implemented functions (`and`, `or`, `map`, `filter`, etc.) are already in the prelude.

### Validate prelude functions

- [x] Run prelude end-to-end with evaluator and fix any runtime bugs
- [x] Test each LLT stdlib function against expected behavior (79 corpus tests covering all public stdlib functions)
- [x] Performance check: identify any functions that need Rust reimplementation for practical use (see below)
- [x] Unify `value_to_display_string` and `value_to_json` via shared visitor pattern (analyzed: kept separate — divergent leaf rendering, error handling, and dict assembly means a visitor adds more code than it removes)
- [x] Clear thread-local `INCLUDE_CTX` after evaluation for library API safety (`clear_include_context()`)

Remaining stdlib functions stay in LLT prelude: logic (`and`, `or`), control flow (`cond`, `when`, `unless`), dict utilities (`get`, `get-or`, `get-in`, `has?`, `values`, `entries`, `empty?`, `set`, `remove`, `update`), list ops (`first`, `nth`, `last`, `reindex`), collection ops (`map-entries`, `fold`, `slice`, `find-deep`), composition (`compose`, `->`), error handling (`try-or`), assertions (`assert`), identity (`identity`).

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

New functions implementable in LLT without Seq support. Identified by stdlib-author review (2026-04-19).

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

- [x] Move `range`, `repeat`, `cycle` from LLT prelude to Rust builtins
- [x] `range` returns Seq; 1-arg form `[call $range start]` is infinite, 2-arg form finite
- [x] `repeat` returns infinite Seq; 1-arg form `[call $repeat val]` only
- [x] `cycle` returns infinite Seq; 1-arg form `[call $cycle xs]` only
- [x] `iterate`: `[call $iterate $f $x]` -> x, f(x), f(f(x)), ...
- [x] `unfold`: `[call $unfold $step $seed]` -> step returns `[value state]` or `[]`
- [x] Move `take` from LLT prelude to Rust builtin; dual-dispatch Dict (preserve keys) + Seq (return finite Seq)
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
- [x] Move `map`, `filter` from LLT prelude to Rust builtins
- [x] Tests: map/filter on dicts (lazy verification), map/filter on seqs, mixed pipelines

### drop-reduce: Drop/Reduce + Typing Strategy

Additional sequence operations and typing decisions for dual-dispatch ops.

- [x] `$drop` on seq: return seq skipping first n elements
- [x] `$reduce` on seq: accumulate, materializing each step
- [x] Move `drop`, `reduce` from LLT prelude to Rust builtins
- [x] Decide typing strategy for dual-dispatch ops (`$map`/`$filter` on Record vs Seq): `Any` escape hatch, union types, or separate functions [Major, type-theorist]
- [x] Document `join` O(n^2) due to repeated str concatenation; optimize in Rust builtin (`stdlib/prelude.llt:88-97`) [Minor, stdlib-author]
- [x] Tests: drop on seq, reduce on seq and dict

### include-cache: Include Caching

Cache `$include` results so re-including the same file returns the cached thunk instead of re-evaluating. Jsonnet caches import thunks; LLT currently re-evaluates every `$include` call.

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
- [x] Span conversion (`src/lsp/convert.rs`): LLT Span (offset, 1-indexed) ↔ LSP Position (0-indexed, UTF-16)
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

### error-structured-types: ErrorKind Type Definitions

- [x] Add `ErrorKind` enum with 25 variants and `ArityBound` enum to `src/error.rs`
- [x] Add `ErrorKind::code()` method returning stable error code strings
- [x] Add `ErrorKind::is_cacheable()` method returning `false` for `DepthExceeded`
- [x] Add `Display` impl for `ErrorKind` and `ArityBound` (rustc style)
- [x] Replace `message: String` with `kind: ErrorKind` in `EvalError` struct
- [x] Update `EvalError::Display` to include error code prefix `[E001]`
- [x] Update named constructors to construct `ErrorKind` variants
- [x] Add `EvalError::internal(message, span)` replacing `EvalError::new` (kept as backward-compatible shim)
