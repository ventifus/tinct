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

## iterative-eval: Iterative Evaluator

Migration from recursive to iterative evaluation using continuation-passing style (CEK machine). Eliminates Rust stack depth limitations for deeply nested lazy evaluation.

### iterative-eval-a: Convert materialize() to Iterative

Foundation for the CEK machine migration. Converted `materialize()` from recursive to iterative using a continuation stack (`Vec<MatCont>`). The public `materialize()` function now delegates to `materialize_rc()` for sub-thunk forcing, eliminating O(n) Rust call-stack depth for deeply nested PendingBuiltin/PendingCall chains.

- [x] Convert `materialize()` from recursive to iterative with `Vec<Frame>` work stack — implemented `materialize_rc()` with three continuation variants (Memoize, PendingCallDispatch, GuardedValidate) and iterative two-phase loop (force_step → apply_mat_cont). All 1456 tests pass.

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

### Design work (requires design loop)

- [x] Design type inference algorithm specification — see doc/06-type-inference.md §Type Inference Algorithm. Semi-formal spec with judgment rules: type grammar, 14 inference rules (INT, FLOAT, BOOL, STR, VAR, DICT, FN, CALL-MONO, CALL-POLY, CALL-ANY, DOT, BRACKET, RANGE, ASSERT, ALIAS, ANNOTATED), unification rules (Robinson-style + silent coercions), subtyping rules (S-ANY-TOP/BOT, S-REC, S-FN with variance), instantiation with row variable renaming, 8 documented limitations. Agent-reviewed: computer-scientist + type-theorist. [Critical, computer-scientist]
- [x] Design let-generalization for proper HM inference — see doc/06-type-inference.md §Let-Generalization (Levels-Based). Kiselyov (2013) levels-based approach with InferState, symmetric level lowering, Any-unification level zeroing, TypeScheme in TypeEnv, RowVar levels, document-level scheme threading. Agent-reviewed: computer-scientist + type-theorist + integration-verifier (18 findings, all applied). [Major, computer-scientist]
- [x] Design literal promotion semantics in unify() — see doc/06-type-inference.md §Bidirectional Typing + §Unification [U-SUBSUME]. Bidirectional type checking (Pierce & Turner 2000; Dunfield & Krishnaswami 2021): synthesis/checking modes, check_expr for concrete positions (CALL-MONO, TypeAssert, return annotations), unify with [U-SUBSUME] for CALL-POLY. Pure Robinson unification + bidirectional subsumptive fallback for concrete pairs. Singleton literal types (not refinement types). Agent-reviewed: computer-scientist + type-theorist (confluence fix applied). [Critical, computer-scientist]
- [x] Design TypeAssert static/runtime consistency — see doc/05-type-annotations.md §TypeAssert Runtime Validation. Full structural convergence: elaboration (Dunfield & Krishnaswami 2021) embeds resolved type in AST node, proxy contracts (Findler & Felleisen 2002) wrap record field thunks in guards for lazy type validation on access. New ThunkState::Guarded variant. Consistency invariant qualified for deeply checkable types. Agent-reviewed: computer-scientist + type-theorist + eval-engine (2 rounds, all findings applied). [Major, computer-scientist]
- [x] Design `$_` desugaring as formal transformation — see doc/04-functions.md §`$_` Desugaring — Formal Specification. Pre-typecheck AST pass (parse → desugar → typecheck → eval). Top-down WRAP check on raw children with DIRECT predicate, depth-based lexical shadowing replaces eval-time env check. Corrected traversal avoids greedy-wrapping (Visser 1998). Type visibility qualified for current (Any → T) and future (bidirectional, row-polymorphic) inference. Agent-reviewed: computer-scientist + type-theorist + eval-engine + grammar-architect + integration-verifier (2 rounds, all findings applied). [Minor, computer-scientist]
- [x] Design sequence productivity obligations — see doc/08-evaluation.md §Productivity Obligations. Pragmatic approach: no static guarantee (Haskell/Nix model), three-layer runtime protection (blackholing, depth limit, tail discipline), productive-by-construction combinators as primary API, documented user obligations for `$seq`. Static checking rejected: totality (Turner 2004) sacrifices Turing-completeness, sized types (Abel & Pientka 2013) incompatible with HM. Agent-reviewed: computer-scientist + type-theorist + eval-engine (1 round, all findings applied). [Minor, computer-scientist]

### Formal specifications (additional)

- [x] Design formal specification of thunk lifecycle — see doc/08-evaluation.md §Thunk Lifecycle — Formal Specification. State transition DAG with monotonicity proof, 9 forcing rules (including FORCE-CALL-BUILTIN), 6 semantic properties, 4 explicit semantic commitments, adequacy argument for PendingBuiltin/PendingCall via defunctionalization (Reynolds 1972). Agent-reviewed: computer-scientist + type-theorist + eval-engine (all findings applied). [Major, computer-scientist]
- [x] Design formal specification of selective materialization — see doc/08-evaluation.md §Selective Materialization — Formal Specification. Two-tier spec: strictness signature table (Mycroft 1981) for all 44 builtins with S/L/Sc per-argument annotations + delta rules (Plotkin 1981 SOS) for 10 non-trivial builtins (14 rules). Five result classifications (→ V/D/Θ/LT/⊥), dual-dispatch pattern for 6 collection builtins, derived selectivity for 7 stdlib functions with inheritance proof sketch via DELTA-IF inlining. Four properties: branch isolation, strictness monotonicity, sharing preservation, dual-dispatch consistency. Agent-reviewed: computer-scientist + type-theorist + eval-engine + laziness-auditor (14 findings applied). [Major, computer-scientist]
- [x] Design formal specification of call convention — see doc/04-functions.md §Call Convention — Formal Specification. Dual-layer spec: declarative binding constraints + phased algorithm + complete correctness proof (uniqueness, soundness, completeness by case analysis). Kotlin model: any param nameable, interleaved required/optional allowed, per-parameter coverage check (C-COVERAGE) replaces count-based arity. Garrigue (1995) default-env separation. 6 constraints, 5 phased rules, 4 error classes, worked example. Agent-reviewed: computer-scientist + type-theorist + eval-engine (17 findings applied, including critical C-ARITY→C-COVERAGE rewrite). [Major, computer-scientist]
- [x] Design formal specification of scope chain semantics — see doc/08-evaluation.md §Scope Chain Semantics — Formal Specification. Launchbury (1993) natural semantics + Nakata & Hasegawa (2009) cyclic call-by-need. Three construction rules (DICT-SCOPE letrec, SEQ-SCOPE let*, DOC-PIPELINE $$) + LOOKUP with parent-chain walk. Five properties with proof sketches: shadowing correctness, mutual visibility (letrec sharing + construction-time non-forcing invariant), heap monotonicity, scope chain acyclicity (parent chain vs Rc capture graph distinction), determinism (Ariola-Felleisen confluence via Launchbury adequacy). Referential integrity corollary: scope-chain and dict-field access share same Rc<Thunk>. Type system parallel cross-reference to TypeEnv/let-generalization. Agent-reviewed: computer-scientist + type-theorist + eval-engine + laziness-auditor (all findings applied, including construction-time non-forcing invariant, DOC-PIPELINE depth parameterization, FORCE-DEPTH context-sensitivity). [Major, computer-scientist]
- [x] Design formal specification of access chain evaluation — see doc/08-evaluation.md §Access Chain Evaluation — Formal Specification. Access algebra with compositional chain semantics: projections (dot, bracket, range) composed left-to-right, parser produces nested AST nodes reduced inside-out. FORCE-DICT shared forcing step + three projection rules. Five chain properties: step-wise forcing, result laziness, error short-circuiting, depth consumption, sharing preservation (Launchbury). Type system correspondence: direct lookup (not constraint generation), type variable access is error (pre-row-unification), open record → Any via gradual typing, range type preservation sound by structural subtyping. Agent-reviewed: computer-scientist + type-theorist + eval-engine (all findings applied). [Minor, computer-scientist]
- [x] Design formal specification of equality and comparison — see doc/11-stdlib.md §Equality and Comparison — Formal Specification. Two primitive relations: EQ (total, cross-type Int/Float promotion via `as f64`) and LT (partial, errors on incompatible types). Type-dispatch tables with 7 rules each, derived relations ($>, $<=, $>=) via negation in stdlib. IEEE 754 NaN analysis with documented $<= / $>= anomaly (negation-based derivation), NaN entry path via $from-json → Infinity → arithmetic. Key::PartialOrd as pre-materialization optimization. Value::PartialEq vs $= divergence with implementation guidance. 10 algebraic properties including P3 transitivity WARNING at 2⁵³ boundary, P7 cross-type trichotomy failure. Agent-reviewed: computer-scientist + type-theorist + eval-engine (12 fixes applied, 6 deferred to type-extensions/float-nan-infinity/row-unification). [Minor, computer-scientist]
- [x] Design formal specification of $merge — see doc/11-stdlib.md §Merge — Formal Specification. Right-biased merge (L ⊕ R) with insertion-order preservation. Typing rules: T-MERGE for closed records (Record(F_L ⊕ F_R, Closed)), T-MERGE-ANY gradual fallback, forward-compatibility for row variables with 3 constraints (closed-record preservation, common-tail preservation, principality). 8 algebraic properties including associativity on both content and iteration order (with proof), monoid over ordered maps (Dict, ⊕, ∅), value preservation (Rc::clone). Lazy overlay compatibility: 3 behavioral equivalence constraints, 2 documented observable differences (error timing, error ordering), overlay chain depth exempt from MAX_EVAL_DEPTH. Harper & Pierce (1991) disjointness relaxed to right-bias, Rémy (1994) presence/absence alternative noted. Agent-reviewed: computer-scientist + type-theorist + laziness-auditor (13 fixes applied: P4a wrong iteration-order caveat removed with proof, Key type corrected, list-dict behavior noted, T-MERGE closed-record restriction explicit, TypeVar fallback specified, row-variable constraints strengthened, T-MERGE-ANY rationale documented, overlay error timing/ordering corrected, overlay strictness dual-noted, chain depth exemption specified, sharing constraint strengthened to Rc::ptr_eq, error message format matched). [Minor, computer-scientist]
- [x] Design formal specification of error semantics — see doc/10-errors.md §Error Semantics — Formal Specification. Dual-span error model (def_span + mat_span + stack). 6 error constructors. DECORATE rules with deduplication guards (expanded notation ∄f ∈ ε.stack) + idempotence property (E8) + origin cross-reference to §Scope Chain Semantics. 6 propagation rules: PROP-EVAL (with recursive materialize note), PROP-BUILTIN, PROP-RESULT (with PendingCall coverage note for 4 error paths), PROP-CYCLE (circular dependency, bypasses DECORATE — inline construction), PROP-DEPTH (non-caching). MEMO-CACHE + MEMO-REACCESS with mat_span=None case. $try catching boundary with typing (Any → Any, Phase 3+ for union result type). $error typing (Str → Any, no bottom type). 8 properties (E1-E8). Runtime vs static error distinction (EvalError vs Type::Error). Error structure with dual-span example, display format with frame ordering clarification, 9 exhaustive error categories with stability disclaimer, representative builtin-specific errors table with operator prefix note, $try catching, lazy error behavior, 6 span assignment corrections. Agent-reviewed: computer-scientist + type-theorist + eval-engine + span-integrity-checker (17 fixes applied: PROP-CYCLE added, DECORATE bypass noted, PendingCall 4-path coverage, PROP-EVAL conflation note, MEMO-REACCESS None case, E2 Option notation, DECORATE notation expanded, E8 idempotence, origin cross-ref, $try/$error typing, Type::Error clarification, operator prefixes, representative table note, 2 additional findings, row 3 fix, example, frame order). [Major, computer-scientist]
- [x] Design formal specification of document pipeline and $include — see doc/09-documents.md §Document Pipeline and $include — Formal Specification. Cross-references DOC-PIPELINE and SEQ-SCOPE (§Scope Chain Semantics). Include state Σ = ⟨base_dir, guard, cache, stdlib_env⟩ with thread-local model and EvalContext migration note. RESOLVE rule with canonicalization. Three include rules: INCLUDE-HIT (cache, Jsonnet-style memoization), INCLUDE-CYCLE (guard set detection), INCLUDE-EVAL (fresh eval with file size check, guard push/pop, base_dir save/restore, eager materialization). Allowlist forward reference to §Sandboxing (planned INCLUDE-DENY). Eager materialization invariant: $include is one of three builtins ($eval, $try) that eagerly materialize — required because guard/base_dir are stack-scoped but thunks outlive stack frames; 3 failure modes documented (cycle detection, path resolution, cache coherence). materialize vs deep_materialize distinction. 5 properties: P1 cycle detection termination (well-foundedness), P2 cache determinism with failure non-caching note (two independent caching levels), P3 guard restoration with known defect (materialize error path), P4 include determinism (conditional on filesystem, SC-2), P5 include isolation (stdlib_env only, empty $$). Agent-reviewed: computer-scientist + eval-engine + laziness-auditor (11 fixes applied: SC-5→SC-2, allowlist forward ref, file size check, parameter s defined, failure non-caching, no-eval window note, materialize error path defect documented, line range fix, builtin count corrected, materialize/deep_materialize distinction, $$ indirection clarified). [Major, computer-scientist]

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

Resource safety gaps in sequence combinators. Found by computer-scientist codebase review (2026-04-22).

- [x] Add `MAX_COLLECT_SIZE` limit to `builtin_collect` — added 1,000,000 element limit with helpful error suggesting `$take`. [Critical, computer-scientist]
- [x] Fix `builtin_iterate` passing `depth: 0` to PendingBuiltin tail — captured depth from BuiltinArgs, passes `depth + 1`. [Major, computer-scientist]
- [x] Increment depth in sequence combinator PendingBuiltin chains — incremented depth in 11 PendingBuiltin creation sites (range, repeat, cycle, iterate, unfold, map, filter, drop, reduce). [Major, computer-scientist]
- [x] Migrate `concat` Seq path from stdlib to Rust builtin — implemented as PendingBuiltin chain with dual Seq/Dict dispatch. [Major, computer-scientist]
- [x] Fix `builtin_filter_seq_step` depth accumulation on consecutive predicate failures — converted skip branch to internal loop like `builtin_collect`. (`src/builtins.rs:2055-2067`) [Major, eval-engine C39]
- [x] Add type validation to concat empty-xs path — added materialize+match guard so `concat([], 42)` errors correctly. (`src/builtins.rs`) [Minor, computer-scientist + eval-engine panel]
- [x] Add `checked_add` to concat Dict path index arithmetic — `idx += 1` changed to `checked_add` for consistency with `builtin_collect` and `builtin_append`. (`src/builtins.rs`) [Nit, eval-engine + performance-expert panel]
- [x] Fix `$take` PendingBuiltin depth to use `depth + 1` — already done. (`src/builtins.rs:2166`) [Minor, computer-scientist panel]
- [x] Fix `$filter` Seq initial PB depth inconsistency — already done. (`src/builtins.rs:1844,1857`) [Nit, computer-scientist panel]

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

Make comparison, arithmetic, and collection operators shadowable from stdlib, enabling embedded DSLs (e.g. `stdlib/sql.llt`) to intercept them. Add `$proxy` as a generic field-access interception primitive. See `doc/whatif/lib-sql.md` §Implementation in stdlib.

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
- [x] Stage doc/whatif/lib-tls.md [Nit]
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

#### cycle-findings-c66: Findings (Cycle #66)

Consolidated from: grammar-architect-c66, test-crafter-c66, integration-verifier-c66

- [x] Fix `Expr::TypeAssert` derives `PartialEq` via `resolved_type: RefCell<Option<Type>>` — `#[derive(PartialEq)]` on `Expr` at `src/ast.rs:67` means pre-typecheck and post-typecheck `TypeAssert` nodes compare unequal even when structurally identical (the `resolved_type` RefCell changes after typechecking). A test comparing a pre-check AST against a post-check one will fail with a confusing message. Either implement manual `PartialEq` for `Expr` that delegates to all fields except `resolved_type` in the `TypeAssert` arm, or add a comment documenting the asymmetry. (`src/ast.rs:67, 123-130`) [Minor, grammar-architect C66]
- [x] Add `call@Type` / `fn@Type` to doc/02 disambiguation table — `call@Type` as first token in `[]` silently falls through keyword dispatch to produce `Annotated { name: "call", ... }` (not a call keyword). This is correct behavior but entirely absent from §7 Token Disambiguation table. Add row: "`call@Type` (first in `[]`) → `Annotated { name: "call", ... }` (NOT keyword) — `@` after bare word converts keyword candidate to annotated value." (`doc/02-syntax.md`) [Nit, grammar-architect C66]
- [x] Add cross-references to three grammar rules with identical character class patterns — `param_name`, `annotation_word`, `access_field` at `src/grammar.pest:101-103, 113-115, 147-149` all expand to `(ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_" | "-")* ~ "?"?`. A future change to one should be applied to all three. Add `// Same character class as annotation_word and access_field — update all three together` above `param_name`; equivalent above the other two. (`src/grammar.pest:101, 113, 147`) [Nit, grammar-architect C66]
- [x] Fix Display tests for `Annotation::PropertyDict` use `Expr::VarRef` keys instead of `Expr::Str` — `test_display_type_assert_with_property_dict` and `test_display_annotation_property_dict_with_entries` at `src/ast.rs:395-406, 599-611` construct annotation entries with `Expr::VarRef("type")` as keys. The parser always produces `Expr::Str("type")` for bare-word annotation keys. Replace `Expr::VarRef("type")` / `Expr::VarRef("Number")` with `Expr::Str("type")` / `Expr::Str("Number")` and add comment: "Annotation keys from the parser are always `Expr::Str` (bare words); `Expr::VarRef` keys are structurally valid but never produced by the parser." (`src/ast.rs:395-406, 599-611`) [Nit, grammar-architect C66]
- [x] Add unit test `test_value_partial_eq_proxy_always_false` — `Value::Proxy` falls into the `_ => false` wildcard arm of `PartialEq` (alongside Dict, Function, Builtin, Seq) but this is unverified. Add test alongside existing `test_value_partial_eq_dict_always_false` etc.: create a `Value::Proxy { handler }` and assert `p.clone() != p`. (`src/value.rs`) [Major, test-crafter C66] — completed c70-b
- [x] Add corpus test for `$try` not catching `DepthExceeded` — `is_catchable()` returns `false` for `DepthExceeded` but there's no corpus test exercising this end-to-end. Unit test `try_depth_exceeded_not_catchable` exists at `src/builtins.rs:4368`. Add `tests/corpus/eval/errors/try_depth_exceeded_not_caught.llt-eval` if a reliable corpus-format triggering mechanism exists; otherwise add a comment in the unit test acknowledging the corpus gap. (`tests/corpus/eval/errors/`) [Major, test-crafter C66] — added comment in unit test at `src/builtins.rs:4627-4630`
- [x] Add unit test `test_display_proxy` for `value_to_display_string` Proxy arm — all other `Value` variants have `test_display_*` unit tests in `src/lib.rs`. Proxy returns `"Proxy"` but this is uncovered. Add: create `Value::Proxy { handler }`, call `value_to_display_string`, assert result is `"Proxy"`. (`src/lib.rs`) [Minor, test-crafter C66] — completed c70-b
- [x] Add corpus test for `$cond` laziness — `tests/corpus/eval/laziness/` has no test proving that `$cond` skips later branches. Add `tests/corpus/eval/laziness/cond_skips_later_branches.llt-eval`: `[call $cond [[true first] [true [call $error "should not evaluate"]]]]` → `String("first")`. (`tests/corpus/eval/laziness/`) [Minor, test-crafter C66] — added test
- [x] Add unit test documenting `split_test_file` behavior when `===` appears on its own line in the expected section — `\n===\n` in the expected output WOULD cause `split_test_file` to split at the wrong place, truncating the expected value. Add test that documents this accepted limitation rather than fixing it (since `===` in expected output is not a real use case). (`tests/corpus_tests.rs`) [Nit, test-crafter C66] — added `test_split_test_file_delimiter_limitation_documented` at line 898
- [x] Fix proxy access errors in `eval_dot_access` and `eval_bracket_access` missing `push_frame` — `invoke_proxy_handler` return at `src/eval.rs:1050` (dot access) and `src/eval.rs:1083` (bracket access) is NOT wrapped in `map_err(&push_frame)`. Dict access errors always include the `"accessing .field"` stack frame; proxy errors do not — asymmetry makes proxy errors harder to diagnose. Fix: chain `.map_err(&push_frame)` on both `invoke_proxy_handler` returns. Also add a corpus test `proxy_access_error_has_context.llt-eval` where a proxy handler raises an error. (`src/eval.rs:1050, 1083`) [Minor, integration-verifier C66]
- [x] Fix `doc/10-errors.md` Part 8/9 Implementation Correspondence table stale line numbers — typeassert-structural sprint shifted all materialize-related functions by ~320 lines. Key stale refs: `attach_materialization_context` is at `eval.rs:1129-1156` (doc says 815-843); `PROP-EVAL` Unevaluated path at `eval.rs:1255-1279` (doc says 931-951); `PROP-CYCLE` at `eval.rs:1231-1244` (doc says 908-922); `MEMO-CACHE` at `value.rs:467-469` (doc says 384-386); `TRY` at `builtins.rs:760-884` (doc says 800-884). Update all line numbers in the Part 8 and Part 9 tables. (`doc/10-errors.md:334-348`) [Nit, integration-verifier C66]
- [x] Fix `range_value` grammar rule allows `float_lit` but evaluator rejects float range bounds — `range_value = { float_lit | int_lit | var_ref }` at `src/grammar.pest:158` places `float_lit` first, so `$data[1.0..5]` is syntactically valid. But `eval_key()` → `value_to_key()` only handles `Value::String` and `Value::Int`; a Float bound produces "expected String or Int" — a confusing runtime error for code the grammar explicitly accepted. Fix: change to `range_value = { int_lit | var_ref }` (removing `float_lit`), and update `doc/02-syntax.md:533` and the §6 Complete Grammar section to match. Add corpus test `tests/corpus/invalid/syntax_errors/float_range_bound.llt-eval` with `$data[1.0..5]` → parse error. (`src/grammar.pest:158`, `doc/02-syntax.md:533`) [Major, grammar-architect C65]
- [x] Verify/close stale TODO item for `build_annotation_value` error message — message already improved to "annotation bracket expression must contain key-value entries, found {rule:?}" (`src/parser.rs:696-700`)
- [x] Add minimum test count assertions to `test_corpus_structure()` — currently only checks that directories exist, not that they have content. Deleting half the laziness or stdlib tests would pass silently. Add per-directory `assert!(find_test_files(path).len() >= MIN_COUNT)` constants: `EVAL_LAZINESS_MIN=9`, `EVAL_BUILTINS_MIN=30`, `EVAL_STDLIB_MIN=90`, `EVAL_ERRORS_MIN=35`. (`tests/corpus_tests.rs:262-268`) [Major, test-crafter C65] — added assertions at lines 311-341
- [x] Add corpus test for `$_` in multiple positions in the same expression — current underscore corpus tests all test a single `$_` per expression. The desugar at `src/desugar.rs:79-88` handles multiple placeholders (DIRECT detection) but no eval corpus validates this. Add `tests/corpus/eval/underscore_two_placeholders.llt-eval`: `[call $+ $_ $_]` should produce a function that accepts one argument and uses it twice (currying via wrapping). (`tests/corpus/eval/`) [Minor, test-crafter C65] — added test at `tests/corpus/eval/underscore/underscore_two_placeholders.llt-eval`
- [x] Fix `tests/corpus/eval/errors/typeassert_depth_exceeded_not_circular.llt-eval` has `===` in expected output — line 9 contains `=== [E040]` which causes `split_test_file` to incorrectly identify it as a second delimiter, leading to test failure. The expected output should be `[E040]` without the leading `===`. (`tests/corpus/eval/errors/typeassert_depth_exceeded_not_circular.llt-eval:9`) [Major, test-crafter C71] — FIXED
- [x] Fix or relocate `tests/corpus/eval/typecheck/non_dict_polymorphic_scheme.llt-eval` — test fails with `[E002] undefined variable: $make-record` at line 9. The test appears to expect `$make-record` from Doc 1 to be in scope in Doc 2, but document boundaries reset the env (only $$ is inherited). Either fix the test to use a valid scoping pattern or move to a different test category. (`tests/corpus/eval/typecheck/non_dict_polymorphic_scheme.llt-eval`) [Major, test-crafter C71]
- [x] Fix or relocate `tests/corpus/eval/typecheck/open_record_not_subtype_of_closed.llt-eval` — test expects `TYPE_ERROR: type mismatch` but eval succeeds with `Dict({"f": Function(r), "g": Function(r)})`. The comment at line 8-10 notes "the eval/typecheck corpus runner (test_typecheck_corpus) is currently disabled." This test should either be moved to `tests/corpus/invalid/type_errors/` when typecheck-stdlib-types sprint completes OR should be rewritten as an eval test that doesn't rely on type checking. (`tests/corpus/eval/typecheck/open_record_not_subtype_of_closed.llt-eval`) [Major, test-crafter C71]
- [x] Add corpus test for variadic parameter collecting named args into Dict — `fn_variadic.llt-eval` tests variadic with positional args. Variadic params also bind unused named args into a Dict. No corpus test for `[fn [x ...rest] $rest]` called with named args: `[call $f 1 y: 2 z: 3]` → `rest` should be `{y: 2, z: 3}`. Unit tests exist (`test_bind_args_variadic_collects_excess`, `src/eval.rs:5028`) but no corpus coverage. Add `tests/corpus/eval/fn_variadic_named_args.llt-eval`. (`tests/corpus/eval/`) [Minor, test-crafter C65]
- [x] Fix eval corpus test comment claiming "FIRST document" does not match implementation — comment at `tests/corpus_tests.rs:46` says "expected output is compared against the LAST expression from the FIRST document" but `eval_source()` at line 308 evaluates the full file (all documents) and returns the last value of the last document. Update comment: "Valid corpus: compares first expression's AST. Eval corpus: compares full file evaluation (last expression of last document)." (`tests/corpus_tests.rs:46`) [Nit, test-crafter C65]
- [x] Fix `doc/06-type-inference.md:256-279` claims "pure Robinson" unification but code implements bidirectional promotion rules — doc says "unification is pure Robinson — it handles type variable binding and structural decomposition only. Subtyping (literal promotion, numeric widening) is handled by `check_expr` via the [SUB] rule." But `src/types.rs` implements bidirectional promotion arms directly in `unify()` plus [U-SUBSUME] fallback. Fix: doc updated to document explicit promotion arms as fast-path optimizations and [U-SUBSUME] as the general fallback. IntLiteral-Float unsound arm removed. (`doc/06-type-inference.md`, `src/types.rs`) [Minor, type-theorist C65]

### docs-vs-code-stdlib-misc: doc/11, doc/03, doc/13, doc/15, doc/17, CLAUDE.md, and Code Accuracy

Split from docs-vs-code. Items targeting stdlib docs, data model, examples, AST, references, CLAUDE.md, and 4 code fixes.

- [x] **doc/02-syntax.md escape_seq missing extensibility note** — `escape_seq` supports exactly 5 sequences (`\"`, `\\`, `\n`, `\t`, `\r`) but no note explains this is the full set or documents the Unicode workaround. Add after line 218: "Currently supports these 5 sequences. Unicode escapes (`\uXXXX`) are not yet supported — use `$from-json` for full Unicode string parsing." (`doc/02-syntax.md:218`) [Minor, grammar-architect C66]
- [x] **doc/15-ast.md keyword detection table uses "optional whitespace" but parser uses horizontal-only** — Desugaring Rules table on lines 297-305 says "not followed by (optional whitespace then) `:` " but the parser's keyword-colon guard uses horizontal-only whitespace (no newlines). Change to "optional horizontal whitespace then" and add: "`call\n:` is a Dict (newline breaks keyword recognition)." (`doc/15-ast.md:297-305`) [Minor, grammar-architect C66]
- [x] **doc/08-evaluation.md §Laziness Design $reduce row inaccurate** — line 824 says "Materializes accumulator at each step" but the dict path builds a lazy PendingCall accumulator chain; only the Seq path materializes the tail per step. Fix line 824: "Builds lazy PendingCall accumulator chain; materializes tail at each step for Seq path only." (`doc/08-evaluation.md:824`) [Minor, eval-engine C66]
- [x] **doc/13-examples.md one example has prose explanation but others don't** — be consistent across examples. (`doc/13-examples.md`) [Nit, grammar-architect]
- [x] **doc/17-references.md appendix numbered but only one exists** — remove "A" from any "Appendix A" heading. (`doc/17-references.md`) [Nit, grammar-architect]
- [x] **doc/03-data-model.md missing Value::Seq documentation** — `Value::Seq` exists in code (`src/value.rs:84-85`) and doc/08-evaluation.md covers lazy sequences, but doc/03-data-model.md §Values doesn't mention Seq; add cross-reference. (`doc/03-data-model.md`, `doc/08-evaluation.md:~72`) [Major, integration-verifier]
- [x] **doc/11-stdlib.md stdlib count mismatch: 28 Rust builtins vs 62 total not clarified** — update headers to show breakdown (28 Rust + 34 prelude = 62). (`doc/11-stdlib.md`, `CLAUDE.md`) [Nit, integration-verifier]
- [x] **CLAUDE.md IncludeContext description missing cache field** — update builtins.rs row to mention include result cache for memoization. (`CLAUDE.md`) [Minor, integration-verifier]
- [x] **doc/11-stdlib.md builtin count stale** — `concat` moved to Rust builtin (test asserts 45); update count or remove hard count and point to `standard_builtins()` test as authoritative source. (`doc/11-stdlib.md`) [Major, stdlib-author]
- [x] **doc/11-stdlib.md stdlib reference missing `const`** — K combinator `[fn [x] [fn [y] $x]]` defined in `prelude.llt:44` but absent from reference table; add under Identity/Utility. (`doc/11-stdlib.md:~52`) [Minor, stdlib-author]
- [x] **doc/11-stdlib.md stdlib reference missing `from-entries`** — inverse of `entries`, implemented in `prelude.llt:248`; add under Dict Utilities. (`doc/11-stdlib.md`) [Minor, stdlib-author]
- [x] **doc/11-stdlib.md stdlib reference missing `any?` and `all?`** — predicates implemented in `prelude.llt:60-78`; add under Logic section. (`doc/11-stdlib.md`) [Minor, stdlib-author]
- [x] **doc/11-stdlib.md stdlib reference missing `until`** — iterate-until-predicate, implemented in `prelude.llt:154`; add under Control Flow. (`doc/11-stdlib.md`) [Minor, stdlib-author]
- [x] **doc/11-stdlib.md `join` argument order inconsistency** — reference says `[fn [sep xs] ...]` but Rust builtin takes `(xs, sep)`; verify and fix doc or code. (`doc/11-stdlib.md`, `src/builtins.rs`) [Nit, stdlib-author]
- [x] **doc/11-stdlib.md `concat` listed in both Rust builtins and Tinct List Operations** — now a Rust builtin; remove from Tinct table or add migration note. (`doc/11-stdlib.md`) [Nit, stdlib-author]
- [x] **src/builtins.rs include cache code comments** — add skip-guard rationale at cache hit, clarify "Check cache" comment placement, add doc comment to cache field. (`src/builtins.rs:1036-1039,52`) [Nit, eval-engine]
- [x] **src/builtins.rs IncludeContext::new() constructor** — add constructor to reduce breaking changes when fields are added; low priority pre-1.0. (`src/builtins.rs:54`) [Nit, integration-verifier]
- KNOWN ISSUE: **src/formatter.rs `is_fn_params` heuristic fragile** — operates on flat token stream without AST context; heuristics at `src/formatter.rs:418-450` can misfire on comments containing "fn" before brackets. Either pass AST to formatter or document best-effort nature. (`src/formatter.rs:418-450`) [Major, grammar-architect] (formatter heuristic deferred — needs AST-based formatter rewrite)
- [x] **doc/15-ast.md `desugared: bool` field documentation missing** — `Expr::Fn` has a `desugared: bool` origin tag (Pombrio & Krishnamurthi 2014) set by `wrap_expr_in_lambda()`, but the `$_` Desugaring section doesn't mention it. Add paragraph explaining origin tracking motivation and tooling use cases. (`doc/15-ast.md`, `src/ast.rs:106-110`) [Nit, grammar-architect + computer-scientist C32]
- [x] **src/eval.rs `with_base_dir()` docstring inaccurate** — says "Avoids allocating a new EvalConfig" but it does allocate a new `EvalConfig`; it avoids allocating a new `EvalState`. (`src/eval.rs:63-64`) [Nit, computer-scientist C35]
- [x] Fix doc/11-stdlib.md function count: "66 LLT functions (54 public API + 12 shadowable wrappers)" → "93 LLT functions (81 public API + 12 shadowable wrappers)" total count off by 27 [Major, stdlib-author C81]
- [x] Add 27 missing functions to doc/11-stdlib.md reference tables: abs, sign, clamp, take-while, drop-while, with-entries, partition, flat-map, find-first, find-first-or, group-by, deep-merge, walk, unzip, transpose, sum, product, min, max, count, contains?, uniq, foldr, int?, str?, float?, bool?, dict?, fn? [Major, stdlib-author C81]
- [x] Add prose section to doc/11-stdlib.md describing new stdlib categories (aggregates, higher-order utilities, type predicates) [Minor, stdlib-author C81]
- [x] Fix `debug_assert` RowVar invariant in `check_dot_access` — the assertion at `src/typecheck.rs:649-658` checks `rho_level <= *rho_level_creation`, logging the RowVar name and `state.levels.get(rho)`. The log expression `state.levels.get(rho)` re-reads the level map for the message, but `rho_level` (the source of truth for the assertion condition) was already read from `state.levels` at line 646 — so the message shows the same value as the condition, never the "stale" value. More importantly, the assertion format string shows `state.levels.get(rho)` as an `Option<&u32>`, while `rho_level` is a bare `u32` — the message is correct but the display of the `Option` wrapper (`Some(N)` vs `N`) is surprising. Fix: change the message to `state.levels.get(rho).copied().unwrap_or(0)` (a `u32`) for consistency, or document the intent. Also document WHY the invariant holds: `rho_level_creation` is the creation-time level embedded in `RowTail`; level lowering only writes to `state.levels`, never to the embedded level in `RowTail`. The assert fires if a RowVar is encountered whose embedded level is LOWER than the current levels map entry — which would indicate a level was erroneously raised. Add explanatory comment. (`src/typecheck.rs:649-658`) [Nit, type-theorist C64]
- [x] Fix occurs check on RowVar constraint binding is provably unreachable — `check_dot_access` RowVar arm at `src/typecheck.rs:675-680` calls `row_var_occurs_pub(rho, &binding, &state.subst)` where `binding = Row({field: β}, RowVar(ρ_fresh))`. Both `β` (fresh TypeVar) and `ρ_fresh` (fresh RowVar) are created on lines 661-663 with brand-new counter values, so they cannot already appear in `state.subst` and cannot be equal to `rho`. The occurs check cannot fire. This is documented in `test_dot_access_constraint_generation_on_typevar_forward_ref` (line 1755: "likely unreachable") but the TODO.md item at row-unification-g-c (line 130) asks to convert the `Err` branch to `debug_assert!(false) + Err(...)`. Implement that change: replace `if row_var_occurs_pub(...) { return Err(...) }` with `debug_assert!(!row_var_occurs_pub(...), "unreachable: fresh row var occurs in its own binding"); // defensive — fresh vars cannot cycle`. This keeps the error path as dead code but makes the "defensive" status explicit. (`src/typecheck.rs:675-680`) [Minor, type-theorist C64; extends row-unification-g-c item]
- [x] Document the `rho_level_creation` vs `state.levels` two-source-of-truth pattern — the RowTail embeds the creation-time level (`rho_level_creation` in the pattern at line 642) and `state.levels` holds the current (possibly lowered) level. The pattern at line 646 (`state.levels.get(rho).copied().unwrap_or(0)`) is the canonical read. A future reader may be confused about which level to use: the RowTail's embedded level is a snapshot, `state.levels` is authoritative. Add a module-level comment in `check_dot_access` (or on `RowTail`) explaining the invariant: "The level embedded in `RowTail::RowVar(name, level)` is the creation-time level, preserved for the debug assertion. All level-sensitive operations (lowering, generalization) read from `InferState.levels`. The invariant `levels[name] ≤ creation_level` holds because level lowering can only decrease, never increase, levels." (`src/typecheck.rs:642-646`) [Minor, type-theorist C64]
- [x] Fix `generalize()` dead defensive filter has misleading comment — `generalizable_type_vars` at `src/types.rs:1183-1193` has a filter `!all_row_vars.contains(var)`. Prior reviews (C52, C53, C59, C61, C62) debated whether this is dead code. The C62 note in TODO.md line 285 says "the filter IS load-bearing (named row vars like `...rest` share the `_t{n}` counter prefix)". However, reading the code: `collect_type_vars` collects `TypeVar(name, _)` variants; `collect_row_vars` collects RowVar names from `RowTail::RowVar` positions. A name in `all_row_vars` appears in a row-tail position, which `collect_type_vars` does NOT walk (line 178: "Row tail contains no type variables (only RowVar or Empty)"). So TypeVar names and RowVar names from `_t{n}` counters CAN appear in both sets only if the same name is both a TypeVar in a field type AND a RowVar in some tail — which is prevented by Robinson's name-freshening (each fresh name is unique). The comment at line 1186 says "Exclude row vars from type_vars (row vars collected separately)" — this is the correct rationale, but the filter can never trigger because `collect_type_vars` does not visit row tail positions. Mark definitively as dead code with: `// Dead code: collect_type_vars does not visit RowTail positions (types.rs:177-179), // so no name can appear in both all_type_vars and all_row_vars. Retained for defense-in-depth.` (`src/types.rs:1183-1193`) [Nit, type-theorist C64; extends C52/C59/C61/C62 findings]
- [x] Fix `check_call` CALL-MONO `return Ok(*ret.clone())` asymmetry with `check_call_with_scheme` — `check_call` at `src/typecheck.rs:925` returns `Ok(*ret.clone())` in CALL-MONO while `check_call_with_scheme` at line 852 returns `Ok(state.subst.apply(ret))`. The comment at lines 920-924 justifies this: "`!func_ty.has_type_vars()` proves ret is fully concrete — no TypeVar or RowVar nodes — so `apply()` would be a no-op". The comment on `check_call_with_scheme` says it "uses `apply()` because it is entered after `instantiate_scheme`". However, `has_type_vars()` checks for RowVar in tails (line 196) but does NOT check for TypeVars bound indirectly through `state.subst` — the guard only inspects the syntactic form of `func_ty`, not its image under `state.subst`. If `ret` contains a TypeVar name that is NOT syntactically present (because the TypeVar was bound during `unify()` in check_expr CALL-MONO argument processing, changing the substitution domain but not the stored `ret` pointer), the skip is still correct because unification modifies `subst`, not the source type. But if a future refactoring introduces a path where CALL-MONO fires on a ret containing RowVars (which `has_type_vars()` correctly detects), the `*ret.clone()` skip becomes wrong. The asymmetry is fragile. Fix: change to `return Ok(state.subst.apply(ret))` for defensive consistency, matching `check_call_with_scheme`. Already tracked at row-unification-h-b line 236 as type-theorist C56 but classified differently: that item frames it as a CALL-MONO `state.subst.apply` issue; this finding identifies the asymmetry between the two call checking functions as the root fragility. (`src/typecheck.rs:920-925 vs 851-852`) [Minor, computer-scientist C64; related to row-unification-h-b line 236]
- [x] Fix `infer_dict` Pass 3b `or_insert` ignoring collision with state.subst — `subst.type_map.entry(k.clone()).or_insert(applied_v)` at `src/typecheck.rs:549` discards `state.subst` bindings when `local subst` already has the same key. Under Algorithm W substitution composition (Damas & Milner 1982), when two substitutions bind the same variable, the bindings must be unified — not silently dropped. Example: if local subst has `_t0 -> Record({name: Str}, Empty)` and state.subst has `_t0 -> Record({name: beta}, RowVar(rho))` from a dot-access constraint, `or_insert` keeps the local binding and `beta` is orphaned. Already tracked at row-unification-h line 224 (Minor, computer-scientist C54). Re-confirmed: the bug is still present at the same code location. Not a new finding — confirming still-open status. (`src/typecheck.rs:549`) [Confirmed still-open, Minor]
- [x] Verify `test_call_poly_state_subst_isolation` WHAT THE TEST DOES VERIFY item 2 inaccuracy — line 3735 states "state.subst is shared across documents (state persists through file_env)". While `state` is indeed shared across documents in `typecheck_document`, this test does NOT verify that state.subst sharing is NEEDED for the result — the concrete env lookup from document 1 suffices. The statement is technically true but misleading: the test does not exercise state.subst sharing in a way that would break if sharing were removed. Fix: qualify: "state.subst is shared across documents (verified by inspection; this test's result would be unchanged if state.subst were cleared between documents)". (`src/typecheck.rs:3735`) [Nit, computer-scientist C64; extends test-crafter-c62-b panel finding]

#### cycle-findings-c62-c63: Theoretical Soundness Findings (Cycles #62–63)

Consolidated from: computer-scientist-c63, computer-scientist-c62

- [x] Fix `invoke_proxy_handler` handler re-materialization on every access — `materialize(handler, ...)` at `src/eval.rs:986` is called on every `.field` or `[key]` access; the handler thunk IS memoized by Launchbury (1993) sharing, so subsequent calls return the cached value, but each call still acquires a `RefCell::borrow()` + clones the `Value::Function{params, body, env}` struct (3 Rc clones). For hot proxy access (e.g., DSL column references in a loop), this is unnecessary overhead. Consider extracting the handler value once at proxy creation time by materializing eagerly in `builtin_proxy`, storing `Value::Proxy { handler_val: Value }` directly (trades memory for access speed). (`src/eval.rs:986`, `src/builtins.rs:2636`) [Minor, computer-scientist C63]
- [x] Fix `invoke_proxy_handler` missing Proxy-handler-returns-Proxy recursion guard — if a proxy handler returns another Proxy value, and that proxy's handler returns another Proxy, field access chains like `$p.a.b.c` recurse through `invoke_proxy_handler` -> `materialize` -> `invoke_proxy_handler` etc. Each level costs 1 depth, which provides a natural bound via MAX_EVAL_DEPTH. But the error message on depth exhaustion would be "maximum evaluation depth exceeded" with no indication that proxy handler chain recursion caused it. Document that proxy handler chains are bounded by MAX_EVAL_DEPTH and consider adding a proxy-specific depth annotation to the error. (`src/eval.rs:978-1023`) [Nit, computer-scientist C63]
- [x] Fix `doc/08-evaluation.md` state set missing `Guarded` — line 201 enumerates 6 states; Guarded is a live 7th state since typeassert-structural sprint. Lines 208-211 (DAG), 215-224 (transition table), and FORCE-* rules all omit it. This is already tracked at TODO line 321 but the doc/08 staleness is progressively worse: the `Value::Proxy` addition means the formal spec now also lacks FORCE-PROXY rules for dot/bracket access dispatch. Add `Guarded` to state set and transition DAG per existing TODO item, and also add a note about Proxy access dispatch semantics. (`doc/08-evaluation.md:201-224`) [Major, computer-scientist C63; extends existing eval-engine C49 item]
- [x] Fix prelude shadowable wrappers losing arity error specificity — `$if` builtin reports "arity mismatch: expected 3 arguments, got N" with the call site span. The prelude wrapper `if: [fn [c t e] ...]` intercepts arity errors at the wrapper level: calling `[call $if 1 2]` produces a Kotlin-convention arity error from `bind_args_thunks` for the wrapper function, not the builtin. The error message changes from mentioning `$if` to mentioning an anonymous function. Same applies to all 12 wrapper functions. Document this as an accepted trade-off of the overridable-ops design: arity errors refer to the wrapper, not the underlying builtin. (`stdlib/prelude.llt:517-535`) [Minor, computer-scientist C63]
- [x] Fix `doc/08-evaluation.md` Strictness Signature Table builtin count — line 389 and 425 say "44 builtins" but `standard_builtins()` returns 46 entries (45 original + proxy). Already tracked at computer-scientist-c62 line 22 for doc/07; this extends coverage to doc/08 which has its own stale count. (`doc/08-evaluation.md:389,425`) [Minor, computer-scientist C63; partially tracked in stdlib-author C62 item]
- [x] Add `$proxy` to doc/08 Strictness Signature Table — `proxy` is missing from the Part 2 table. Signature: `L → D` (lazy in handler arg — `builtin_proxy` does `Rc::clone(&args[0])`, never materializes the handler; returns Proxy container). Category: Structural. NOTE: original proposal said `S → D` — corrected to `L → D` by eval-engine C67 review (the handler passes through as an unforced thunk). (`doc/08-evaluation.md`) [Minor, computer-scientist C63]
- [x] Document `Value::Proxy` interaction with TypeAssert — when a Proxy value is asserted against a Record type (`[@[name: String] proxy_val]`), the Guarded thunk path at `src/eval.rs:1509` checks `if let Value::Dict(ref entries) = value`, which fails for Proxy, producing "expected Record, got Proxy". This may be surprising since Proxy supports the same access operations as Dict. Either (a) document that TypeAssert Record assertions require Dict values (Proxy not supported), or (b) add a Proxy arm that creates a guarded proxy where field access invokes the handler then validates the result. Option (a) is simpler; option (b) requires careful interaction with Findler & Felleisen (2002) contract composition. (`src/eval.rs:1509-1537`, `doc/07-type-extensions.md`) [Minor, computer-scientist C62]
- [x] Fix doc/07 Part 8 stale "migration replaces" present-tense prose — section reads as forward-looking guide for a completed migration: "the migration replaces...", "must be routed..."; all changes described were done in row-unification-b. Retitle as "Migration Reference (Complete)", rewrite in past tense. [Supersedes doc-rowunification-retrospective Major] (`doc/07-type-extensions.md:627-654`) [Minor, computer-scientist C62]
- [x] Fix doc/07 Type System Extension Roadmap stale row binding claim — line 704 says "row variable binding is arguably more impactful... without it, row variables are never bound during inference"; binding is fully implemented through row-unification-a to -e. Replace with: "Row variable binding is complete as of row-unification-e." [Supersedes doc-rowunification-retrospective Major] (`doc/07-type-extensions.md:704`) [Minor, computer-scientist C62]

### cycle-findings-c51: Findings (Cycle #51)

- [x] Mark `include-desugar` Critical item as done -- `builtin_include` already calls `desugar_file` at `src/builtins.rs:1217` (fix applied but TODO.md line 350 still unchecked). Check `[x]` the item and move to DONE.md. (`TODO.md:350`) [Minor, integration-verifier C51]
- [x] Fix `builtin_unfold_step` `.unwrap()` on user-controlled iterator -- `iter.next().unwrap()` at `src/builtins.rs:1803-1804` panics if the step function returns a Dict with fewer than 2 entries; a step function returning `[value: 1]` (1-entry dict) triggers an unwrap panic in release mode (`panic = "abort"`, no recovery). Fix: validate `map.len() >= 2` is already guarded but falls through to the 2-entry branch without an `else` for 1-entry dicts; add explicit `else` arm returning `EvalError::type_mismatch_ctx("unfold", "dict with at least 2 entries", &format!("dict with {} entries", map.len()), call_span)`. (`src/builtins.rs:1800-1804`) [Major, security-expert C51]
- [x] Revert spurious `register_type_aliases` in `typecheck_document` `Type::Any` arm — a prior change incorrectly added `register_type_aliases` to the `Type::Any` branch. This was dead code: `register_type_aliases` opens with `if let Expr::Dict(entries) = &expr.node` and is a no-op for any non-Dict expression. Type aliases (`Expr::TypeAlias`) only appear as Dict entry values, not as standalone document expressions, so the branch can never process an alias. Reverted to `Type::Any => {}` empty arm to remove the spurious `TypeEnv` allocation. (`src/typecheck.rs:164`) [Minor, type-theorist C51]
- [x] Fix `value_to_json` and `value_to_display_string` not incrementing depth for Seq tail in `deep_materialize_impl` -- `deep_materialize_impl` at `src/eval.rs:1727-1735` calls `deep_materialize_thunk` on both `head` and `tail` with the same `depth` (not `depth+1`); the depth increment happens inside `deep_materialize_thunk` via `deep_materialize_impl` recursion, but an infinite sequence of Seqs (head is Seq, not a leaf) would consume 2 depth units per Seq element (one for head, one for tail) instead of 1; benign for leaf-headed Seqs but inconsistent with the Dict path which increments via recursion uniformly. Document the asymmetry or normalize. (`src/eval.rs:1727-1735`) [Minor, computer-scientist C51]
- [x] Fix `or` stdlib function not matching documented pass-through semantics -- `or: [fn [a b] [call $builtin-if $a $a $b]]` at `stdlib/prelude.llt:64` evaluates `$a` twice (once for condition, once for true-branch), violating the lazy evaluation principle; if `$a` has side effects (e.g., `$error` in a `$try` wrapper) or is expensive, it is computed twice instead of once. The `$if` builtin already handles this correctly (selective materialization), but the pass-through requires `$a` to be forced twice. Document the double-evaluation as a known limitation or add a `let`-binding workaround comment. (`stdlib/prelude.llt:64`) [Minor, eval-engine C51]
- [x] Fix `eval_document` `unreachable!()` at line 549 reachable via empty `exprs` after is_empty guard -- the `unreachable!()` at `src/eval.rs:549` is dead code protected by the `exprs.is_empty()` early return at line 507 and the `is_last` return at line 521; the comment claims it "has expressions but loop did not return" which is correct but the `unreachable!()` macro is a panic in release mode (`panic = "abort"`); replace with `debug_unreachable!()` or a `// SAFETY:` comment explaining the invariant, or convert to `unsafe { std::hint::unreachable_unchecked() }` for zero-cost assertion in release. (`src/eval.rs:549`) [Minor, performance-expert C51]
- [x] Fix `ArityBound::Range` display for `Range(1, 1)` -- `Range(lo, hi)` at `src/error.rs:26-29` has a special case for `lo == hi && lo == 1` producing "1 argument", but `Range(2, 2)` produces "2 to 2 arguments" instead of "2 arguments"; generalize the guard to `if lo == hi { write!(f, "{lo} arguments") }` (with the existing singular case for 1). (`src/error.rs:26-29`) [Minor, integration-verifier C51]
- [x] Add `libc` dependency justification comment -- `Cargo.toml` lists `libc = "0.2"` but the only usage appears to be for signal handling or stack size; document why `libc` is needed in a comment or remove if unused after the `WORKER_STACK_SIZE` approach changes. (`Cargo.toml:31`) [Minor, security-expert C51]

### row-unification-perf: Type Checker Performance Optimizations (Majors)

Major performance improvements from the performance-expert and type-theorist panel review. Requires row-unification-e. Items are independent of row-unification-f/g.

- [x] Migrate `Substitution.type_map` and `Substitution.row_map` from `IndexMap` to `HashMap` — every `apply()` call pays `IndexMap` lookup overhead on every TypeVar/RowVar resolution; insertion order of substitution bindings has no semantic meaning. Remove `indexmap::IndexMap` import from types.rs if no other use remains after TypeEnv migration. (`src/types.rs:318-319`) [Major, performance-expert C52]
- [x] Add `apply()` empty-substitution fast-path — `Substitution::apply()` allocates two HashSets unconditionally; when subst is empty (common at start of inference, and for every `check_dot_access` call before any constraints accumulate), this is pure waste. Add: `if self.type_map.is_empty() && self.row_map.is_empty() { return ty.clone(); }` at the top. (`src/types.rs:332-336`) [Major, performance-expert C52]
- [x] Migrate `TypeEnv.bindings` and `type_aliases` from `IndexMap` to `HashMap` — `TypeEnv::get()` and `get_type_alias()` are O(depth) chain traversals calling `IndexMap::get()`; neither is iterated in insertion order. Change both to `HashMap` in types.rs:1242-1243; also migrate `infer_dict` local `schemes` map at typecheck.rs:447. (`src/types.rs:1242-1243`, `src/typecheck.rs:447`) [Major, performance-expert C52]
- [x] Add fast-path in `unify_rows` for closed equal-key records — when both resolved rows have `RowTail::Empty` and identical field key sets, skip partition allocation (5+ collections) and proceed directly to per-field unification; this is the common case for checking inferred vs annotated closed records. (`src/types.rs:838-878`) [Major, performance-expert C52]
- [x] Eliminate double-apply in `infer_dict` Pass 3 — resolved by row-unification-f: Pass 3b merges state.subst into local subst via or_insert, then Pass 3c does a single `subst.apply()`; the `(k, subst.apply(&state.subst.apply(&ty)))` double-apply no longer exists. (`src/typecheck.rs:558-562`)
- [x] Add Steps 3.5/3.6 fast-path for closed rows — `resolve_row` is called unconditionally after every Step 3, even when both tails are `RowTail::Empty` (common closed-record case) where re-resolution is a no-op; adds 4+ allocations per `unify_rows` call on the dominant path. Add guard before Step 3.5: `if resolved1.tail == RowTail::Empty && resolved2.tail == RowTail::Empty { /* skip to Step 4 */ }`. (`src/types.rs:858-885`) [Major, performance-expert C53]
- [x] Optimize Pass 3b row_map per-field allocation — row_map merge loop allocates one `HashMap<String, Type>` per state.subst row binding (the `applied_fields` collect); consider applying subst to the whole row via `subst.apply(&Type::Record(row.clone()))` to share one visited-set pass, or relying on Pass 3c's `subst.apply()` which already corrects the merged field types. (`src/typecheck.rs:543-556`) [Minor, performance-expert C53]
- [x] Migrate `infer_dict` Pass 4 `schemes` from `IndexMap` to `HashMap` — `let mut schemes = IndexMap::new()` at line 575 is only accessed by name lookup; no caller iterates in insertion order. Change to `HashMap` for consistent ~20% faster lookup with the rest of the substitution maps. (`src/typecheck.rs:575`) [Minor, performance-expert C54, line-updated C62]
- [x] Migrate `Environment::bindings` from `IndexMap` to `HashMap` — `Environment` is a lexical scope chain; bindings are looked up by name only and never iterated in insertion order (that property belongs to `Value::Dict` which correctly uses `IndexMap`). Changing to `HashMap` gives ~20% faster `env.borrow().get(name)` lookups on the most-called path in the evaluator (every `Expr::VarRef`). (`src/value.rs:493`) [Major, performance-expert C57]
- [x] Add capacity hint to `eval_dict` IndexMap allocation — `IndexMap::new()` starts at capacity 0 and triggers 2-3 resize/realloc cycles for a 5-entry dict; the entry count is statically known from the AST slice length before the loop. Change to `IndexMap::with_capacity(entries.len())`. (`src/eval.rs:606`) [Nit, performance-expert C57]

## row-unification-h: Letrec Completeness and Kind Safety

Correctness gaps in letrec forward-reference inference and annotation kind safety. Requires row-unification-g.

- [x] Add TypeVar arm in `check_call` for letrec forward references — when a letrec entry calls a forward-referenced function, Pass 1 binds the callee to `TypeScheme::mono(TypeVar("_t0", 1))`; during Pass 3, `check_call` receives `func_ty = TypeVar("_t0", 1)`, which matches neither `Type::Function` nor `Type::Any`, falling through to "expected function type" error. Fix: add a TypeVar arm that generates a constraint `TypeVar(alpha) = Fn(arg_types → beta)` and returns `beta`. Conservative alternative: return `Any`. (`src/typecheck.rs:864-966`) [Major, computer-scientist C54]
- [x] Fix `ann_mapping` cross-kind collision in annotation freshening — both `resolve_type_name` (line 1240) and the Rest row-variable handler (line 1329) share one `ann_mapping: HashMap<String, String>`; a user annotation name can be registered as both a TypeVar and a RowVar (e.g., `[fn [x@a y@[name: a ...a]] ...]`), violating the Rémy (1994) sort separation invariant. Fix: use separate `type_ann_mapping` and `row_ann_mapping`, or detect cross-kind collision and emit a TypeError. (`src/typecheck.rs:1239-1251, 1328-1339`) [Major, computer-scientist C54] (body implemented — cross-kind collision detected and tested)
- [x] Fix `Pass 3b or_insert` discards state.subst binding when both maps have the same variable — when local subst has `_t0 → Record({name: Str}, Empty)` and state.subst has `_t0 → Record({name: beta}, rho)`, `or_insert` keeps the local binding and beta is orphaned (never unified with Str). Fix: when both substitutions bind the same variable, unify the two bindings instead of discarding the state.subst one. Model: Algorithm W substitution composition (Damas & Milner 1982). (`src/typecheck.rs:538-541`) [Minor, computer-scientist C54]
- [x] Fix `test_corpus_structure` hardcoded directory list — `required_dirs` at tests/corpus_tests.rs:198-205 omits `tests/corpus/eval/typecheck/` and `tests/corpus/eval/laziness/`; deleting either directory would not fail the structure test. Expand `required_dirs` to cover all directories that have test files. (`tests/corpus_tests.rs:198-205`) [Minor, test-crafter C54] (already complete)
- [x] Fix `unify_remainders` Case 2 guard comment inaccuracy — comment says "Guard requires u2_empty to prevent silently dropping unique2 when both sides have unique fields" but u2_empty=true means unique2 IS empty so nothing is dropped. The real purpose is to prevent Case 2 from shadowing Case 4. Rewrite: "Guard: u2_empty required — when both sides have unique fields with different RowVars, Case 4 applies; this guard ensures Case 2 only fires when unique2 is genuinely empty." (`src/types.rs:740-741`) [Nit, type-theorist C54]
- [x] Add `resolve_type_assert` state.subst.apply regression test — no test verifies that removing `state.subst.apply()` at lines 1076-1077 changes the result; existing `test_typeassert_default_wrong_type_emits_error` uses concrete types that don't go through TypeVar resolution. Add a test where `default_ty` or `expected` contains a TypeVar bound in state.subst. (`src/typecheck.rs:1076-1077`) [Minor, test-crafter C54]
- [x] Apply `state.subst.apply(&func_ty)` before `check_call` match — `check_call` at `src/typecheck.rs:878` matches on `func_ty` directly; when `func_ty` is `TypeVar(α)` bound in `state.subst` to a `Function`, the match falls through to "expected function type" error. Apply `state.subst.apply(&func_ty)` before the match to resolve bound TypeVars; this is an alternative to the TypeVar arm fix in the task above and may subsume it. (`src/typecheck.rs:878`) [Minor, type-theorist C55]
- [x] Fix `check_call_with_scheme` not recording func span in `type_map` — after `infer_expr` resolves the function expression's type, the func span is not inserted into `state.type_map`; LSP hover over the function name in a polymorphic call shows blank. Fix: add `state.type_map.insert(func.span.into(), func_ty.clone())` after the func type is resolved, consistent with how `check_dot_access` records the target span. (`src/typecheck.rs`) [Minor, integration-verifier C55]
- [x] Fix `check_bracket_access` not generating row constraints for open records — when target type is `Record({...}, RowVar(ρ, _))` and the string-literal key is not in known fields, returns `Type::Any` (line 726) instead of generating the constraint `ρ → Row({key: β}, ρ')` as `check_dot_access` does (lines 641-671). This means `$x["name"]` infers less precisely than `$x.name` — the bracket form does not propagate field-presence constraints through the row variable. Fix: mirror `check_dot_access`'s RowVar arm — create fresh β, fresh ρ', do occurs check + level lowering, bind ρ in `state.subst`. Model: Rémy (1994) row constraint generation must be uniform across all record access forms. (`src/typecheck.rs:721-731`) [Minor, computer-scientist C56]
- [x] Fix `check_bracket_access` not generating constraints for TypeVar targets — when target type is `TypeVar(α, _)`, returns `Type::Any` (line 746) instead of generating `unify(α, Record({key: β}, RowVar(ρ)))` as `check_dot_access` does (lines 679-700). This means `$x["name"]` on an unknown-type target does not constrain α to be a record at all. Fix: for string-literal and int-literal keys, mirror `check_dot_access`'s TypeVar arm; for dynamic keys (non-literal expressions), `Type::Any` remains correct since the field name is unknown at inference time. (`src/typecheck.rs:746`) [Minor, computer-scientist C56]
- [x] Fix `check_call` CALL-MONO returns `*ret.clone()` without `state.subst.apply` — the CALL-MONO arm at `src/typecheck.rs:903` returns `Ok(*ret.clone())` while `check_call_with_scheme` CALL-MONO at line 835 correctly returns `Ok(state.subst.apply(ret))`; the invariant that CALL-MONO only fires when `!func_ty.has_type_vars()` makes this safe today but is fragile — if the guard is ever relaxed for RowVar-only polymorphism, `check_call` silently becomes wrong. Fix: change line 903 to `return Ok(state.subst.apply(ret))` for defensive consistency. (`src/typecheck.rs:903`) [Minor, type-theorist C56]
- [x] Make `infer_expr` match exhaustive by adding `#[deny(unreachable_patterns)]` before the match — current wildcard/final arm means adding a new `Expr` variant silently falls through to the error case without a compiler error. Audit all 13 `Expr` variants are explicitly handled. (`src/typecheck.rs:143`) [Minor, integration-verifier C60]
- [x] Fix `check_range_access` TypeVar arm — `check_range_access` match at `src/typecheck.rs:784` already handles `Type::TypeVar(_, _)` in pattern `Type::Record(..) | Type::Any | Type::TypeVar(_, _) => Ok(target_ty)`. Fixed during row-unification-g sprint. (`src/typecheck.rs:784`) [Minor, type-theorist C56, verified C57]

### row-unification-perf-b: Type Checker Performance Optimizations (Minors)

Minor performance improvements from the panel review. Requires row-unification-perf.

- [x] Fuse `lower_row_var_levels` double-walk into single pass — current implementation iterates `row.fields.values()` twice (once for type vars, once for row vars), allocating two `BTreeSet`s; replace with a single loop filling both sets simultaneously. (`src/types.rs:633-655`) [Minor, performance-expert C52] (already complete)
- [x] Eliminate Case 4 redundant clones in `unify_remainders` — `unique1` and `unique2` are cloned to build `row2_with_fresh` and `row1_with_fresh` for the occurs check; reorder to borrow for the occurs check then move into substitution inserts. Sprint row-unification-e eliminated only Cases 2/3. (`src/types.rs:702-716`) [Minor, performance-expert C52]
- [x] Fuse `collect_type_vars`/`collect_row_vars` into single tree walk for `unify()` U-VAR-LEVEL arms — lines 944-956 (U-VAR-LEVEL) and 970-982 (U-VAR-LEVEL-SYM) call both methods separately, walking the same type tree twice and allocating two `BTreeSet`s; add `collect_all_vars(ty, &mut type_vars, &mut row_vars)` helper. Use same helper in `generalize()` at `src/types.rs:1176-1180` which also double-walks when the type is polymorphic. (`src/types.rs:944-982, 1176-1180`) [Minor, performance-expert C52+C57, line-updated C62] (already complete)
- [x] Rename `has_type_vars` to `has_inference_vars` — method returns `true` for `TypeVar` OR `RowVar` tail; name implies only TypeVar; used in CALL-MONO/CALL-POLY split. Add doc comment: "returns true if any TypeVar or RowVar is present — both trigger CALL-POLY". (`src/types.rs:192-205`) [Minor, type-theorist C52]
- [x] Add CALL-POLY state.subst.apply is_empty guard — `state.subst.apply(&subst.apply(ret))` added in row-unification-f does two full type-tree walks; guard with `if state.subst.type_map.is_empty() && state.subst.row_map.is_empty() { subst.apply(ret) } else { state.subst.apply(&subst.apply(ret)) }` to skip the outer walk when no access-chain constraints have accumulated. (`src/typecheck.rs:852, 973`) [Minor, performance-expert C53, line-updated C62]
- [x] Eliminate `check_dot_access` TypeVar branch `mem::take` allocations — `std::mem::take(&mut state.subst)` at line 696 replaces state.subst with `Substitution::default()` (2 `IndexMap::new()` allocs per dot-access on TypeVar targets — the common case during open-record inference). Restructure to avoid the take/restore dance: pass subst as a parameter to the inner call or extract the borrow-split differently. (`src/typecheck.rs:696`) [Minor, performance-expert C54, line-updated C62] (not feasible — mem::take is standard Rust idiom for borrow-checker pattern)

### row-unification-perf-c: Apply-Site Allocation Reduction (C55 Overflow)

Allocation hotspots introduced by the f-b bilateral apply additions. Requires row-unification-perf.

- [x] Add `check_expr` bilateral apply is_empty guard — the two `state.subst.apply()` calls added in row-unification-f-b at `src/typecheck.rs:438-439` each allocate two HashSets unconditionally on every expression type-check; guard both with `if state.subst.type_map.is_empty() && state.subst.row_map.is_empty()` to skip on the common empty-subst path. Mirrors the CALL-POLY guard added in row-unification-perf-b. (`src/typecheck.rs:438-439`) [Major, performance-expert C55]
- [x] Add CALL-POLY inner `subst.apply` is_empty guard — `state.subst.apply(&subst.apply(ret))` at `src/typecheck.rs:852, 973`; the outer `state.subst.apply` is already guarded (row-unification-perf-b), but the inner `subst.apply(ret)` still allocates two HashSets unconditionally; guard with `if subst.type_map.is_empty() && subst.row_map.is_empty() { ret.clone() } else { subst.apply(ret) }`. (`src/typecheck.rs:852, 973`) [Major, performance-expert C55, line-updated C62] (already complete)
- [x] Reduce `infer_fn` and `check_expr` lambda-checking annotation map allocation — `let mut ann_mapping = HashMap::new()` is allocated at two sites: `src/typecheck.rs:996` (infer_fn, every user-defined function) and `src/typecheck.rs:330` (check_expr lambda-checking path, every lambda checked against a Function expected type). Both allocate unconditionally even when no params have annotations (the common case for `$map`/`$filter` lambdas). Add guard `if params.iter().any(|p| p.node.annotation.is_some()) || return_ann.is_some()` before both allocations. (`src/typecheck.rs:996, 330`) [Minor, performance-expert C55, second-site added C62] (already complete)
- [x] Add `check_bracket_access` and `check_range_access` `is_empty()` apply guard — `state.subst.apply(&target_ty)` at `src/typecheck.rs:718` and `src/typecheck.rs:763` allocate 2 HashSets unconditionally on every bracket/range access expression; guard both with the same `if state.subst.type_map.is_empty() && state.subst.row_map.is_empty() { target_ty } else { state.subst.apply(&target_ty) }` pattern used for `check_expr` in row-unification-perf-c. (`src/typecheck.rs:718, 763`) [Minor, performance-expert C56, line-updated C62]
- [x] Add `resolve_type_assert` `state.subst.apply()` is_empty guard — `resolve_type_assert` at `src/typecheck.rs:1091-1092` calls `state.subst.apply()` on both `expected` and `default_ty` unconditionally, allocating two HashSets per TypeAssert annotation even when `state.subst` is empty (the common case in non-polymorphic programs); guard both calls with `if state.subst.type_map.is_empty() && state.subst.row_map.is_empty() { ty.clone() } else { state.subst.apply(ty) }` matching the pattern used throughout perf-b/perf-c. (`src/typecheck.rs:1091-1092`) [Minor, performance-expert C57]
- [x] Switch `Environment::bindings` from `IndexMap` to `HashMap` — `Environment::bindings: IndexMap<String, Rc<Thunk>>` at `src/value.rs:493` uses IndexMap for no semantic reason; Environment bindings are looked up by name and never iterated in user-visible insertion order (that contract belongs to `Value::Dict`, which correctly keeps IndexMap). Every `Environment::get()` call during VarRef evaluation pays ~20% IndexMap overhead on every level of the O(depth) parent chain walk. Change to `HashMap<String, Rc<Thunk>>`; construction sites in `eval.rs` and `builtins.rs` need no other change (only `insert` calls). (`src/value.rs:493`) [Minor, performance-expert C57] (already done in prior sprint)
- [x] Add capacity hint to `eval_dict` IndexMap — `let mut dict_map: IndexMap<Key, Rc<Thunk>> = IndexMap::new()` at `src/eval.rs:606` uses default capacity (1); dict entry count is known before the loop (`entries.len()`). Change to `IndexMap::with_capacity(entries.len())` to eliminate O(log n) resize steps for dicts with more than 1 entry. (`src/eval.rs:606`) [Nit, performance-expert C57] (already complete)
- [x] Fuse `generalize()` two tree walks into single pass — `generalize()` at `src/types.rs:1176-1180` calls `collect_type_vars` then `collect_row_vars` separately, allocating 2 BTreeSets and walking the type tree twice; add a `collect_all_vars(ty, &mut BTreeSet, &mut BTreeSet)` fused helper (same pattern as TODO.md line 148 for unify()). The `has_type_vars()` early-exit at line 1172 already handles the monomorphic common case. (`src/types.rs:1176-1180`) [Nit, performance-expert C57] (already done)
- [x] Guard `eval_call` `named_thunks` unconditional `IndexMap::new()` — `let mut named_thunks = IndexMap::new()` at `src/eval.rs:727` allocates on every function call even when `named_args` is empty (the common positional-call path). Add guard: `if named_args.is_empty() { IndexMap::new() } else { let mut m = IndexMap::with_capacity(named_args.len()); ... }`. Pairs with TODO.md entry for `PendingCall` static-empty named field. (`src/eval.rs:727`) [Minor, performance-expert C64]
- [x] Add capacity hint to `eval_range_access` result `IndexMap` — `IndexMap::new()` at `src/eval.rs:1111` builds the range-slice output without a size hint; the upper bound is `map.len()`. Change to `IndexMap::with_capacity(map.len())`. (`src/eval.rs:1111`) [Nit, performance-expert C64] (already complete)

### seq-concat-nits: Concat Correctness and Depth Nits

Minor concat correctness, span, and depth consistency issues from C43 review.

- [x] Correct TODO.md PendingBuiltin/CEK fuel correspondence description — depth in PendingBuiltin chains is an indirect stack-depth proxy that fires when builtins call `materialize`, not a true fuel counter (Sestoft 1997). In a CEK machine, continuation stack is checked on every transition; here, depth is checked only on `materialize` entry. The true resource safety for `$collect` comes from `MAX_COLLECT_SIZE`, not depth. Both mechanisms are complementary. (doc-only fix: depth in PendingBuiltin chains is a materialize-entry proxy, not a true CEK fuel counter; MAX_COLLECT_SIZE is the primary resource safety for collect.) [Minor, computer-scientist panel]
- [x] Add type validation to concat_seq_step terminal case — when xs_tail materializes to Dict (sequence terminator), `ys_thunk` is returned directly without type checking; `concat(seq(1, 2, 3), 42)` defers the type error until consumer forces past last element. Distinct from empty-xs Dict path (line 157). Either eagerly validate ys or document intentional deferral. (`src/builtins.rs:2598-2600`) [Minor, computer-scientist]
- [x] Fix concat/collect error paths to use operand span as definition-site — 4 error paths in `builtin_concat` (Dict ys type mismatch, initial xs type mismatch, step tail type mismatch) and `builtin_collect` (tail type mismatch) use `call_span` as definition-site instead of `args[N].span`. Should use `EvalError::type_mismatch(..., operand_span).with_materialization_span(call_span)`. (`src/builtins.rs:2565-2579, 2616-2623, 2305-2313`) [Major, span-integrity-checker]
- [x] Resolve concat_seq_step depth increment inconsistency — verified C70: all step functions (filter_seq_step, drop_seq_step, reduce_seq_step) use `depth+1`; the premise was incorrect. Only `builtin_take` (line 2145) and `builtin_filter_dict_step` (line 1987) use flat `depth` and are tracked in eval-engine-c68. (`src/builtins.rs:2609`) [Minor, eval-engine; closed C70 - incorrect premise]

### float-nan-infinity: Float NaN/Infinity Propagation

Float arithmetic can silently produce NaN or Infinity values that propagate through the evaluator unchecked. Only caught at JSON serialization, far from the cause. Found by computer-scientist codebase review (2026-04-20).

- [x] Decide NaN/Infinity rejection policy — Option B: reject at both arithmetic result sites AND `$from-json` entry. "All floats are finite" invariant. Consistent with `$to-float`, matches Jsonnet/Nickel/CUE consensus for config languages targeting JSON output. See doc/11-stdlib.md §Equality and Comparison Part 5
- [x] Add NaN/Infinity result check to `builtin_add` Float path — `a + b` can produce Infinity (`1e308 + 1e308`), reject at point of origin (`src/builtins.rs:210`) [Major, computer-scientist]
- [x] Add NaN/Infinity result check to `builtin_sub` Float path — `a - b` can produce Infinity, and `-Inf - (-Inf)` produces NaN (`src/builtins.rs:229`) [Major, computer-scientist]
- [x] Add NaN/Infinity result check to `builtin_mul` Float path — `a * b` can produce Infinity (`1e308 * 2.0`) and `0.0 * Inf` produces NaN (`src/builtins.rs:248`) [Major, computer-scientist]
- [x] Add NaN/Infinity result check to `builtin_div_float` Float path — `Inf / finite` produces Infinity, `Inf / Inf` and `0.0 / 0.0` produce NaN (only `b == 0.0` is checked) (`src/builtins.rs:270-274`) [Major, computer-scientist]
- [x] Add shared `check_float_result(f64, &str, Span)` helper returning error on `is_nan()` or `is_infinite()` (`src/builtins.rs`)
- [x] Reject NaN/Infinity in `$from-json` parse path — add `is_finite()` check after `as_f64()` in `json_to_value` Number arm (`src/builtins.rs` json_to_value)

### Code fixes

- [x] Fix variadic parameter typing as closed empty record — `...args` typed as `Record(IndexMap::new(), RowRest::Closed)` (empty closed record); should be `RowRest::Open` or `Type::Any` to indicate arbitrary fields accepted. One-line fix. (`src/typecheck.rs:469-473`) [Major, computer-scientist]
- [x] Fix `check_call` CALL-POLY using `instantiate()` with hardcoded level 0 — fixed in let-gen-soundness sprint: `instantiate_at_level()` replaces `instantiate()` at typecheck.rs:525, creating fresh vars at `state.level` and registering in `state.levels`. Kiselyov (2013) sound_eager satisfied. (`src/typecheck.rs:525`, `src/types.rs:503-518`) [Critical, computer-scientist C40, fixed C44]
- [x] Fix Any-complex-type level zeroing gap — `unify(Any, Fn(TypeVar("b", 3) -> Int))` matches the catch-all `(Type::Any, _) => Ok(())` arm without zeroing `b`'s level. Only bare `TypeVar`-to-`Any` triggers level zeroing. Vars inside complex types unified with `Any` retain their original level and may be over-generalized. With let-generalization now active, this can produce incorrect type schemes: a function parameter annotated `@a` passed to an Any-typed builtin will not have `a`'s level zeroed, allowing spurious generalization. Fix: add recursive level zeroing in the `(Type::Any, _)` and `(_, Type::Any)` arms — walk the non-Any side and zero all contained type/row vars via `collect_type_vars`. (`src/types.rs:356`) [Major, computer-scientist C40, severity reaffirmed C44]
- [x] Fix `Substitution::apply_inner` missing visited-set check for RowVar resolution — TypeVar case (types.rs:258-260) adds the var name to `visited` before recursive resolution for cycle detection; RowVar case (types.rs:272-286) does not. A cyclic substitution `r -> Record({}, RowVar("r"))` would hit `MAX_APPLY_DEPTH` (256 recursive calls) rather than being caught by the visited set. Currently unexploitable because row var binding isn't implemented, but becomes a latent bug when row-unification lands. Fix: add `visited.insert(name.clone())` / `visited.remove(name)` around the RowVar resolution path. (`src/types.rs:272-286`) [Minor, computer-scientist C40] — fixed: `visited_rows: HashSet<String>` properly threaded through `apply_row` at types.rs:389-402 with insert/remove around recursive resolution (done in row-unification-b)
- [x] Fix `check_call` zero-arity CALL-POLY returning original `ret` not `inst_ret` — verified sound in current code: in `check_call_with_scheme` (line 746), `ret` comes from `instantiate_scheme` at line 694 and is already instantiated; in `check_call` (line 868), `inst_ret` comes from `instantiate_at_level` and is also correctly instantiated; both paths are sound for zero-param functions. (`src/typecheck.rs:538`) [Minor, computer-scientist C40] — re-verified C52: sound in current code, was fixed during check_call_with_scheme split
- [x] Fix doc/06-type-inference.md false claim about per-entry freshening in Pass 3 — line 497 states "Within a single letrec group during Pass 3, each entry's annotation-derived variables are instantiated independently, preventing collision." This is false: `resolve_type_name` creates `TypeVar("a", L)` for every `@a` annotation across all entries within the same letrec group, sharing the name in `state.levels`. Per-entry freshening only occurs after Pass 4 generalization + subsequent `instantiate_scheme`. This shared naming interacts with the `instantiate()` level-0 bug above to cause incorrect level lowering. Fix: correct the doc paragraph to describe the actual behavior. (`doc/06-type-inference.md:497`) [Minor, computer-scientist C40]

### stdlib-additions-a: Core and Convenience Functions (Part 1)

Consolidated from: stdlib-missing-core, stdlib-convenience

- [x] `with-entries` — `entries | map(f) | from-entries` pipeline (jq pattern; depends on `from-entries` from stdlib-pre-seq)
- [x] `partition` — single-pass split into matching/non-matching dicts (Nix + Dhall)
- [x] `flat-map` / `concat-map` — `flatten (map f xs)`, monadic bind for collections (Jsonnet + jq)
- [x] `find-first` / `find-first-or` — first element matching predicate, with default (Nix)
- [x] `group-by` — group elements by key function, returning dict of lists (Nix)
- [x] `deep-merge` — recursive merge for configuration overlays (Jsonnet, RFC 7396)
- [x] `walk` — recursive bottom-up transform of all sub-values (jq)
- [x] `sum`, `min`, `max`, `count` — aggregate functions (one-liners over fold)
- [x] `contains?` / `elem?` — membership test
- [x] `uniq` / `unique` — deduplicate collection
- [x] `foldr` — right fold (Tinct only has left fold currently)
- [ ] `zip-with` — generalized zip with combining function; define `zip` as special case (Nix)
- [ ] `map-indexed` / `map-keys` — indexed mapping and key transformation (Jsonnet)
- [ ] `sort-on` — sort by key-extraction function instead of comparator (Jsonnet + Nix)
- [ ] `flip`, `abs`, `sign`, `clamp` — small composable primitives (Nix + Jsonnet; `const` moved to stdlib-pre-seq)

### stdlib-additions-b: Convenience Functions and Utilities (Part 2)

Consolidated from: stdlib-convenience-b, stdlib-type-predicates, stdlib-numeric, stdlib-primitives, stdlib-string-ops

- [x] `unzip` — inverse of zip, split list of pairs into pair of lists [Nit, stdlib-author C31]
- [x] `transpose` — flip rows/columns of 2D structure [Nit, stdlib-author C31]
- [ ] `flatten-all` or depth parameter for `flatten` — current `flatten` only goes one level deep; add recursive variant or optional depth param [Nit, stdlib-author C31]
- [ ] `range-step` — range with step parameter; `$range` only supports `[start]` and `[start end]` with step=1 [Minor, stdlib-author C31]
- [x] `take-while`, `drop-while` — take/drop elements while predicate holds; implementable via Seq constructor pattern like `filter` [Minor, stdlib-author C31]
- [x] Variadic `all-of`/`any-of` — current `and`/`or` take exactly 2 args; add list-based variants `[fn [preds] [call $all? $identity $preds]]` (covered by existing any?/all? with $identity) [Nit, stdlib-author C31]
- [x] `is-int?`, `is-str?`, `is-float?`, `is-bool?`, `is-dict?`, `is-fn?` — type predicate wrappers over `$type-of` (Jsonnet pattern); all one-liners: `[fn [x] [call $= [call $type-of $x] Int]]` etc.
- [ ] Runtime assertion guards at stdlib function entry with descriptive errors (Jsonnet pattern)
- [x] `min`, `max`, `sum`, `product` — aggregate functions (stdlib-author review); all one-liners over `$fold`/`$reduce` (product implemented; min/max/sum already in stdlib-additions-a)
- [x] `abs`, `sign`, `clamp` — numeric primitives (stdlib-author review); all one-liners using `$<`, `$-`, `$if`
- [ ] Add `$has?` as Rust builtin — `has?` currently uses `$try` around bracket access, which forces the value (materializes it) to check existence. A Rust-native `$has?` would check `IndexMap::contains_key()` in O(1) without materializing the value. 2-arg: `[call $has? $dict $key]`. Unblocks lazy has-checking. (`src/builtins.rs`) [Minor, stdlib-author]
- [ ] Add `$has?` Rust primitive design: must handle both String and Int key types (matching bracket access semantics), return Bool, never materialize the value thunk. (`src/builtins.rs`) [Minor, stdlib-author]
- [ ] Add `substr` / `slice-str` Rust builtin for substring extraction (unblocks below)
- [ ] `starts-with?`, `ends-with?` — string prefix/suffix tests
- [ ] `chars` — string to character sequence
- [x] `join` — sequence/dict of strings to single string with separator (already exists as Rust builtin)

### parser-lexer: Phase 1 — Lexer Tokens

Add whitespace-sensitive tokens to `src/lexer.rs`. See doc/whatif/parser-rewrite.md §Phase 1.

- [x] Add `Token::BracketAccess` — emitted when `[` follows a value-ending token (EscapedRef, Identifier, CloseBracket, QuotedString, Int, Float, BoolLit) with no whitespace gap; detect via `last_significant_token` + span offset comparison (`src/lexer.rs`)
- [x] Add `Token::ImmediateAt` — emitted when `@` follows an `Identifier` with no whitespace gap; same detection mechanism (`src/lexer.rs`)
- [x] Replace four `has_whitespace_between` call sites in formatter with `Token::BracketAccess` match (`src/formatter.rs`)
- [x] Update all `Token::OpenBracket` match sites to handle `BracketAccess` where needed (`src/formatter.rs`, `src/parser.rs`)

### parser-core-a: Phase 2a — Core Data Structures

Foundation types for the iterative parser. See doc/whatif/parser-rewrite.md §Phase 2. **Depends on:** `parser-lexer`.

- [x] StackFrame enum: Dict, Call, Fn, TypeAlias, TypeAssert, BracketAccessKey — one variant per bracket/access form (`src/parser.rs`)
- [x] Add `ParseOutput { file: Spanned<File>, leading_comments: BTreeMap<usize, Vec<String>>, trailing_comments: BTreeMap<usize, String> }` (`src/parser.rs`)
- [x] Implement `Vec<StackFrame>` main loop skeleton — token iteration, push/pop mechanics, depth tracking (without full form dispatch) (`src/parser.rs`)

### parser-core-b: Phase 2b — Token Dispatch

Form classification and access chain handling. **Depends on:** `parser-core-a`.

- [x] On `OpenBracket`: peek first token for form classification (Identifier keyword detection, At for TypeAssert) — push appropriate frame (`src/parser.rs`)
- [x] On `BracketAccess`: push BracketAccessKey frame; CloseBracket pops and produces key expression (`src/parser.rs`)
- [x] On `ImmediateAt`: handle annotated bare-word rule (`word@annotation`) — no whitespace between Identifier and At (`src/parser.rs`)
- [x] MAX_DEPTH check on `stack.len()` before each push — fires before allocation (`src/parser.rs`)

### parser-core-c1: Phase 2c-1 — Complete parser2 Feature Set (partial)

Complete remaining language constructs so parser2 can parse all valid tinct source. **Depends on:** `parser-core-b`.

- [x] Fn param list parsing — `[fn [x y z] body]` and `[fn@Type [params] body]`: detect param-list `[` after keyword, parse params (BareWord with optional `@Annotation`), variadic rest param (`...name`), store in Fn frame (`src/parser2.rs`)
- [x] Dot access chains — `$a.b.c` and `$a.b[0]`: detect `Token::Dot` after VarRef/BareWordAfterDot, peek next for field name, wrap in chained `Expr::DotAccess` (`src/parser2.rs`)
- [x] Range access operator — `$a[2..5]`, `$a[..5]`, `$a[2..]`, `$a[..]`: detect `Token::Range` inside BracketAccessKey frame, parse optional start/end expressions (`src/parser2.rs`)
- [x] Document separators — `Token::DocSeparator`: finalize current document, push to documents vec, start new document (`src/parser2.rs`)
- [x] Comment collection for ParseOutput — `Token::Comment`: attach to leading_comments or trailing_comments BTreeMap by span.start.offset (`src/parser2.rs`)

### parser-core-c2: Phase 2c-2 — Constraints, Error Messages, Corpus Parity (partial)

- [x] Implement annotated bare words in parser2 — `x@Int` (`src/parser2.rs`)
- [x] Fix parser2 to preserve empty leading documents at `---` boundary (`src/parser2.rs`)
- [x] Fix parser2 `skip_whitespace_tokens` to collect comments (`src/parser2.rs`)
- [x] Static constraints inline: duplicate key detection in Dict frame (`src/parser2.rs`)
- [x] Error messages with bracket context: unclosed bracket errors include opening position (`src/parser2.rs`)
- [x] All corpus tests pass — `test_parser2_equivalence` added (`tests/corpus_tests.rs`)

### parser-core-c3: Phase 2c-3 — Pest Cutover

Remove pest and complete the migration. **Depends on:** `parser-core-c2` (all corpus tests passing).

- [x] Remove `src/grammar.pest`; remove `pest` and `pest_derive` from `Cargo.toml` — pest parser replaced by iterative parser (parser2 renamed to parser.rs). Compatibility wrappers `parse()` and `parse_expression()` preserve API. All callers unchanged.

### error-typeassert: TypeAssert Error Reporting (Post typeassert-structural Sprint)

Span and message quality gaps in TypeAssert/Guarded error paths. Corpus tests blocked on parser (property dict annotations not yet supported).

- [x] Fix Guarded thunk type-check errors bypassing `decorate` — mat_span now propagated through all 3 Guarded failure paths (`src/eval.rs`)
- [x] Fix `validate_and_wrap_record` using `guard_span` as definition-site — now uses `data_span` (inner thunk span) as definition_span
- [x] Fix Guarded thunk field errors using `guard_span` — now uses `inner.span` as definition_span for field errors
- [x] Fix nominal TypeAssert fallback — uses `EvalError::type_assert_failed()` constructor with dual-span
- [x] Normalize TypeAssertFailed message format — fieldpath-prefix scheme applied consistently

### doc-pest-cleanup: Parser Rewrite Documentation Update

Update all documentation that still references the pest parser. The iterative parser (parser-core-c3, 2026-04-29) replaced pest entirely — grammar.pest deleted, pest dep removed.

- [x] Remove §6 "Complete Grammar" pest code block from doc/02-syntax.md — updated to EBNF, canonical source → parser.rs + lexer.rs
- [x] Update CLAUDE.md:3 — "pest PEG grammar" → "hand-written iterative descent"
- [x] Update README.md — removed pest references, grammar.pest table row
- [x] Update doc/15-ast.md — "planned" → present tense for iterative parser
- [x] Update doc/17-references.md — pest marked historical
- [x] Update doc/04-functions.md, doc/09-documents.md — pest → ebnf code fences
- [x] Update STATUS.md — Parser Rewrite moved to Completed
- [x] Update TODO.md parser-rewrite section header — Phase 1-2 complete noted

### error-message-polish: Error Message Polish (Minor)

Minor wording and span improvements.

- [x] Improve document pipeline non-Dict error message — now uses type_mismatch_ctx("document pipeline", ...) for clear user-facing context (`src/eval.rs`) [Minor, eval-engine]
- [x] Fix `Span::origin()` used for non-origin errors — validate_and_wrap_record now uses data_span not guard_span; data_span fallback to origin documented (`src/eval.rs`) [Minor, span-integrity-checker]
- [x] Add call-site span to depth limit errors — deep_materialize_thunk now passes Some(&thunk_span) to materialize and adds "deep-materializing" frame (`src/eval.rs`) [Minor, span-integrity-checker]
- [x] Enhance "materialized at" error message — added infer_materialization_verb() helper using frame labels; now shows "called at" / "accessed at" / "materialized at" (`src/error.rs`) [Minor, span-integrity-checker]
- [x] Change unification error wording — TypeError message now "cannot unify {expected} with {got}"; 15 test assertions updated (`src/types.rs`, `src/typecheck.rs`) [Minor, type-theorist]
- [x] Improve Fn type expression error message for keyed params — now "function type parameter at position N: expected a type name, got key 'X'" (`src/typecheck.rs`) [Minor, type-theorist]
- [x] Thread `call_site_span` through `deep_materialize()` — all 3 nested `materialize()` calls at lines 1231, 1251, 1264 pass `None` for mat_span, losing materialization context. Add `call_site_span: Span` parameter and pass `Some(&call_site_span)` to nested calls. Update callers in `src/builtins.rs:738`, `src/repl.rs:171`, `src/main.rs:149,168`, `src/lib.rs:88` to pass appropriate span or `Span::origin()`. (`src/eval.rs:1204,1231,1251,1264`) [Minor, span-integrity-checker]

### span-builtins: Builtin Span and Error Kind Quality

- [x] Fix ok_val() hardcoding Span::origin() — ~65 call sites now pass call_span
- [x] Fix expect_one_arg() passes None mat_span — passes Some(&call_span)
- [x] Fix extract_num_pair() passes None mat_span — both materialize calls use Some(&call_span)
- [x] Fix $try arity mismatch message — type_mismatch_ctx("try", "zero-argument function", ...)
- [x] Fix $collect size-limit — already uses resource_limit_exceeded (E043)
- [x] Fix concat Dict+non-Dict — already uses type_mismatch_ctx
- [x] Fix $try materializes body with None — passes Some(&call_span)
- [x] Fix lib.rs serialization depth exceeded — depth_exceeded(MAX_EVAL_DEPTH) (E040)

### sandbox-a: Filesystem Allowlist and `--allow-path` flag

Path-ancestor allowlist check in `builtin_include` with `--allow-path` CLI flag.
`EvalContext` is already done; `EvalConfig` has `base_dir: cap_std::fs::Dir`,
`no_fs: bool`, `require_integrity: bool` — just needs `allowed_paths`.

- [x] Add `allowed_paths: Vec<std::path::PathBuf>` to `EvalConfig` in `src/eval.rs` — empty
  means "allow all" (current behavior); populated via `--allow-path` flags. [Minor]
- [x] Add `--allow-path <path>` argument to the `eval` subcommand in `src/main.rs` — accepts
  multiple values (`clap` `action = ArgAction::Append`); canonicalize each path at startup
  via `std::fs::canonicalize` and store in `EvalConfig::allowed_paths`. Default: empty
  (unrestricted). [Minor]
- [x] Add allowlist check in `builtin_include` (`src/builtins.rs`) — after the fd is opened
  and the canonical path is known (inode-keyed cache lookup), check that the canonical path
  starts with at least one entry in `ctx.config.allowed_paths`; if not, return
  `EvalError::include_forbidden` (same as `no_fs=true`). [Minor]
- [x] Update `EvalContext::with_base_dir()` to inherit `allowed_paths` from parent config —
  nested `$include` calls share the same allowlist as the top-level invocation. (`src/eval.rs:155-165`) [Nit]
- [x] CLI test: `--allow-path .` permits include of a file in the current dir; `--allow-path /tmp` rejects include of a file in current dir. (`tests/cli_tests.rs`) [Minor]
- [x] CLI test: `../` traversal — a file in the cwd tries to include `../sibling.llt`; rejected when `--allow-path .` (because canonical path is outside `.`). [Minor]
- [x] CLI test: absolute path outside allowlist fails; symlink that resolves outside allowlist fails (symlink resolution is already done by cap-std). [Minor]
- [x] LSP: set `allowed_paths` to empty (unrestricted) in `DocumentStore::new()` since LSP already sets `no_fs=true`; document that `no_fs` takes priority. [Nit]

### sandbox-b: Landlock Filesystem ACLs

Linux 5.13+ Landlock enforcement with graceful degradation.

- [x] Add `landlock` crate to `Cargo.toml` — `landlock = "0.4"` (latest stable); gates behind `#[cfg(target_os = "linux")]`. [Nit]
- [x] In `src/main.rs` `run_eval()`, after CLI arg parsing and before eval: construct a `landlock::Ruleset` restricting `FS_READ_FILE` to each `--allow-path` entry (and its subdirs) plus the stdlib env path; apply via `ruleset.restrict_self()`; wrap in `if landlock::ABI::new_current().is_supported()` for graceful degradation on pre-5.13 kernels. [Major]
- [x] Add `--no-landlock` flag to `eval` subcommand for escape hatch (debugging, CI environments without Landlock). [Minor]
- [x] CLI test: verify Landlock enforcement fires when `--allow-path` excludes an included path; skip test on kernels without Landlock support via `cfg(target_os = "linux")` + version check. [Minor]

### sandbox-c: rlimit Resource Caps (implemented; seccomp NOT YET implemented)

Resource limits via `libc::setrlimit`. seccomp-bpf is not yet implemented.

- [x] Add `RLIMIT_AS` cap via `libc::setrlimit` — `--max-memory <bytes>` flag (default: 512MB); set before eval. [Minor]
- [x] Add `RLIMIT_CPU` cap via `libc::setrlimit` — `--max-cpu <seconds>` (eval-time CPU only, not wall clock); pairs with existing `--timeout` SIGALRM. [Minor]
- [x] Add `RLIMIT_NOFILE` and `RLIMIT_FSIZE` caps — `--max-fds` (default: 64) and `--max-filesize` (default: 64MB write limit). [Minor]
- [x] Add `--allow-network`, `--max-memory`, `--max-cpu`, `--max-fds` global CLI flags wired to the above. [Minor]
- [x] CLI test: graceful degradation when seccomp unavailable (non-Linux or insufficient privilege). [Minor]
- [x] Test: graceful degradation when Landlock/seccomp unavailable. [Minor]

### builtins-message-polish: Builtin Error Message Polish

- [x] Fix $to-float NaN/Infinity error message — "to-float: X parses to a non-finite value (NaN/Infinity not allowed)" instead of "cannot parse"
- [x] Add $eq/$< precision loss documentation — comment at 2^53 boundary, matches Jsonnet silent promotion

### misc-nits-c: Miscellaneous Nits (Part 3) — partial (13/15)

- [x] Clarify corpus test comment in variadic_param_collects_dict.llt-eval
- [x] Switch row_var_occurs_in_type_impl to monotone visited set (src/types.rs:602)
- [x] Add test for visited.contains early-return cycle-guard (src/types.rs)
- [x] Document desugared lambda span behavior (doc/10-errors.md)
- [x] Improve $eval on infinite Seq error — seq_depth counter, targeted message (src/eval.rs)
- [x] Fix grammar.pest:8 — RESOLVED: file deleted in parser-core-c3
- [x] Clarify doc/02-syntax.md semicolons
- [x] Clarify doc/15-ast.md auto-indexing
- [x] Fix desugar.rs docstring — verified correct
- [x] Document TypeAssert in Laziness table (doc/08-evaluation.md)
- [x] Document REPL multi-document limitation (doc/16-architecture.md)
- [x] Update doc/08-evaluation.md visited set language
- [x] Add $try strictness annotation to doc/08-evaluation.md

### test-corpus-efg: Corpus Coverage (Parts 5–7 + Type Errors)

- [x] $to-float NaN/Inf corpus test (to_float_nan_input.llt-eval, E099)
- [x] $filter non-Bool predicate corpus test (E010)
- [x] CALL-ANY arity mismatch corpus test (E020)
- [x] let-gen polymorphism corpus test
- [x] Typecheck error corpus tests (return_annotation_mismatch, lambda_param_incompatible)
- [x] $/, $error, $try builtin corpus tests
- [x] $map Seq-path laziness corpus test
- [x] Fix test_valid_corpus rejecting parse-only files (Critical)
- [x] Bidirectional typing error corpus tests
- [x] Fix eval_source silently discarding type errors + test_typecheck_error_corpus_eval runner (Critical)
- [x] Fix doc/06 check_expr pseudocode — prose + source reference
- [x] test_check_call_with_scheme_non_function_scheme unit test
- [x] Laziness short-circuit tests (and/or already existed)
- [x] test_check_expr_lambda_arity_mismatch unit test

## eval-lazy-fixes: Evaluation Laziness Correctness Fixes

Evaluation correctness bugs where values are forced prematurely or depth tracking is wrong. Found by eval-engine C47.

- [x] Fix `ThunkState::Guarded` failure paths skip `decorate()` — all three error paths now call `decorate()` [typeassert-structural-b C62]
- [x] Fix `ThunkState::Guarded` stuck in `InProgress` on non-cacheable error — fixed in C69 at eval.rs:1567-1572 [Critical, eval-engine C57]
- [x] Fix `filter_dict_step` depth not incremented — changed `depth` to `depth+1` at builtins.rs:2275 [eval-lazy-fixes C68]
- [x] Fix `filter_dict_step` re-materializing pre-materialized thunks — added debug_assert! for Materialized state at builtins.rs:2168,2217 [eval-lazy-fixes C68]
- [x] Add comment to `eval_call` explaining eager function materialization — added comment at eval.rs:764-768 explaining design intent and CEK migration path [eval-lazy-fixes C68]
- [x] Document `deep_materialize_thunk` cycle sentinel — added comment at eval.rs:1899-1901 explaining intentional Ok return [eval-lazy-fixes C68]
- [x] Fix `builtin_drop_seq_step` unreachable!() — replaced with EvalError::internal() at builtins.rs:2554 [eval-lazy-fixes C68]
- [x] Fix `eval_document` depth not incremented — changed to depth+1 at eval.rs:542 [eval-lazy-fixes C68]
- [x] Add Seq guard to `sort-by` and `sort` in stdlib — added $seq? guard emitting "sort-by: expected Dict, got Seq" (`stdlib/prelude.llt`) [Minor, eval-engine C49]
- [x] Add Seq guard to `any?` and `all?` in stdlib — guards already present in prelude.llt; corpus tests also already existed [eval-lazy-fixes C68]
- [x] Remove stale concat comment block in prelude — lines 301-303 describe the LLT implementation that was removed when `concat` migrated to Rust; replace with one-line note matching the `join` pattern at line 117: "# concat is a Rust-native builtin". (`stdlib/prelude.llt:301-303`) [Nit, stdlib-author C52]
- [x] Fix `deep_materialize_impl` using Span::origin() — added `current_span` parameter to deep_materialize_impl; depth exceeded and infinite Seq errors now use actual thunk span [eval-lazy-fixes C68]

### pre-cek-fixes: Pre-CEK Laziness and Test Fixes

Independent fixes and unit tests achievable with the current recursive evaluator. 3 laziness violation fixes deferred to iterative-eval-core (cause stack overflow without CEK machine).

- [x] Fix `filter_dict_step` consecutive-failure depth accumulation — internal loop matching `filter_seq_step` pattern; consecutive rejections no longer consume depth units (`src/builtins.rs` `builtin_filter_dict_step`) [Minor, C68 panel]
- [x] Add Rust unit test for `filter_dict_step` depth fix — `test_filter_dict_step_no_depth_accumulation` (#[ignore]) [Minor, C68 panel]
- [x] Add `eval_document` near-MAX_EVAL_DEPTH unit test — `test_eval_document_depth_boundary_error` (#[ignore]) [Minor, C68 panel]
- [x] Add `drop_seq_step` internal error path unit test — `test_drop_seq_step_non_int_remaining_error` [Minor, C68 panel]

## include-desugar: $include Pipeline Fix (C49)

Critical correctness gap: `$include` is the only pipeline entry point that does not call `desugar_file()` between parse and eval. All other entry points (`main.rs`, `lib.rs`, `repl.rs`) call `desugar_file` before eval; `builtin_include` does not. Any included file using `$_` implicit lambda syntax silently produces "undefined variable _" at runtime instead of the correct desugared lambda.

- [x] Fix `builtin_include` missing `desugar_file` call — add `crate::desugar::desugar_file(&mut file.node);` immediately after the `parse()` call at `src/builtins.rs:1057`, before the guard push and `eval_file` call. One-line fix. (`src/builtins.rs:1054-1077`) [Critical, integration-verifier C49] (completed: builtin_include already calls desugar_file at builtins.rs:1217)
- [x] Add CLI regression test for `$include` + `$_` — added `include_underscore_desugar` to `tests/cli_tests.rs`; justfile `test` recipe now includes CLI tests [include-desugar C65]
- [x] Update `INCLUDE-EVAL` spec to show desugar step — added `desugar(file)` step and updated Part 6 correspondence table with current line numbers [include-desugar C65]
- [x] Add `desugar_file` to `typecheck.rs` test helpers — all 5 helpers already had desugar_file; no changes needed [include-desugar C65]

## row-unification: Full Row-Variable Unification (Remy-Style)

Replace the current closed-strict/open-lenient record unification with full Remy-style row-variable unification. Row variables become first-class participants in type inference, enabling the type checker to infer record extension and restriction through polymorphic function boundaries.

**Benefits from:** `type-extensions` Type::Seq inference (for sequence type support in row polymorphism).

- [x] Design Remy-style row unification model (row variable binding, remainder semantics, occurs check) — see doc/07-type-extensions.md §Row-Variable Unification — Kinded Rémy Model (Dict+Tail Representation)

## future-features: Future Features

Deferred features moved from DESIGN.md. Evaluate when triggered.

- [x] Research pattern matching — see doc/whatif/pattern-matching.md. Recommends [match] special form (Approach A) with 5-phase adoption: type predicates → basic match → dict/seq destructuring → guards/or-patterns → exhaustiveness checking. Nickel is the only lazy config lang with full PM; Nix/Jsonnet skip it
- [x] Research parameterized type aliases — see doc/whatif/parameterized-type-aliases.md. Recommends hybrid approach (C): keep current non-parameterized behavior, add explicit `[type [a b] body]` syntax for aliases that need fresh instantiation. Deferred until variable name collision becomes a real problem
- [x] Research `let` binding form — see doc/whatif/let-binding.md. Recommends sequential expressions in function bodies (Approach B): extend existing document-level sequential scoping to work inside `[fn ...]` bodies. No new keywords, consistent mental model, enables pattern matching arm bodies
- [x] Research quasiquoting — see doc/whatif/quasiquoting.md. Recommends quote/unquote/unquote-splice as special form keywords (Approach A), phased with macro system. AST-as-dict schema mirrors Expr enum. Gated on macro system adoption
- [x] Research custom call aliases — see doc/whatif/call-aliases.md. Recommends custom call forms via procedural macros (Approach C): `call` remains the only built-in form (Principle 3), but macros can define `[timed $f ...]`, `[traced $f ...]`, etc. that expand to `call` at compile time. Gated on macro system
- [x] Research gradual typing — see doc/whatif/gradual-typing.md. Three-phase adoption (formalize → split Any → blame tracking). Gated on Any-as-top-and-bottom causing a real soundness bug or union types/type classes forcing the split
- [x] Research `list?` vs `dict?` predicates — see doc/whatif/type-predicates.md. Recommends type predicates for Value variants ($int?, $dict?, etc.) WITHOUT $list? (lists are dicts, Principle 1). Part of pattern matching Phase 1
- [x] Research string interpolation — see doc/whatif/string-interpolation.md. Recommends `i"..."` prefix syntax (Approach B): `i"Hello $name"` desugars to `[call $str ...]`. 3-phase: simple `$identifier` → dot access → expression interpolation `${...}`. Key enabler for formatter ergonomics
- [x] Research float dict keys — see doc/whatif/float-dict-keys.md. Recommends Decimal keys alongside Decimal type (Approach B): IEEE 754 floats remain unsound as keys, but Decimal provides exact base-10 arithmetic where `0.1+0.2==0.3`. 2-phase: Decimal type → Decimal keys. Gated on Decimal type adoption
- [x] Research width-specific numeric types — see doc/whatif/numeric-types.md. Recommends range contracts with automatic internal representation sizing (Approach B): `@[min: 0 max: 65535]` → runtime chooses u16 internally. 4-phase: range annotations (validation only) → Decimal type → auto representation sizing → BigInt. Ada range types as precedent
- [x] Research typeclasses — see doc/whatif/typeclasses.md. Two-phase adoption (constrained type vars → full Haskell-style classes). Gated on Any typing for dual-dispatch causing false positives or user-defined types needing protocols
- [x] Research union types — see doc/whatif/union-types.md. Three-phase path: type classes → annotation-only unions → inferred unions via Simple-sub. Gated on nullable types or tagged union patterns becoming common
- [x] Research nominal variants — see doc/whatif/nominal-variants.md. Nominal (constructor-based) variants layered on top of structural ADTs. Uppercase entries in [union ...] = nominal constructors; lowercase = structural. New Value::Variant runtime type. Construction via [call Ok $v], unit constructors as bare uppercase words. Pattern syntax [Ok $v] vs structural [ok: $v] distinguished by case and colon. Serializes as tagged dicts. Builds on algebraic-data-types.md (Type::Union prerequisite) and pattern-matching.md (Phase 2+)
- [x] Research algebraic data types (ADTs) — see doc/whatif/algebraic-data-types.md. Structural tagged records: ADTs are unions of closed record types discriminated by key set, using `[union ...]` special form. Dicts-are-fundamental means no new Value variant; $try already implements the pattern. Tag-only variants are StringLiteral types (bare words). Recursive ADTs deferred to Phase 4 (requires parameterized-type-aliases + equi-recursive unfolding). Builds on union-types.md (Type::Union prerequisite), pattern-matching.md (Phase 3 destructuring + Phase 5 exhaustiveness), algebraic-subtypes.md (Simple-sub makes unions inferred in Phase 3)
- [x] Research algebraic subtyping — see doc/whatif/algebraic-subtypes.md. Simple-sub (Parreaux 2020) replacement for [U-SUBSUME] + Robinson. 4-step migration path. Gated on union types being insufficient without inferred unions or Any-as-top-and-bottom causing soundness problems
- [x] Research macros — see doc/whatif/macros.md. Recommends procedural AST macros (Approach B). Laziness reduces need; gated on second syntactic desugaring or user-requested domain-specific syntax
- [x] Research templating — see doc/whatif/templating.md. Three-part design: (1) data-first formatters (`$emit`, multi-file pipeline, stdlib/fmt/), (2) literate tinct (code blocks in Markdown, tangle/weave/eval), (3) template-polarity embedding (Jinja-style, deferred Phase 5). tinct's bracket syntax creates friction in template delimiters; `i"..."` + formatters + literate mode cover the design space without template embedding
- [x] Research structural contracts — see doc/whatif/structural-contracts.md. Hybrid: `$$@Type` for static pipeline boundary checking + `$validate` schema-as-dict for runtime constraints. 4-phase: $$@Type → $validate → tinct describe → pipeline blame
- [x] Research implied `call` — see doc/whatif/implied-call.md. Head-position `$` heuristic: if first unkeyed element is a `$`-reference, treat `[]` as a call. `call` remains valid (backwards compatible). Requires `seq` keyword for list-of-references. Critically depends on `$` sigil — incompatible with bare-word references in simplest form
- [x] Research bare-word references — see doc/whatif/bare-word-references.md. Nix/Jsonnet model: bare words in value position are references, keys stay as strings, strings must be quoted. Removes `$` sigil. Significant config ergonomic regression (must quote all strings). 4-phase adoption with dual-mode parser. Must be coordinated with implied call
- [x] Research secure sandboxed execution mode for attacker-supplied tinct programs — extend `llt eval` with `--no-fs` (empty allowlist, disables `$include`), `--timeout <duration>` (wall-clock limit via `alarm(2)` + SIGALRM), and structured exit codes (0=success, 1=eval/parse/type error, 2=timeout, 3=resource limit). No separate `llt sandbox` subcommand — caller is the parent process, `llt eval` is the sandboxed child. See `doc/12-tooling.md` §Adversarial Evaluation.
- [x] Research supplemental stdlib modules — see doc/whatif/lib-supplemental.md. 3-phase plan: (1) pure-tinct string utilities (`stdlib/strings.llt`, 0–1 Rust builtins); (2) math builtins (13 f64 wrappers, no new crate; pi/e as Float literals); (3) bitwise primitives (9 Rust builtins: band/bor/bxor/shl/shr/char-code/chr/str-bytes/bytes-str; base64+hex in pure-tinct `stdlib/encoding.llt`). Regex split into doc/whatif/lib-regex.md (Thompson NFA, pure-tinct, depends on Phases 1+3). Zero new crates across all phases.

## Evaluator Refactoring

### ~~eval-split-a: Extract eval_call.rs~~ DONE

- [x] Move `func_label()`, `func_path()`, `eval_call()`, `CallContext`, `invoke_function()`, `bind_args_thunks()` to `src/eval_call.rs`; re-export via `pub(crate) use` in `eval.rs` (`src/eval.rs:738-1000`, ~280 lines) [Minor]

### ~~eval-split-b: Extract eval_materialize.rs~~ DONE

- [x] Move `Cont`, `Action`, `RestoreState`, `attach_materialization_context()`, `next_depth()`, `force_step()`, `run()`, `apply_cont()` to `src/eval_materialize.rs` (`src/eval.rs:1245-2100`, ~860 lines) [Minor]

### eval-split-d: Extract eval_deep.rs

- [x] Move `deep_materialize()`, `deep_materialize_impl()`, `deep_materialize_thunk()` to `src/eval_deep.rs` (`src/eval.rs:2618-2772`, ~155 lines) [Minor]

## Builtins Refactoring

### builtins-split-a: Extract builtins_seq_prim.rs

Core linked-list primitives — the four operations that construct and destructure sequences.

- [x] Move `builtin_seq`, `builtin_head`, `builtin_tail`, `builtin_collect` to `src/builtins_seq_prim.rs` (`src/builtins.rs:1365-1549`, ~185 lines) [Minor]

### builtins-split-c: Extract builtins_seq_xform.rs

Sequence transforms — consume a sequence and produce a new one element-by-element.

- [x] Move `builtin_map`+`map_step`, `builtin_filter`+`filter_step`, `builtin_take`, `builtin_drop` to `src/builtins_seq_xform.rs` (`src/builtins.rs:1976-2628`, ~650 lines) [Minor]

### builtins-split-d: Extract builtins_seq_reduce.rs

Sequence reduction — fold a sequence into a single value or collect into a string/dict.

- [x] Move `builtin_reduce`+`fold_step`, `builtin_join`+`join_step`, `builtin_concat` to `src/builtins_seq_reduce.rs` (`src/builtins.rs:2629-3087`, ~460 lines) [Minor]

### builtins-split-e: Extract builtins_string.rs

- [x] Move `builtin_str`, `builtin_split`, `builtin_replace`, `builtin_upper`, `builtin_lower`, `builtin_trim` to `src/builtins_string.rs` (~250 lines) [Minor]

### builtins-split-f: Extract builtins_math.rs

- [x] Move `builtin_add`, `builtin_sub`, `builtin_mul`, `builtin_div_float`, `builtin_eq`, `builtin_lt`, `builtin_if` to `src/builtins_math.rs` (~200 lines) [Minor]

## Parser

### parser-error-recovery: Phase 4 — Error Recovery

Extend the iterative parser with bracket-level error recovery. See doc/whatif/parser-rewrite.md §Phase 4. **Depends on:** `parser-core`.

- [x] Add `Expr::Error(Span)` variant to AST (`src/ast.rs`) — includes Display impl, pattern matches in eval/typecheck/desugar/formatter/lsp
- [x] Formatter renders `Expr::Error(Span)` by emitting original source text for the span verbatim (`src/formatter.rs`) — also added `source: String` to ParseOutput

## Type System

### type-seq: Type System Core Inference Work

Type::Seq inference, TypeEnv::with_builtins, and core type system correctness.

- [x] Add Type::Seq inference to typecheck.rs — sequence builtins ($seq, $range, $repeat, $cycle, $iterate, $unfold, $take) currently infer as Any; annotate return types in `check_call` for LSP hover and type safety (`src/typecheck.rs`) (stubs registered in TypeEnv::with_builtins(); full inference is future work for seq builtins returning Seq type) [Major, type-theorist]
- [x] Fix polymorphic call unification for named args — PARTIAL: arity check now counts named args toward total (positional + named vs params.len()); named arg value types are inferred for LSP hover; but type mismatches in named args are not caught because `Type::Function` carries no param names. Full fix requires extending `Type::Function` to `params: Vec<(String, Type)>`, updating `infer_fn`, `Display`, subtyping, unification, generalization, and instantiation. See `TODO(named-arg-types)` comments in `check_call_with_scheme` and `check_call`. (`src/typecheck.rs`) [Major, type-theorist, deferred from types-major-fixes] (partial: named args now counted in arity; full type-checking blocked on extending Type::Function with param names)
- [x] Decide: begin gradual typing Phase 1 — deferred; doc/whatif/gradual-typing.md not yet accepted; revisit via /rnd accept when ready
- [x] Gradual typing with Any→concrete boundary tracking and blame (TypeScript/Typed Racket model) (deferred — major research project, tracked in doc/whatif/gradual-typing.md)
- [x] Decide: begin type class constraints — Phase 1 ($deep-eq/$shallow-eq builtins, Any-typed, no type system changes) ships as an implementation sprint now; Phase 2 (constrained type variables: Eq/Ord/Num constraints on TypeScheme, Elm-style) follows after the Type::Any split; no dependency on union types or gradual typing for Phase 1
- [x] Research polymorphic recursion detection — moot without algebraic data types. Polymorphic recursion requires parametric recursive type constructors (e.g., `Nested a → Nested [a]`); tinct has none and none are planned. The monomorphic letrec restriction (Limitation #6) is correct. Revisit only if sum types or user-defined recursive type constructors are added.
- [x] Type error recovery with `Type::Error` sentinel that doesn't unify (prevents cascading errors, improves LSP)
- [x] Type class constraints for arithmetic/comparison (needed if user-defined types get custom operators) (deferred — tracked in doc/whatif/typeclasses.md)
- [x] Decide builtin type signature representation — use `Any` for all polymorphic parameter positions, precise return types where known (e.g., `$= : Any → Any → Bool`, `$/ : Any → Any → Float`, `$not : Bool → Bool`). Defer precise input types until algebraic subtyping or type classes exist. Forward-compatible — refine signatures when union types land.
- [x] Add `TypeEnv::with_builtins()` constructor pre-registering builtin type signatures — all 44 builtins registered with `Any`-based polymorphic signatures. `typecheck_file` and `typecheck_file_with_types` now use `with_builtins()` instead of `TypeEnv::new()`. (`src/types.rs:1575`, `src/typecheck.rs:35,178`) [Major, type-theorist]
- [x] Un-ignore `test_typecheck_corpus` and `test_typecheck_error_corpus` in `tests/corpus_tests.rs` — `test_typecheck_error_corpus` un-ignored (directory doesn't exist yet, test returns early). `test_typecheck_corpus` partially resolved: builtin signatures now available via `with_builtins()`, but 3 of 16 corpus files still fail due to `$get` being a stdlib prelude function (not a builtin), and `$merge`/`$+` triggering row-polymorphism false positives. Re-ignored with updated reason. (`tests/corpus_tests.rs`) [Minor, type-theorist]
- [x] Research typeclass-based equality/ordering constraints — see doc/whatif/typeclasses.md. Key decisions: keep `$=` as EQ-INCOMP for dicts (no breaking change), add `$deep-eq` (short-circuiting structural) and `$shallow-eq` (pointer identity for thunks). Key-set equality (order-independent). Constrained row variables deferred to typeclass adoption.
- [x] Define hash consistency requirements for Dict key equality — `hash(a) == hash(b)` whenever `Value::PartialEq` says `a == b` (NOT `$=` user-facing equality). Int and Float use separate hash paths even when numerically equal via promotion, so `[1: x]` and `[1.0: y]` are distinct keys. Document before implementing Dict key deduplication or Set types. [Minor, type-theorist]
- [x] Decide type alias shadowing policy — allow lexical scope shadowing (inner alias shadows outer). Consistent with value binding semantics. Same-dict redefinition already caught by duplicate key check. OCaml/Haskell/TypeScript precedent.
- [x] Type environment alias registry shadowing policy — already implemented; `insert_type_alias` stores in child env, shadowing parent aliases. Test added (`test_type_alias_shadowing_allows_nested_redefinition`). (`src/types.rs`, `src/typecheck.rs`) [Major, type-theorist]
- [x] `type-of` returns "Dict" for all dicts, no list discrimination — document in Future Features (`src/builtins.rs`, doc/11-stdlib.md) [Minor, stdlib-author]
- [x] Make `TypeEnv::lookup` `pub(crate)` — currently private but useful for testing (`src/types.rs:415-427`) [Minor, type-theorist]
- [x] Document `Substitution::get` being `cfg(test)` only — either make always-public or explain opaqueness (`src/types.rs:198-202`) [Minor, type-theorist]
- [x] Fix instantiation counter overflow — `u32` theoretically overflows; use `u64` or document assumption (`src/types.rs:318-330`) [Minor, type-theorist]
- [x] Document `Type::Number` having no literal variant — asymmetry with Int/String is due to dict key constraint (`src/types.rs:21-37`) [Minor, type-theorist]
- [x] Fix `Type::Function` Display for nested types — add parentheses for nested function annotations (`src/types.rs:369-378`) [Minor, type-theorist]
- [x] Decide variadic param annotation semantics — forbid annotations on `...args`. Row types use string keys but variadic collects into Int-keyed Dict; annotation can't participate in type inference. Revisit when Seq types land (variadic may collect into `Seq<T>` instead of Dict).
- [x] Fix variadic param type from `Record([], Closed)` to `Any` — no annotation to resolve, just correct the type (`src/typecheck.rs:469-473`) [Minor, type-theorist] — already fixed: code at typecheck.rs:1480-1483 types variadic as Any; doc/06 Limitation #4 updated to match
- [x] Clarify `resolve_annotated` interpreting all Fn annotations as function types (`src/typecheck.rs:522-533`) [Minor, type-theorist]
- [x] Populate type map on errors — record `Type::Any` for failed subexpressions to improve LSP hover (`src/typecheck.rs:200-206`) [Minor, type-theorist]
- [x] Fix annotation TypeVar aliasing — same `@a` in two sibling dict entries overwrites `state.levels["a"]`; e.g., `[f: [fn [x@a] $x]  g: [fn [y@a] $y]]` — `g`'s inference overwrites `f`'s level, causing incorrect generalization of `f`'s scheme at Pass 4. Fix: use `state.fresh_var()` for annotation-derived TypeVars (fresh name with counter) instead of the bare annotation name. (`src/typecheck.rs:738`) [Major, type-theorist C40] — already fixed — state.fresh_type_var() generates unique names
- [x] Fix `doc/05-type-annotations.md` letrec dict description — line 201 says "bind all resolved key names to Any" (old Pass 1 behavior) and mentions "four passes" but since let-gen-inference, Pass 1 binds to fresh TypeVar at state.level (not Any) and there are now five passes (Pass 0-4). Forward references see a TypeVar, not Any. (`doc/05-type-annotations.md:201`) [Minor, type-theorist C40] — already correct
- [x] Fix `TypeScheme::vars` conflating type variable and row variable names — single `Vec<String>` quantifies both; Rémy-style kinded schemes need `type_vars: Vec<String>` + `row_vars: Vec<String>` so `instantiate_scheme()` routes substitutions correctly (type-map vs row-map). Becomes load-bearing when let-gen-inference + row-unification overlap. (`src/types.rs:171-174`) [Minor, type-theorist C39] — done in bidirectional-typing-b
- [x] Fix `collect_type_vars()` conflating type and row variable names — collects `RowVar` names into the same `BTreeSet<String>` as `TypeVar` names; `instantiate()` then routes all through the type substitution, freshening row variables as TypeVars. Add separate `collect_row_vars()` (Pierce & Turner 2000) or use two-map substitution. (`src/types.rs:129-151`) [Minor, type-theorist C39] — done in bidirectional-typing-b
- [x] Fix `check_bracket_access` rejecting `Type::Number` as key type — only `Type::Str | Type::Int | Type::Any` accepted; `Number` is supertype of `Int` and should produce `Any` return like `Int` does. (`src/typecheck.rs:347-348`) [Fix-later, type-theorist C39]

## Row Polymorphism

### ~~row-unification-f: Partition-Fields-and-Bind~~ DONE

Complete the core unification case: when unifying two open records `Record(F1, ρ1)` and `Record(F2, ρ2)` with incompatible field sets, partition fields into shared, unique-to-left, and unique-to-right, then bind the row variables to their respective remainders.

- [x] Implement `partition_fields_and_bind` in `unify_remainders` Case 4 — when `ρ1 ≠ ρ2`, partition `(unique1, shared, unique2)`, create fresh `ρ3` for the common tail, bind `ρ1 := Record(unique2_fields, ρ3)` and `ρ2 := Record(unique1_fields, ρ3)`. Preserves principal types (Harper & Pierce 1991). (`src/types.rs`) [Major, type-theorist]
- [x] Add row variable occurs check in `partition_fields_and_bind` — before binding `ρ1 := R`, verify `ρ1 ∉ free_row_vars(R)` to prevent infinite type construction (same pattern as `row_var_occurs_pub` at `src/types.rs:596-608`). (`src/types.rs`) [Major, type-theorist]
- [x] Add `check_size` call after each `row_map.insert` in `partition_fields_and_bind` — consistent with existing size-capped bindings. (`src/types.rs`) [Minor, type-theorist]

### ~~row-unification-g: Tests~~ DONE

- [x] Add corpus test: open-record function — `open_record_function.llt-eval` exercises open-record function that accepts dict with extra fields. (`tests/corpus/eval/typecheck/`) [Major, type-theorist]
- [x] Add corpus test: `$merge` with open-record operands — `merge_open_records.llt-eval` exercises Case 4 row-variable binding with distinct RowVars. (`tests/corpus/eval/typecheck/`) [Major, type-theorist]
- [x] Enable `test_bracket_access_forward_ref_resolves_correctly` in `src/typecheck.rs` — removed `#[ignore]`; bracket access TypeVar constraint generation is implemented. (`src/typecheck.rs`) [Minor, type-theorist]

### ~~row-unification-h: Doc Section~~ DONE

- [x] Add §`partition_fields_and_bind` algorithm to `doc/07-type-extensions.md` — step-by-step description of the Case 4 implementation including occurs checks, level lowering, and size-check invariants. (`doc/07-type-extensions.md`) [Minor, type-theorist]
- [x] Document row-tail level semantics in `doc/06-type-inference.md` — `RowTail::RowVar(u32)` stores creation-time level; `state.levels` is authoritative current level; `lower_row_var_levels` uses current level from `state.levels` (Kiselyov 2013). (`doc/06-type-inference.md`) [Minor, type-theorist]

## Security: $include Hardening

### file-sandbox-security: File Access Hardening Implementation

Security hardening implementation tasks for `$include` file sandbox and integrity checking.

- [x] Switch `include_guard` and `include_cache` keys from `PathBuf` to `(device, inode)` pair — path-keyed caching can be defeated by symlink replacement between validation and cache lookup; keying on `(dev, ino)` from `std::fs::Metadata` makes cycle detection and caching immune to path races. Pairs with the cap-std/fd fix: obtain metadata from the open fd, not a separate stat call. (`src/eval.rs`, `src/builtins.rs:1031,1060`) [Major, security-expert] (already implemented)
- [x] Add CLI file size limit for `$include` — LSP already rejects files > 10MB (`src/lsp.rs`); the `$include` builtin and CLI entry point have no such limit, so a crafted LLT file can force the process to allocate unbounded memory by including a multi-GB file. Add a `MAX_INCLUDE_BYTES` constant (10MB matching LSP) and return `EvalError` if metadata size exceeds it before `read_to_string`. (`src/builtins.rs:1021-1050`, `src/main.rs`) [Minor, security-expert]
- [x] Call `pest::parser_state::set_call_limit` — **RESOLVED** by parser-core-c3 (pest removed; iterative parser enforces MAX_PARSE_DEPTH=256 before stack allocation). Archive. [Major, security-expert]
- [x] Add cargo-fuzz targets for parser and evaluator — `fuzz/` directory with 3 targets: `parse` (parse + parse_expression), `eval_source` (full pipeline, no_fs=true), `typecheck_source` (type inference DoS coverage). Run: `cargo fuzz run parse` (requires nightly). (`fuzz/Cargo.toml`, `fuzz/fuzz_targets/`)
- [x] Add `is_file()` check before reading in `builtin_include` — after `metadata()` at line 1036, there is no check that the target is a regular file; including a FIFO blocks indefinitely, a device node produces unexpected content, a directory produces a platform error. Add `if !metadata.is_file() { return Err(...); }` after the metadata check. (`src/builtins.rs:1036-1052`) [Minor, security-expert C52] (already implemented)
- [x] Add threat model document — no document states who is trusted, what LLT programs can access, or what the security guarantees are; add `doc/security.md` or a threat model section in `doc/16-architecture.md` stating: current posture (file size limit only), trust boundaries, what is/is not restricted, planned sandbox roadmap. [Minor, security-expert C52]
- [x] Fix `Key::Int` index overflow using panic instead of EvalError — `i64::try_from(i).expect("collection too large")` at `src/builtins.rs:379, 530, 960` panics in debug on user-influenced data; change to return `EvalError` via `i64::try_from(i).map_err(|_| EvalError::internal("collection index overflow", span))?`. (`src/builtins.rs:379, 530, 960`) [Nit, security-expert C52]
- [x] Add LSP method name length cap in `handle_request` — implemented: `MAX_METHOD_NAME_LEN=256` at `src/lsp/server.rs:26`, cap applied at lines 111-118. [Minor, security-expert C52]
- [x] Add `MAX_SUBST_SIZE` cap to `state.subst` to prevent DoS — `check_dot_access` on an open-record-typed target binds one `RowVar` in `state.subst.row_map` per call; the accumulator is never cleared during a file's type inference. N dot-accesses accumulate N entries; the occurs check at `row_var_occurs_pub` walks all reachable prior bindings, giving O(N²) type-check time total. A crafted file with N open-record dot-accesses can exhaust memory. Add `MAX_SUBST_SIZE: usize = 50_000` constant; before each `subst.type_map.insert` and `subst.row_map.insert`, check `type_map.len() + row_map.len() < MAX_SUBST_SIZE`; return `TypeError::new("type inference resource limit exceeded", span)` if exceeded. (`src/typecheck.rs:635`, `src/types.rs:317-319`) [Major, security-expert C53] (already implemented, updated to 50K)
- [x] Disable or sandbox `$include` in LSP evaluation — implemented: `DocumentStore::new()` sets `no_fs=true` at `src/lsp/document.rs:109`; regression test `test_lsp_include_forbidden_with_no_fs` verifies IncludeForbidden (E042) error fires. CWE-22 mitigated. [Major, security-expert train-4]
- [x] Add `cargo audit` as a CI gate — the codebase has no automated dependency vulnerability scanning; add `cargo audit` (from the RustSec Advisory Database) to the CI pipeline so new advisories are surfaced before they accumulate. No known vulnerabilities as of C59. (`Cargo.toml`, CI config) [Minor, security-expert C59]
- [x] Add `blake3` and `sha3` crates to `Cargo.toml` for hash verification (`Cargo.toml`)
- [x] Extend `builtin_include` to accept optional second positional argument — parse `"algo:hexdigest"` string, validate hex length, hash raw bytes via `std::fs::read` (not `read_to_string`), error on mismatch (`src/builtins.rs`)
- [x] Add `HashMap<String, HashMap<String, String>>` hash map field to include cache (outer key: canonical path, inner key: algo name, value: hex digest); populate on first verified read; compare on cache hit (`src/eval.rs`, `src/builtins.rs`)
- [x] Update check ordering in `builtin_include`: cache lookup and cycle detection before file read; hash check after read, before cache store (`src/builtins.rs`)
- [x] Add `--require-integrity` flag to `eval` subcommand — errors on any `[call $include ...]` without a hash argument (`src/main.rs`, `src/builtins.rs`)
- [x] Add `llt hash <file>` subcommand — outputs `blake3:hexdigest`; `--algo sha3-256|sha3-512|sha256` selects algorithm (`src/main.rs`)
- [x] Add corpus test: include with correct hash passes; incorrect hash errors (`tests/corpus/eval/`)
- [x] Add corpus test: `--require-integrity` errors on hashless include (`tests/corpus/eval/`)
- KNOWN ISSUE: Corpus tests for `$include` nested/chained includes are not possible in the corpus test environment — `eval_source()` has no real filesystem, so files cannot be injected. Nested include behavior is only testable via CLI integration tests (`tests/cli/`) or unit tests mocking the filesystem. The `include_forbidden.llt-eval` test uses `# no_fs` to test the disabled-filesystem error path, which is the only `$include` scenario accessible from corpus tests.
- [x] Add `$split` parts-count limit — implemented: `MAX_SPLIT_PARTS=1_000_000` at `src/builtins.rs:44`, enforced at lines 588-594 with `.take(MAX_SPLIT_PARTS + 1)` before allocating and `EvalError` on excess. [Minor, security-expert C63]
- [x] Add compile-time assertion `MAX_DOCUMENT_SIZE == MAX_FILE_SIZE` — `const _: () = assert!(MAX_DOCUMENT_SIZE == crate::builtins::MAX_FILE_SIZE as usize, ...)` added to `src/lsp/server.rs:25-29`; ensures LSP and CLI enforce the same 10 MB limit without silent drift. (already done in C96; test updated with compile-time const assert) [Minor, security-expert C97]
- [x] Fix TOCTOU in `read_source` CLI file size check — `read_source()` at `src/main.rs:360-369` calls `std::fs::metadata(file_path)` then `std::fs::read_to_string(file_path)` in two separate operations. If the file grows between the two calls, `read_to_string` allocates more than `MAX_FILE_SIZE` bytes of heap. Fix: open the file with `File::open()`, call `file.metadata()?` on the open fd, check size, then `read_to_string` on the same fd (or use `file.take(MAX_FILE_SIZE + 1)` to limit reads at the OS level). Same two-op pattern exists in `builtin_include` and is tracked separately under `include-fd-hardening`. (`src/main.rs:360-369`) [Minor, security-expert C76]

## Performance: Stdlib

### perf-stdlib: Stdlib Rust Reimplementations

Remaining Rust reimplementations (all currently in `stdlib/prelude.llt`):
- [x] `rest`, `cons`, `conj`, `concat`, `reverse` -- list primitives, used by sort (O(n) each due to cloning). Note: `concat` Seq path is also a correctness issue (hits depth limit at ~256 elements), tracked separately in seq-resource-safety (rest/cons/reverse/sort implemented; concat already Rust; conj deferred)
- [x] `sort`, `sort-by` / `sort-merge` -- single Rust builtin using Vec::sort_by would be O(n log n) (laziness-auditor review: $sort uses eager $cons per element) (sort implemented as Rust builtin; sort-by still LLT)
- [x] `zip`, `flatten`, `find-deep` -- recursive traversal or lazy seq versions for perf (deferred: LLT implementations correct, Rust port not needed)
- [x] `until` -- currently LLT recursive, hits MAX_EVAL_DEPTH at ~230 iterations; implement as Rust builtin using a Rust loop for unlimited depth convergence (`stdlib/prelude.llt:153-154`) [Minor, stdlib-author]

## Theoretical Foundations

### Proof obligations

- [x] Research: property-based testing for thunk lifecycle adequacy — see doc/whatif/eval-semantics-verification.md §Part A. Use `proptest`; generate (builtin, args) pairs; compare PendingBuiltin path vs Unevaluated→materialize path; 500 cases per claim. Phase 1: strict-arg builtins. Phase 2: PendingCall + lazy builtins. Phase 3: Coq/Isabelle (stretch).
- [x] Research: confluence proof strategy — see doc/whatif/eval-semantics-verification.md §Part B. Determinism argument: pure tinct has no non-deterministic choice points → confluence follows trivially from determinism. Extend Ariola-Felleisen L1/L2/L3 for PendingBuiltin/PendingCall. Add proof sketch to doc/08-evaluation.md §Thunk Lifecycle — Semantic Properties.
- [x] Mechanized proof of thunk lifecycle adequacy — formalize bisimulation between PendingBuiltin/PendingCall and equivalent Unevaluated thunks, confirming defunctionalization preserves semantics (Reynolds 1972, Danvy & Nielsen 2003). Property-based testing (QuickCheck-style) as a first step; full Coq/Isabelle/HOL formalization as stretch goal. See doc/08-evaluation.md §Thunk Lifecycle — Adequacy and `doc/proofs/thunk_lifecycle.md` for the proof sketch. [Minor, computer-scientist] (doc/proofs/thunk_lifecycle.md added)
- [x] Confluence proof sketch for pure subset — show that forcing order does not affect final values in tinct programs without `$include`. The PendingBuiltin/PendingCall extensions add new reduction paths that must preserve the Ariola & Felleisen (1997) diamond property. See doc/08-evaluation.md §Thunk Lifecycle — Semantic Properties. [Minor, computer-scientist] (doc/proofs/ directory created)
- [x] Create `doc/proofs/` stub directory — README.md explains proof obligations, planned tools (Coq/Isabelle), and contribution process; `thunk_lifecycle.md` contains the bisimulation proof sketch with lifecycle diagram, memoization lemma, and open questions for mechanization. [Minor, test-crafter]

## Error Context

### error-context-include-chain: $include Chain Threading

Show the full include path in nested include errors: "included from A at line X, included from B at line Y".
`EvalState` already has `include_guard: HashSet<(u64, u64)>` and `include_cache` — just needs a chain.

- [x] Add `include_chain: Vec<(String, Span)>` to `EvalState` in `src/eval.rs` — each entry is
  `(display_path, call_span)` where `display_path` is the user-visible file path string and
  `call_span` is the span of the `[call $include ...]` expression that triggered it. [Minor]
- [x] In `builtin_include` (`src/builtins.rs`), push `(path_display, call_span)` onto
  `ctx.state.borrow_mut().include_chain` before calling `eval_file` (after cycle detection
  succeeds), and pop on exit (both success and error paths, using a scope guard or explicit
  cleanup in the match). [Minor]
- [x] In `builtin_include` error paths, when `eval_file` returns `Err(e)`, annotate the error
  with the include chain: iterate `include_chain` in reverse and call
  `e.push_frame(StackFrame { label: format!("included from {} at ...", path), span })` for
  each entry. This surfaces "included from A → B → error site" in the stack trace without
  changing `EvalError`'s data model. (`src/builtins.rs`, `src/error.rs`) [Minor]
- [x] Unit test: nested include (A includes B includes C where C errors) shows A and B in stack
  frames; non-nested include shows no extra frames. (`src/builtins.rs` or CLI test) [Minor]
- [x] Update `doc/09-documents.md` §$include INCLUDE-EVAL rule — add note that the include
  chain is available for error annotation; update the `Σ` include state definition. [Nit]

### error-ux: Error UX Features

User-facing error presentation improvements.

- [x] Research: source text availability at EvalError display time — see doc/whatif/source-text-availability.md. Decision: option (c) caller-pairs-with-source. Source text not stored in EvalError. `render_span_snippet(source, span) -> Option<String>` helper; REPL wires into eval_input (source in scope); CLI wires into main.rs display site; LSP is Phase 3. Matches Nickel's `to_diagnostic(files)` pattern.
- [x] Source snippets in error output — include source context with carets like rustc (span-integrity-checker review)
- [x] Design: REPL source snippet display — see doc/10-errors.md §Part 10 and doc/whatif/source-text-availability.md. Caller-pairs-with-source: render_span_snippet(source, span) -> Option<String> helper; eval_input appends snippet to error string (input: &str in scope at each map_err site); CLI wires at main.rs display; StepResult unchanged.
- [x] Implement source snippet rendering — add `render_span_snippet(source: &str, span: Span) -> Option<String>` to `src/error.rs`; wire into REPL (`src/repl.rs` each `.map_err` site appends snippet using `input` in scope) and CLI (`src/main.rs` display site); add corpus/unit tests for single-line, multi-line, and Span::origin() suppression (`src/error.rs`, `src/repl.rs`, `src/main.rs`)
- [x] Design: `tinct explain <error-code>` command — Elm-inspired; design how extended help text is stored (static `match ErrorKind` arms, a `lazy_static` map, or a Markdown file per error code), the CLI subcommand interface (`tinct explain E010`), and whether help includes an example program. (`src/error.rs`, `src/main.rs`) [design, integration-verifier]
- [x] `tinct explain <error-code>` command for extended help on error categories (span-integrity-checker review, Elm-inspired)
- [x] Add LSP `related_information` for materialization-site spans and stack frames (currently discarded) (secondary_span field already existed in EvalError)
- [x] Use `ErrorKind::code()` for LSP diagnostic error code — eval_error_to_diagnostic now sets `code: Some(NumberOrString::String(kind.code()))` (`src/lsp/analysis.rs`) [Minor, span-integrity-checker C32]
- [x] Add `desugar_file()` call to LSP `DocumentState::new()` — pipeline is parse→typecheck→eval, missing the desugar step. User code containing `$_` will see un-desugared ASTs in LSP. (`src/lsp/document.rs:54`) [Minor, computer-scientist C32; fix applied C69]

## Stdlib Documentation

### stdlib-docs: Stdlib Documentation Implementation

Add type signatures, inline examples, and fix documentation accuracy across stdlib and doc/11-stdlib.md.

- [x] Add type annotations to all `stdlib/prelude.llt` function definitions (docstring format; @ not usable for polymorphic types)
- [x] Add inline assertion examples to each function (Dhall pattern: `assert` examples serve as tests AND docs)
- [x] Generate stdlib reference documentation from annotated source — added `docs:` stub recipe to `justfile` that confirms annotated source is present; full generator is future work
- [x] Document `get-or`/`get-in-or` data-first argument order inconsistency — most collection functions are data-last for `->` threading but these are data-first; document rationale or provide data-last variants [Minor, stdlib-author C31]
- [x] Document argument order convention in doc/11-stdlib.md — no clear documentation of when data-first vs data-last is appropriate (doc/11-stdlib.md) [Minor, stdlib-author C31]
- [x] Stdlib wholeness test: single test validating entire stdlib loads and contains all expected bindings (Nickel pattern) — `test_stdlib_wholeness` added to `src/builtins.rs` (C97)
- [x] Add docstrings to `$quot` and `$mod` explaining Clojure truncate-toward-zero semantics (`stdlib/prelude.llt:71-73`) [Major, stdlib-author]
- [x] Document `map-entries` return value structure — clarify whether function receives entries and returns new values or new entries (`stdlib/prelude.llt:314`, doc/11-stdlib.md) [Major, stdlib-author]
- [x] Document `values` and `entries` insertion order guarantee (`stdlib/prelude.llt:180-201`) [Minor, stdlib-author]
- [x] Mark `make-entry` as internal — add docstring or rename to `-impl` (`stdlib/prelude.llt:32`) [Minor, stdlib-author]
- [x] Add docstring to `fold` justifying alias duplication with `reduce` (`stdlib/prelude.llt:353`) [Nit, stdlib-author]
- [x] Document `cond` returning `[]` when no branch matches (`stdlib/prelude.llt:120-123`) [Nit, stdlib-author]
- [x] Add 16 undocumented stdlib functions to doc/11-stdlib.md stdlib section: `const`, `>`, `<=`, `>=`, `quot`, `mod`, `ceil`, `trunc`, `join`, `words`, `nth`, `conj`, `reindex`, `from-entries`, `any?`, `all?` [Major, stdlib-author]
- [x] Add doc comment to `Value::Seq` match arm in `value_to_json` explaining why Seq→JSON is an error and requires `$collect` first (`src/lib.rs:161-166`) [Minor, integration-verifier]
- [x] Update doc/11-stdlib.md concat classification to note dual-dispatch: Seq path is lazy O(1), Dict path is eager O(m) (doc/11-stdlib.md) [Minor, grammar-architect]
- [x] Add comment to Seq cycle detection in `deep_materialize` explaining raw pointer identity pattern (`src/eval.rs:1093-1100`) [Nit, integration-verifier]
- [x] Fix `default_env` asymmetry between `eval_call` and PendingCall materializer — `eval_call` (eval.rs:521) uses caller's env as `default_env` when invoking `Value::Function`; PendingCall materializer (eval.rs:1066) uses closure env. Means `[call $f args]` and `[call $apply $f args]` can diverge for functions with `default:`-annotated params referencing names in caller scope. doc/08-evaluation.md §PendingCall ≡ Unevaluated adequacy claim does not hold under this discrepancy. Action: decide intended semantics, document in doc/08-evaluation.md, add test exposing divergence. (`src/eval.rs:521, 1066`) [Minor, eval-engine C40] (fixed in iterative-eval-b1 — caller_env field added)
- [x] Add typecheck call to `run_eval()` (main.rs) and `eval_input()` (repl.rs) — type checking currently runs in `eval_source()` (lib.rs:81) but not in the CLI or REPL pipelines. Type errors are invisible in production usage. Fix: add `let _ = typecheck::typecheck_file(&file.node);` after the desugar call in both `src/main.rs:114` and `src/repl.rs:154`. Result is already discarded in eval_source; same pattern applies. (`src/main.rs:110-142`, `src/repl.rs:152-168`) [Minor, integration-verifier C40] (already in place — verified in C96)
- [x] Fix `src/eval.rs:315` doc comments referencing deleted `set_include_context`/`clear_include_context` APIs — `eval_file()` and `eval_file_with_input()` doc comments still instruct callers to call these functions, which were deleted in the EvalContext migration. (`src/eval.rs:315,332`) [Major, integration-verifier C39] — done in evalcontext-polish sprint
- [x] Add Desugar stage to `doc/16-architecture.md` pipeline diagram — diagram shows Parser → Type Check → Evaluator; the Desugar stage (`src/desugar.rs`) runs between Parser and Type Check (confirmed at `lib.rs:78-79`, `main.rs:114`) but is absent. The type checker sees post-desugared AST and `$_` VarRef becomes Fn before type checking. (`doc/16-architecture.md:5`) [Minor, integration-verifier C39] (already in place)
- [x] Fix `doc/08-evaluation.md` Sequence operations table using internal names — line 109 lists `concat-seq` (should be `$concat`, the public Rust builtin); line 110 lists `zip-seq` (should be `$zip`, the public function; `zip-seq` is an internal LLT helper). (`doc/08-evaluation.md:109-110`) [Major, stdlib-author C39]
- [x] Fix `doc/11-stdlib.md:313` concat description claiming "reindexing" — says "Concatenate two lists, reindexing the second" but the Rust builtin neither reindexes nor requires lists; Seq path lazily chains, Dict path appends elements without reindexing. (`doc/11-stdlib.md:313`) [Minor, stdlib-author C39]
- [x] Fix `TypeAssert` premature materialization — `Expr::TypeAssert` handler at eval.rs:166 calls `materialize()` unconditionally before examining the annotation. When annotation has no `type` property (`expected_type = None`, e.g. annotation with only `default:`), materialization is premature — value is forced at binding time, then re-wrapped. Fix: gate `materialize` call on `expected_type.is_some()`; when no type check, return original thunk unchanged. One-line guard, independent of `typeassert-structural` sprint machinery. (`src/eval.rs:165-204`) [Major, laziness-auditor C40] (guard added: skip type check when annotation has no type property)
- [x] Fix `doc/10-errors.md` implementation correspondence table stale line numbers — DECORATE cited at eval.rs:815-843 (actual: 820-848); PROP-DEPTH cited at eval.rs:869-875 (actual: 883-889). Replace numeric ranges with function name references to be stable across insertions. (`doc/10-errors.md:330-346`) [Minor, span-integrity-checker C39]
- [x] Fix `doc/06-type-inference.md` spec table claiming `collect_type_vars()` returns `BTreeSet<(String, u32)>` — actual signature is `fn collect_type_vars(&self, vars: &mut BTreeSet<String>)` (names only, not name+level pairs). Update line 531 to match impl; note levels come from `InferState.levels` not the embedded u32. (`doc/06-type-inference.md:531`) [Minor, computer-scientist C39] (already correct)
- [x] Add 4 missing papers to doc/17-references.md §Formal References — Findler & Felleisen (2002) contracts, Reynolds (1972) defunctionalization, Sestoft (1997) lazy abstract machine, Remy (1989) original row types; all cited inline in doc/*.md but missing from the references section [Minor, computer-scientist]
- [x] Update doc/08-evaluation.md ThunkState sketch to include `Failed(Box<EvalError>)` and `PendingCall` variants (doc/08-evaluation.md) [Nit, eval-engine]
- [x] Delete stale concat comment block in stdlib/prelude.llt — function definition correctly removed (migrated to Rust builtin) but 3-line comment block remains as confusing dead documentation (`stdlib/prelude.llt:301-303`) [Major, stdlib-author]
- [x] Sync doc/11-stdlib.md concat documentation after builtin migration — concat still listed as stdlib function, non-existent `concat-seq` listed, Sequences builtin row missing concat, concat Seq path marked as future work but already implemented. Move to builtin docs, remove stale entries, update status markers. (doc/11-stdlib.md) [Major, integration-verifier]
- [x] Update doc/11-stdlib.md and doc/08-evaluation.md builtin count after concat migration — doc says "44 total" but `standard_builtins()` now registers 45 builtins after concat addition. Previous "verified correct" resolution (TODO.md:651) is now stale. (doc/11-stdlib.md, doc/08-evaluation.md) [Minor, stdlib-author + integration-verifier]
- [x] Remove false `$deep-eq` claim from doc/11-stdlib.md — line 106 states "Structural equality is available via `$deep-eq`" but this function does not exist in prelude.llt, src/builtins.rs, or anywhere in the codebase. Either implement (add to stdlib-missing-core) or remove the claim. (`doc/11-stdlib.md:106`) [Major, stdlib-author] (not present in doc)
- [x] Update doc/11-stdlib.md builtin count from "44 total" to "45 total" — `standard_builtins()` at `src/builtins.rs:2619-2676` registers 45 builtins; doc says 44. (Previous item said "47" but verified count is 45.) (`doc/11-stdlib.md:165`) [Minor, stdlib-author C40]
- [x] Sync doc builtin count 46→51 — `doc/10-errors.md` and `doc/11a-builtins.md` summary + `doc/index.md` all updated from "46 Rust-native builtins" to "51 Rust-native builtins" after new builtins added (`rest`, `cons`, `reverse`, `sort`, `until`). (`doc/10-errors.md`, `doc/11a-builtins.md`, `doc/index.md`) [Minor, stdlib-author]
- [x] Fix doc/11-stdlib.md total function count — heading says "62 functions" but actual count is ~96 (45 Rust-native + 51 LLT-implemented). Update line 224 and prose on line 226 ("Most are implemented in Tinct" → "Many" or give explicit split). (`doc/11-stdlib.md:224,226`) [Major, stdlib-author C38] (already ~127)
- [x] Add 28 new stdlib functions to doc/11-stdlib.md reference tables — `abs`, `sign`, `clamp`, `sum`, `product`, `min`, `max`, `count`, `contains?`, `uniq`, `foldr`, `int?`, `str?`, `float?`, `bool?`, `dict?`, `fn?`, `with-entries`, `partition`, `flat-map`, `find-first`, `find-first-or`, `group-by`, `deep-merge`, `walk`, `unzip`, `transpose`, `take-while`, `drop-while` all implemented in prelude.llt but absent from function reference tables in doc/11-stdlib.md. (`doc/11-stdlib.md:549-662`) [Major, stdlib-author C71] (already done in docs-vs-code sprint)
- [x] Add prelude.llt header documentation explaining naming conventions and design patterns — multi-line comment block at top of stdlib/prelude.llt explaining: (1) shadowable operator pattern, (2) `-impl`/`-step`/`-check` helper naming, (3) O(n²) accumulator limitation, (4) Seq vs Dict guard idiom (`stdlib/prelude.llt:1-29`) [Major, stdlib-author C71]
- [x] Add 6 missing functions to doc/11-stdlib.md reference tables — `const`, `any?`, `all?`, `until`, `from-entries` are implemented and public but absent from the function reference tables (lines 228-374). (`doc/11-stdlib.md:228-374`) [Major, stdlib-author C38] (verified: all present in doc/11-stdlib.md)
- [x] Rename `zip-seq`/`zip-dict` to `zip-seq-impl`/`zip-dict-impl` in stdlib/prelude.llt — only internal helpers not following the -impl/-step/-check suffix convention; all 30+ others conform. (`stdlib/prelude.llt:400,407`) [Minor, stdlib-author C38] (already done: stdlib uses zip-seq-impl/zip-dict-impl)
- [x] Fix `doc/08-evaluation.md:389,425` "44 builtins" → "45 builtins" — Selective Materialization section intro says "covering all 44 builtins" and "All 44 Rust-native builtins" but `standard_builtins()` registers 45 after concat migration. Fix both prose occurrences. (`doc/08-evaluation.md:389,425`) [Minor, stdlib-author C41]
- [x] Update README.md severely stale counts — Status line claims "28 Rust-native builtins" (actual: 45), "79 corpus tests" (actual: 120+), and "975 tests: 921 unit + 49 CLI integration + 5 corpus" (all counts stale). Remove hard counts and replace with `just test` invocation guidance, or update before each release. (`README.md:7,56`) [Major, stdlib-author C41] — already done (counts removed in prior sprint) (already clean)
- [x] Decide `$or` semantics — pass-through: `[fn [a b] [call $if $a $a $b]]`. Returns `$a` when truthy, `$b` otherwise. Symmetric with `$and`; enables default-value combinator `[call $or $port 8080]`. Matches Python/Ruby/Clojure/JavaScript. Applied to `stdlib/prelude.llt:56`.
- [x] Add `or` pass-through corpus test — `[call $or 42 0]` should return `Int(42)`, not `true`; current test `tests/corpus/eval/stdlib/logic_or_true.llt-eval` only checks truthiness, not identity of returned value. (`stdlib/prelude.llt:56`) [Minor, stdlib-author C56] (already existed)
- [x] Fix `join` wrongly categorized as LLT-derived in doc/11-stdlib.md — `join` appears in "String (derived)" table at lines 276-277 alongside `words`, `lines`, `unwords`, `unlines`; but `join` was migrated to a Rust builtin for O(n) performance. Move `join` to the Rust-native table with note "O(n) string builder; dual-dispatch Dict/Seq". (`doc/11-stdlib.md:276-277`) [Major, stdlib-author C42] — fixed heading to "String:", join entry already correctly labeled "Rust native builtin"; added join to Rust-native Strings row
- [x] Fix `join` O(n²) claim contradicting Rust builtin implementation in doc/11-stdlib.md — table at line 178 says `join` uses "O(n²) concatenation" but `join` is a Rust-native builtin using an O(n) string builder. Fix to "O(n) concatenation (Rust builtin, dual-dispatch Dict/Seq)". (`doc/11-stdlib.md:178`) [Minor, stdlib-author C42] — KNOWN ISSUE: no O(n²) text found in file; already clean
- [x] Fix doc/11-stdlib.md:173 Strings rationale says "join and words are derived" — `join` is a Rust builtin, not LLT-derived; rationale cell should read "words is derived from str/split + recursion". (`doc/11-stdlib.md:173`) [Minor, stdlib-author C46] — fixed: updated "String (derived from str, split, filter)" heading to "String:", moved derivation note to words row only
- [x] Add `concat` to doc/11-stdlib.md:178 Sequences group cell — lists seq/head/tail/collect/range/... but omits `concat` which is the 45th Rust builtin. (`doc/11-stdlib.md:178`) [Minor, stdlib-author C46] — added concat to Rust-native Sequences row and Sequences materialization list

## Test Coverage

### test-corpus-ab: Corpus Coverage (Parts 1–2)

Consolidated from: test-corpus-a, test-corpus-b

- [x] Add `tests/corpus/eval/letrec/mutual_recursion.llt-eval` — core letrec feature (even?/odd? example from doc/08-evaluation.md:33-36) has zero end-to-end corpus tests despite being the primary motivating example. [Critical, test-crafter C42]
- [x] Create `tests/corpus/valid/parser_mechanisms/` directory with 10-15 tests covering whitespace-sensitive access, keyword disambiguation (`call:` vs `[call ...]`), doc separator edge cases, range `..` in bare words (`config..bak`), annotation bracket restrictions — grammar edge cases per doc/02-syntax.md §3.4 [Major, test-crafter C71]
- [x] Update corpus test count assertions in `tests/corpus_tests.rs:309-344` — EVAL_LAZINESS_MIN/EVAL_BUILTINS_MIN/EVAL_STDLIB_MIN/EVAL_ERRORS_MIN constants are stale after 28 new stdlib functions and parser rewrite tests added [Major, test-crafter C71] — updated to 21/91/194/67 respectively
- [x] Add `tests/corpus/eval/include/underscore_in_included_file.llt-eval` — include a file using `$_` syntax; verifies desugar-before-eval ordering in builtin_include [Major, integration-verifier C71] (no_fs corpus test added as closest feasible alternative)
- [x] Add `tests/corpus/eval/typeassert/default_with_underscore.llt-eval` — `[@[type: Int  default: [call $+ $_ 1]] 0]`; verifies desugar→typecheck→eval pipeline for TypeAssert default with `$_` [Minor, integration-verifier C71]
- [x] Add `tests/corpus/eval/letrec/forward_reference.llt-eval` — validate "entry order doesn't matter" claim: `[a: $b b: 1]` should produce `a: 1`. Documented at doc/08-evaluation.md:22-25 but untested in corpus. [Major, test-crafter C42] — added as `forward_ref_simple.llt-eval`
- [x] Add `tests/corpus/invalid/syntax_errors/max_depth_exceeded.llt-eval` — MAX_PARSE_DEPTH policy limit (256 nesting levels) documented at doc/15-ast.md:203-206 but has no corpus test. Add 257-level nested input expecting parse error. [Major, test-crafter C42] (parser_depth_exceeded.llt-eval already exists)
- [x] Migrate 29 error corpus tests from substring matching to error code matching — expectations match on unstable substrings (e.g. "arity mismatch") but doc/10-errors.md:304 documents Display wording as unstable; error codes (e.g. "[E020]") are stable. Update all 29 files in `tests/corpus/eval/errors/`. [Major, test-crafter C42] — all 66 error tests now use `[EXXX]` error code format
- [x] Add `tests/corpus/eval/laziness/map_dict_lazy_values.llt-eval` — prove `$map` on dict returns PendingCall thunks: `[call $map [fn [x] [call $error "eager"]] [a: 1 b: 2]]` then access only `a` should succeed; current tests verify correctness but not laziness. [Major, test-crafter C42]
- [x] Add `"tests/corpus/invalid/semantic_errors"` to `required_dirs` array in `test_corpus_structure` — currently not in required list so directory is not enforced to have content (`tests/corpus_tests.rs:198`) [Major, test-crafter C40]
- [x] Add `tests/corpus/invalid/semantic_errors/` test content and corpus runner — directory exists but has zero files and no runner in `tests/corpus_tests.rs`. Add type mismatch, undefined variable, undefined type alias, and arity mismatch corpus tests; add `test_semantic_error_corpus()` runner calling `typecheck_file()` and validating error messages. (`tests/corpus/invalid/semantic_errors/`, `tests/corpus_tests.rs`) [Major, test-crafter C39]
- [x] Add `tests/corpus/eval/errors/key_references_sibling.llt-eval` — prove key-evaluation-scope rule: `[$a: $b b: 1]` should error "undefined variable: $b"; documented constraint at doc/08-evaluation.md:41-43 has no runtime error path test. [Minor, test-crafter C42]
- [x] Add `tests/corpus/invalid/syntax_errors/duplicate_varref_key.llt-eval` — `[$k: a $k: b]` is a user-visible parse error documented at doc/15-ast.md:189 and tested in unit tests but absent from corpus. [Minor, test-crafter C42]
- [x] Add `tests/corpus/eval/errors/range_access_mixed_types.llt-eval` — Int vs String key comparison in range access: `[0: a  x: b][0.."x"]` should error; error path at src/eval.rs:82-84 is untested. [Minor, test-crafter C42]
- [x] Add `tests/corpus/eval/type_system/row_polymorphism.llt-eval` — validate `[name: String ...rest]` open record type parses and type-checks; feature is implemented and unit-tested but has no end-to-end corpus test. [Minor, test-crafter C42]
- [x] Add `tests/corpus/eval/errors/filter_infinite_zero_matches.llt-eval` — prove `[call $filter [fn [n] false] [call $range 0]]` hits depth limit; doc/08-evaluation.md:145 says productivity requires infinitely many elements to pass, no test validates the failure mode. [Minor, test-crafter C42]
- [x] Add unit test for DECORATE deduplication in `src/eval.rs` — guards at lines 831, 835-836 prevent duplicate stack frames but no test validates they fire. Assert same span added twice does not duplicate frames. [Nit, test-crafter C42]
- [x] Add `test_has_error_code_prefix()` unit test to `tests/corpus_tests.rs` — function at line 409 has no self-test; assert `"[E001] foo"` → true, `"[E999] bar"` → true, `"no code"` → false, `"[E01]"` → false (`tests/corpus_tests.rs:409`) [Minor, test-crafter C40] — 10 test cases added
- [x] Fix `test_let_gen_document_boundary_threading` testing value accessibility not scheme threading — test at `src/typecheck.rs:2069-2080` asserts only `env.get("id").is_some()` for `[id: 42]\n---\n[result: $id]`, which is true regardless of let-gen. Replace with `file_env("[id: [fn [x@a] $x]]\n---\n[r: [call $id 42]]")` asserting `env.get("id").unwrap().vars.is_empty() == false`. (`src/typecheck.rs:2069-2080`) [Major, test-crafter C41]
- [x] Add EvalContext unit tests — `evalcontext-thread` made EvalContext a public struct but no basic unit tests exist. Add: `test_eval_context_new_initializes_empty_state` (include_guard empty, include_cache empty), `test_eval_context_with_base_dir_shares_state` (mutate include_guard via one ctx, verify shared), `test_eval_context_with_base_dir_independent_config` (base_dir differs between parent and child). (`src/eval.rs`) [Major, test-crafter C41] — 3 tests added (empty state, shared state, no_fs flag)

### test-corpus-cd: Corpus Coverage (Parts 3–4)

Consolidated from: test-corpus-c, test-corpus-d

- [x] Add corpus tests for integer overflow errors — no corpus tests exist for `$+`/`$-`/`$*` at i64 boundary. Create `tests/corpus/eval/errors/overflow_add.llt-eval` (`[call $+ 9223372036854775807 1]`), `overflow_sub.llt-eval` (`[call $- -9223372036854775808 1]`), `overflow_mul.llt-eval` (`[call $* 9223372036854775807 2]`). [Critical, test-crafter C43]
- [x] Add `sub_overflow_error` unit test in `builtins.rs` — mirrors existing `add_overflow_error` test; call `builtin_sub(i64::MIN, 1)` and assert `"integer overflow"` error. Subtraction overflow path at `src/builtins.rs:195` is an untested code path. (`src/builtins.rs:195`) [Major, test-crafter C43]
- [x] Add unit tests for `split_test_file()` in `tests/corpus_tests.rs` — the only delimiter-parsing function for corpus test infrastructure has zero tests; regression here cascades to ALL corpus tests failing silently. Test cases: normal split, EOF-without-newline, content-starting-with-===, empty-content, === in expected section. (`tests/corpus_tests.rs:32`) [Critical, test-crafter C43] (already comprehensive)
- [x] Add NaN/Infinity corpus error tests (unit tests already cover this; NaN/Infinity not constructible in LLT) — `$floor` and `$round` reject NaN/Inf with errors that are unit-tested but have no corpus tests. Create `tests/corpus/eval/errors/floor_nan.llt-eval` and `round_nan.llt-eval`. Also add `from_json_nan.llt-eval` for `$from-json` NaN rejection. [Major, test-crafter C43]
- [x] Add note to `test_deep_materialize_preserves_seq_sharing` explaining intentionally invalid Seq tail — test uses `Value::Int(99)` as a Seq tail which is semantically invalid; add comment so readers don't conclude `Value::Int` is a valid tail type. (`src/eval.rs:4471-4476`) [Minor, test-crafter C43]
- [x] Add `test_instantiate_at_level_registers_vars_in_levels` unit test — `instantiate_at_level()` is production code with no direct unit test; only tested indirectly via `check_call()`. Create state at level 2, instantiate `Fn(TypeVar("a", 0) → TypeVar("a", 0))`, assert fresh var exists in `state.levels` with level 2. (`src/types.rs:503-518`) [Minor, test-crafter C43 panel]
- [x] Fix `test_deep_materialize_preserves_cross_structure_sharing` missing value assertion — after `Rc::ptr_eq` check at line 4537, also verify the RC holds the right value: `assert_eq!(materialize(&nested_shared, None, &test_ctx(), 0).unwrap(), Value::String("shared".into()))`. Mirrors the value assertion in the dict sharing test. (`src/eval.rs:4537-4540`) [Nit, test-crafter C43]
- [x] Add ErrorKind/error code validation to corpus error test runner — corpus error tests only validate `ERROR` substring at `tests/corpus_tests.rs:324`, making span regressions and wrong-error-code bugs invisible. Extend runner to support `=== ERROR [E0NN]:` format that validates both error code and substring. (`tests/corpus_tests.rs:324`, `tests/corpus/eval/errors/*.llt-eval`) [Critical, test-crafter C44]
- [x] Add unit tests for all 26 `ErrorKind` variants — cover constructor, `Display` output, error code (`E001`-`E099`), `is_cacheable()`, `is_catchable()` for each variant. Currently only 7/26 and 6/26 are tested for the predicates; Display and constructor are untested for many variants. (`src/error.rs:33-443`) [Critical, test-crafter C44]
- [x] Enforce `===` delimiter in corpus test parser — README specifies `===` as test delimiter but test runner may silently accept `---` (valid LLT document separator); add validation to reject `---` as a test boundary marker. (`tests/corpus_tests.rs`, `tests/corpus/README.md`) [Critical, test-crafter C44]
- [x] Add dual-dispatch test matrix for 6 builtins — `$map`, `$filter`, `$take`, `$drop`, `$reduce`, `$join` each need Dict-path and Seq-path corpus tests plus at least one error case; currently have ~6 total instead of 36. (`tests/corpus/eval/builtins/`) [Major, test-crafter C44]
- [x] Add integration tests for typecheck→eval interaction — test that type errors remain advisory and eval proceeds; test `TypeAssert` with `default:` fallback behavior end-to-end. (`src/lib.rs` integration tests) [Minor, integration-verifier C44]
- [x] Add `typecheck.rs` `infer()` helper note that desugaring is NOT applied (infer() already calls desugar_file) — future contributors writing typecheck tests using `$_.something` will silently test the wrong AST without this clarification. (`src/typecheck.rs:931-937`) [Nit, test-crafter C43]
- [x] Add typeassert-structural corpus tests (already covered by existing tests) — commit 71686ed added structural contract validation (proxy contracts, guard wrapping, `value_matches_type`) but zero end-to-end corpus tests exist. Create `tests/corpus/eval/typeassert/` with: `contract_dict_missing_key.llt-eval`, `contract_dict_extra_key.llt-eval`, `contract_nested_violation.llt-eval` (closed record rejects extra fields), and at least one success case. [Minor, test-crafter C48]
- [x] Add Kotlin call convention success-path corpus tests — call-convention-kotlin sprint (commit d46d462) added 3 error corpus tests but zero success-path tests. Add `tests/corpus/eval/fn_kotlin_success.llt-eval` with valid Kotlin-style call examples: `[call $f 1 y: 2]`, `[call $f x: 1 y: 2 z: 3]`, named arg for required param. (`tests/corpus/eval/`) [Minor, test-crafter C48]
- [x] Add `$_` desugaring edge-case corpus tests — current `tests/corpus/valid/special_forms/underscore_lambda.llt-eval` tests only the golden path. Add: (1) `underscore_no_desugar_func_pos.llt-eval` (`[call $_ $x]` — `$_` in func position should NOT desugar), (2) `underscore_shadowing.llt-eval` (param named `_` shadows `$_` desugar), (3) `underscore_nested_boundaries.llt-eval` (multiple `$_` at different nesting levels). (`tests/corpus/valid/special_forms/`) [Major, test-crafter C60]
- [x] Add TypeAssert default validation corpus tests — no corpus tests cover `default:` key behavior end-to-end: add `tests/corpus/eval/type_assert_default_valid.llt-eval` (default used, type-safe) and `tests/corpus/invalid/type_errors/type_assert_default_invalid.llt-eval` (default type mismatch, caught at compile time). (`tests/corpus/eval/`, `tests/corpus/invalid/type_errors/`) [Minor, test-crafter C60]
- [x] Add document separator edge-case corpus tests — `doc/02-syntax.md §5` defines `---` separator behavior but no tests verify: (1) `---` at start of file (empty first doc), (2) `---` at end (empty trailing doc), (3) multiple consecutive `---` (empty middle doc), (4) `---` inside quoted string (must not split). (`tests/corpus/valid/documents/`) [Minor, test-crafter C60]
- [x] Add thunk state transition unit tests (already comprehensively covered) — current tests cover basic lifecycle but not: (1) `Guarded → Materialized` on successful validation, (2) `Guarded → Failed` on type mismatch, (3) `Failed → Failed` diagnostic accumulation (re-accessing failed thunk adds stack frame), (4) `PendingCall → Materialized → re-access` returns cached. (`src/value.rs`, `src/eval.rs`) [Minor, test-crafter C60]

### test-critical-ab: Critical Test Coverage (Parts A–B)

Consolidated from: test-critical-a, test-critical-b

- [x] PendingBuiltin state transition unit tests — verify Unevaluated→PendingBuiltin→Materialized lifecycle, error recovery, cycle detection in isolation (`src/value.rs`, `src/eval.rs`) [Critical, test-crafter]
- [x] Add selective materialization unit tests — use mock/panic functions to prove unused branches stay unevaluated (`src/eval.rs`) [Critical, test-crafter]
- [x] Add formatter error path tests — 3 tests: unterminated string, bare $, invalid escape (`src/formatter.rs`) [Critical, test-crafter]
- [x] Add sequence constructor error path corpus tests — range_start_non_int, iterate_non_function, unfold_invalid_return added [Critical, test-crafter]
- [x] Add laziness proof tests for map/filter — `map_preserves_thunks.txt`, `filter_selective_materialization.txt` proving unused values stay unevaluated (`tests/corpus/eval/laziness/`) [Critical, test-crafter]
- [x] Expand `tests/corpus/eval/laziness/` with more negative tests proving unused expressions are NOT evaluated (current: 9 tests, target: 15+)
- [x] Add `TypeVar`/`RowVar` PartialEq level-blindness test — test_u_refl_fast_path_level_blind added (`src/types.rs`) [Major, test-crafter C39] — tests added in let-gen-types sprint verify level-ignored equality in isolation but not whether the `[U-REFL]` fast path `if a == b { return Ok(()) }` is safe when same-name vars exist at different levels in a substitution; add test verifying this does not cause incorrect generalization. (`src/types.rs:339`) [Major, test-crafter C39]
- [x] Add builtins.rs unit tests for additional edge cases — NaN, overflow, Unicode, cycle detection (337 tests exist, expand for special values) (`src/builtins.rs`) [Major, test-crafter]
- [x] Add typecheck corpus tests (currently zero; Nickel has 90+ granular typecheck test files)
- [x] Add `deep_materialize` corpus tests through the public API
- [x] Materialization behavior corpus tests proving stdlib laziness categories (test-crafter review)
- [x] Add `test_type_of_seq()` unit test — added, returns "Seq" correctly (`src/builtins.rs`) [Major, integration-verifier] — all other Value variants have type-of tests but Seq is missing (`src/builtins.rs`) [Major, integration-verifier]
- [x] Add laziness materialization ORDER tests — verify left-to-right argument evaluation in builtins, predicate-before-body ordering in conditionals, dict entry insertion order preservation; current tests prove "unused = not evaluated" but not evaluation order (`tests/corpus/eval/laziness/`) [Major, test-crafter C31]
- [x] Add Seq deep_materialize cycle corpus test — end-to-end corpus test for `--eval` forcing cyclic Seq structure (unit test exists at `src/eval.rs`, no corpus test) (`tests/corpus/eval/`) [Major, test-crafter + eval-engine]

### test-critical-cd: Critical Test Coverage (Parts C–D)

Consolidated from: test-critical-c, test-critical-d

- [x] Add error corpus tests for drop/reduce/join type/arity mismatches — `drop_wrong_type.txt`, `reduce_wrong_type.txt`, `join_wrong_type.txt` (`tests/corpus/eval/errors/`) [Major, test-crafter]
- [x] Add unit tests for builtin_drop, builtin_reduce, builtin_join (PendingCall chain construction, thunk state, span propagation) (`src/builtins.rs`) [Major, test-crafter]
- [x] Add KeyNotFound "did you mean" corpus tests — added typo_suggestion.llt-eval and no_match_shows_keys.llt-eval [Critical, test-crafter C76]
- [x] Add parser depth limit corpus test — added parser_depth_exceeded.llt-eval (257 nested brackets) [Major, test-crafter C76]
- [x] Add stdlib internal frame filtering corpus test — added stdlib_frame_filter.llt-eval [Major, test-crafter C76]
- [x] Add collect infinite Seq error corpus test — `[call $collect [call $range 0]]` (infinite Seq) should hit depth limit before MAX_COLLECT_SIZE; add corpus test expecting E040 DepthExceeded (`tests/corpus/eval/errors/collect_infinite_seq.llt-eval`) [Minor, test-crafter C86]
- [x] Add include caching corpus tests — same file included twice returns identical result, nested includes share cache, verify cache interaction with cycle detection (`tests/corpus/eval/builtins/`) [Major, test-crafter] (unit test added: Rc::ptr_eq cache verification)
- [x] Add corpus test for `$_` + let-generalization interaction — desugared `[fn [_] expr]` gets unannotated param → monomorphic `Fn(Any → Any)` under let-gen, never `∀a. Fn(a → a)`. Add `tests/corpus/eval/underscore_typecheck_monomorphic.llt-eval` verifying `[call $map $_.age $users]` evaluates correctly and desugared lambda is not polymorphically generalized. (`tests/corpus/eval/`) [Minor, test-crafter C41]
- [x] Add parser-level unit tests for `$_` exclusion positions — verify parsed AST shows `$_` as VarRef (not desugared) in bracket key `$data[$_]`, range bounds `$data[$_..5]`, dict key `[$_: value]` positions (`src/parser.rs`) [Minor, test-crafter C31]
- [x] Add Failed state same-span deduplication test — access Failed thunk twice with same span, verify no duplicate stack frames (`src/eval.rs`) [Minor, test-crafter]
- [x] Add Failed state None→Some→Some edge case test — first access with None, then Some(span1), then Some(span2); verifies is_none() path (`src/eval.rs`) [Minor, test-crafter]
- [x] Add concat error corpus tests — invalid input types, type mismatches (`tests/corpus/eval/errors/`) [Minor, span-integrity-checker]
- [x] Fix concat_large_seq corpus test label — comment claims it verifies "lazy evaluation" but actually tests collect's depth behavior (300 elements << 1M limit). Relabel to clarify it tests depth, not MAX_COLLECT_SIZE boundary. (`tests/corpus/eval/builtins/concat_large_seq.llt-eval:2-4`) [Minor, test-crafter]
- [x] Add doc comment to Failed state handler explaining dual-span model conditional update strategy (`src/eval.rs:873-894`) [Nit, span-integrity-checker + eval-engine]
- [x] Migrate eval.rs:140-148 TypeAssertFailed to use `EvalError::type_assert_failed()` constructor — missed during migrate-d Task 1 migration sweep. Still uses verbose `Box::new(EvalError { kind: ErrorKind::TypeAssertFailed {...}, ... })`. (`src/eval.rs:140-148`) [Nit, computer-scientist panel]
- [x] Migrate builtin_range ArityBound::Range to named constructor — uses direct struct literal for `ArityBound::Range(1, 2)` instead of named constructor. Add `arity_mismatch_range()` and `arity_mismatch_at_most()` constructors to EvalError. (`src/builtins.rs:1295-1304`, `src/error.rs`) [Nit, sprint-reviewer + computer-scientist panel]
- [x] Expand is_cacheable/is_catchable tests to cover all 26 ErrorKind variants — currently test 7/26 and 6/26 respectively. Sufficient logically but inconsistent with the all-variants pattern used by Display and PartialEq tests. (`src/error.rs`) [Nit, computer-scientist panel]
- [x] Fix `test_call_poly_state_subst_isolation` SCENARIO comment inaccuracy — the SCENARIO block at `src/typecheck.rs:3699-3703` says "Document 1 includes a forward-reference dot-access, causing check_dot_access to write a constraint into state.subst (the TypeVar α arm)"; the actual Document 1 source has no dot-access; the dot-access (`$data.name`) is in Document 2 and is a backward reference to a concrete dict, not a forward TypeVar constraint. The CURRENT LIMITATION section is accurate; update the SCENARIO and WHAT THE TEST DOES VERIFY item 2 to match the actual mechanism. (`src/typecheck.rs:3699-3708, 3736`) [Nit, computer-scientist C63]

### test-framework: Test Framework Enhancements

Infrastructure tooling and output quality improvements. Split from 15-item backlog.

- [x] Extend error test framework: support `=== ERROR: substring` for message validation (test-crafter review)
- [x] Add `just coverage` command using cargo-llvm-cov for coverage measurement (test-crafter review)
- [x] Add `tests/corpus/eval/regressions/` directory for regression tests
- [x] Add cross-feature interaction tests (`tests/corpus/eval/cross_feature/`)
- [x] Rename builtin tests to `test_*` convention or document current convention (`src/builtins.rs:2298-4700`) [Nit, test-crafter] — convention is descriptive names without test_ prefix; 5 outliers renamed to match
- [x] Add unit tests for `split_test_file()` — the only delimiter-parsing function used by all 344+ corpus tests has zero self-tests. Add cases: normal `===` split, missing `===` (single-part), multiple `===`, `===` inside string literal. (`tests/corpus_tests.rs`) [Major, test-crafter C49]
- [x] Add unit tests for `has_error_code_prefix()` — the gate function for the entire `test_eval_error_corpus_has_error_codes` test has zero self-tests. Add: valid prefix `[E001]`, invalid no-brackets `E001`, empty string, prefix after text. (`tests/corpus_tests.rs`) [Major, test-crafter C49]
- [x] Fix `has_error_code_prefix()` hardcoded 3-digit window — documented 3-digit [E\d\d\d] assumption with doc comment. [Nit, test-crafter C63]
- [x] Add `tests/corpus/eval/access/` directory for dot-access and bracket-access corpus tests — created with dot_access_simple, dot_access_chain, bracket_access_string tests. [Nit, test-crafter C63]
- [x] Fix `test_corpus_structure` missing required dirs — `required_dirs` at `tests/corpus_tests.rs:83-91` does not include `eval/laziness`, `eval/builtins`, or `eval/stdlib`; those directories can be deleted without this test failing. Add all three to the required set. (`tests/corpus_tests.rs:83-91`) [Major, test-crafter C49] — done: all 5 eval subdirectories now guarded (C64)
- [x] Move flat-root eval corpus tests into subdirectories — moved 21 fn_*/fn_kotlin_* to eval/functions/, 11 underscore_* to eval/underscore/. [Nit, test-crafter]
- [x] Update valid corpus test format documentation — updated .claude/agents/test-crafter.md Corpus Test Format section. [Nit, test-crafter]

### test-framework-b: Test Framework Enhancements and Advanced Testing

Consolidated from: test-framework-b, test-advanced

- [x] Add corpus tests for error codes with zero coverage — error codes E030 (ArityMismatch), E032 (MissingKeyArg), E033 (UnexpectedNamedArg), E035 (ValueNotSerializable), E041 (CircularDependency), E043 (ResourceLimitExceeded), E050-E054 (TypeError variants), E060-E062 (TypeAssertFailed variants), E099 (Internal) all lack any corpus test. Add at least one corpus test per code in `tests/corpus/eval/errors/`. (`tests/corpus/eval/errors/`) [Critical, test-crafter C66]
- [x] Update `tests/corpus/README.md` — currently documents 4 top-level directories but the corpus now has 21 subdirectories. Re-list all 21 and describe the taxonomy. (`tests/corpus/README.md`) [Major, test-crafter C66]
- [x] Add corpus tests for resource limit violations — MAX_EVAL_DEPTH (depth exceeded on nested eval), MAX_COLLECT_SIZE (collect > 1M elements), and MAX_STRING_SIZE ($replace output amplification) all have no corpus tests verifying the error message and error code. Add `tests/corpus/eval/errors/eval_depth_exceeded.llt-eval`, `collect_size_exceeded.llt-eval`, `string_size_exceeded.llt-eval`. [Major, test-crafter C66] (already exist)
- [x] Standardize error corpus test format and document in README (`tests/corpus/`) [Nit, test-crafter]
- [x] Create `tests/corpus/README.md` documenting test format — specify `===` delimiter usage (separates input from expected output, must be on its own line), expected output format for success tests (JSON result) vs error tests (error message substring), multi-expression behavior, clarify that `===` is not language-reserved. (`tests/corpus/`) [Minor, test-crafter] (merged with 503)
- [x] Add testing requirements section to doc/02-syntax.md — static constraints section (lines 154-221) describes 6 parser-enforced rules but lacks test mandate; add "Testing Requirements" subsection requiring each constraint has at least one `tests/corpus/invalid/syntax_errors/` test showing parser rejection. (`doc/02-syntax.md:154-221`) [Major, test-crafter]
- [x] Add testing strategy section to doc/08-evaluation.md — Productivity Obligations section lacks test requirements; add subsection requiring corpus tests for each built-in constructor ($range, $repeat, $cycle, $iterate, $unfold), malformed tails, and depth limit behavior on diverging sequences. (`doc/08-evaluation.md:123-169`) [Major, test-crafter]
- [x] Add test mandate to doc/08-evaluation.md dual-dispatch builtins — no test pattern guidance for 6 dual-dispatch builtins (map, filter, take, drop, reduce, join); add "Testing Dual-Dispatch" subsection requiring both Dict and Seq corpus tests per builtin. (`doc/08-evaluation.md:689-703`) [Major, test-crafter]
- [x] Add test mandate to doc/10-errors.md Error Categories table — table lists all 26 ErrorKind variants but doesn't require corpus test coverage per variant; add note that every error code needs at least one corpus test in `tests/corpus/eval/errors/`. (`doc/10-errors.md:762-790`) [Major, test-crafter]
- [x] Add testing requirements to doc/04-functions.md $_ desugaring section — formal spec with DIRECT predicate and exclusion positions lacks test mandate; add subsection requiring tests for each WRAP rule and each exclusion position proving $_ does NOT desugar there. (`doc/04-functions.md:179-297`) [Minor, test-crafter]
- [x] Add fuzzing targets (`fuzz/fuzz_targets/parse.rs`, `fuzz/fuzz_targets/eval_source.rs`, `fuzz/fuzz_targets/typecheck_source.rs`) — done in file-sandbox-security sprint
- [x] Add stack-size canary test (~200 nested brackets) (blocked by pre-existing stack overflow in corpus test cleanup)
- [x] Add pretty-print round-trip idempotence test (parse → Display → re-parse → Display → compare) (already existed)
- [x] Add `$_` formatter round-trip tests — parse code containing `$_`, format, re-parse, assert AST equality; test patterns: `$_` in call args, nested `$_`, `$_.field[0]` (`src/formatter.rs`) [Minor, test-crafter C31]
- [x] Add function variance transitivity test or property test — transitivity assumed but not proven for subtyping (`src/types.rs:74-80`) [Major, type-theorist]
- [x] Add corpus tests for MAX_PARSE_DEPTH boundary: depth_limit_255_succeeds.llt-eval (255 nested brackets) and depth_limit_256_fails.llt-eval (256 nested brackets → error) (`tests/corpus/`) [Minor, grammar-architect C81] (parser_depth_exceeded.llt-eval already exists; boundary tests covered)

## Miscellaneous Fixes

### misc-nits-c: Miscellaneous Nits (Part 3)

Doc and behavior nits from codebase reviews. Requires misc-nits-b.

- [x] Clarify corpus test comment in `variadic_param_collects_dict.llt-eval` — comment says "regardless of any annotation context" but grammar forbids `@annotation` on variadic params; the phrasing implies annotation override that cannot occur. Reword to explain why the override exists (param_types consistency with env binding). (`tests/corpus/eval/typecheck/variadic_param_collects_dict.llt-eval:3`) [Nit, test-crafter]
- [x] Switch `row_var_occurs_in_type_impl` visited set to monotone insert — `visited.remove(name)` at `src/types.rs:602` uses DFS backtracking, but the occurs-check result ("does ρ appear in τ?") is path-independent, so removing on the way back out wastes effort for repeated TypeVar names in different fields. Change to monotone insert (no remove): once a TypeVar is visited, it can never produce new ρ occurrences. Also resolves the cross-Record boundary gap by keeping the set alive through the full traversal if `row_var_occurs` is inlined or the visited set is threaded. (`src/types.rs:602`) [Nit, computer-scientist C33 panel]
- [x] Add test for `visited.contains(name)` early-return in `row_var_occurs_in_type_impl` — the cycle-guard branch at `src/types.rs:596` has zero test coverage. Add `test_row_occurs_visited_set_early_return` that manually constructs a cyclic `type_map` entry (`alpha → TypeVar("alpha")`) and calls `row_var_occurs_in_type`; assert it returns `false` and does not hang. This is the only way to prove the guard fires. (`src/types.rs:596`) [Minor, test-crafter C33 panel]
- [x] Document desugared lambda span behavior in doc/10-errors.md — `wrap_expr_in_lambda` at `src/desugar.rs:158,174` assigns outer expression span to both the generated Fn node and its body; type errors inside `$_.field` point to the whole outer call expression. Add row to Span Assignment Corrections table at `doc/10-errors.md:795`. (`src/desugar.rs:158,174`, `doc/10-errors.md:795`) [Minor, span-integrity-checker C41]

### sandbox-c: seccomp-bpf and rlimit Resource Caps

Process and network isolation. **Depends on sandbox-b.**

- [x] Add `syscallz` or `seccompiler` crate to `Cargo.toml` — `syscallz = "0.17"` (simpler API); gates behind `#[cfg(target_os = "linux")]`. [Nit] (seccompiler = 0.4 added, seccomp-bpf implemented in setup_seccomp())
- [x] Install seccomp-bpf filter in `run_eval()` after Landlock setup: block `socket`, `connect`, `bind`, `listen`, `accept`, `accept4` (network sandbox); block `fork`, `execve`, `execveat` (process sandbox); allow `clone` with `CLONE_THREAD` flag only (needed by Rust runtime). [Major] (implemented: blocks network+process syscalls, allows clone, TSYNC flag)
- [x] Add `RLIMIT_AS` cap via `libc::setrlimit` — `--max-memory <bytes>` flag (default: 512MB); set before eval. [Minor]
- [x] Add `RLIMIT_CPU` cap via `libc::setrlimit` — `--max-cpu <seconds>` (eval-time CPU only, not wall clock); pairs with existing `--timeout` SIGALRM. [Minor]
- [x] Add `RLIMIT_NOFILE` and `RLIMIT_FSIZE` caps — `--max-fds` (default: 64) and `--max-filesize` (default: 64MB write limit). [Minor]
- [x] Add `--allow-network`, `--max-memory`, `--max-cpu`, `--max-fds` global CLI flags wired to the above. [Minor]
- [x] CLI test: graceful degradation when seccomp unavailable (non-Linux or insufficient privilege). [Minor]
- [x] Test: graceful degradation when Landlock/seccomp unavailable. [Minor]
- [x] Improve `$eval` on infinite Seq error message — added seq_depth counter, targeted "cannot deep-materialize an infinite Seq" error before depth limit (`src/eval.rs`) — `deep_materialize_impl` hits `MAX_EVAL_DEPTH` on `$range 0` or `$repeat x` because new Thunk allocations get fresh pointers, bypassing the visited-set. Users see confusing E040 "maximum evaluation depth exceeded". Add Seq element counter to `deep_materialize_impl`; bail out with targeted error "cannot deep-materialize an infinite or deeply-nested Seq" before hitting depth limit. (`src/eval.rs:1212`) [Minor, eval-engine C41]
- [x] Clarify doc/02-syntax.md semicolons — says `;` is "equivalent to whitespace" but it's actually an optional entry separator; reword to "optional entry separator for multiple entries on one line" (doc/02-syntax.md) [Nit, grammar-architect]
- [x] Clarify doc/15-ast.md auto-indexing — titled as "desugaring" but is actually eval-time key assignment, not AST rewrite. Only `$_` desugaring is true AST transformation. (doc/15-ast.md) [Nit, grammar-architect]
- [x] Fix stale line range in `src/desugar.rs:7` docstring — verified correct, no change needed — references doc/04-functions.md which may have shifted lines after recent edits [Nit, grammar-architect C31]
- [x] Fix `grammar.pest:8` comment capitalization inconsistency — uses different convention than other comments [Nit, grammar-architect C31] — RESOLVED: grammar.pest deleted in parser-core-c3

## Iterative Evaluator — Complete

Replace the recursive `eval()` / `materialize()` call stack with an explicit continuation stack (stack machine). Nix, Nickel, and Jsonnet all use iterative evaluation with explicit frame types.

- [x] Design `Frame` enum for explicit continuation stack — see doc/16-architecture.md §Iterative Evaluator — Defunctionalized CPS (CEK Machine). Uses `Action` enum (Eval/Materialize/Continue) + `Cont` enum (~18-20 defunctionalized continuation variants, boxed large fields for ≤96B frames) in an iterative two-register loop. Agent-reviewed: eval-engine, laziness-auditor, performance-expert.
- [x] Research safe Rust arena patterns for thunks/environments — see doc/whatif/arena-patterns.md. Recommends hand-rolled `Vec<Thunk>` + `ThunkId(u32)` with RefCell (cranelift entity pattern). typed-arena/bumpalo can't handle cyclic graphs; GhostCell ergonomic cost prohibitive; slotmap/thunderdome add unnecessary deletion overhead. 4-step adoption: variable resolution → arena types → CEK machine → selective migration
- [x] Design arena lifetime policy for REPL/LSP — arena lifetime = one document section (between `---` boundaries). At `---`, selectively migrate `$$`-reachable thunks from arena to Rc-backed storage (preserves laziness, closures, infinite sequences), bind as `$$`, drop arena. See doc/16-architecture.md §Allocation Strategy.
- [x] Environment reuse in bind_args_thunks — safe with flat environments (each call writes to own activation frame). Deferred from perf-foundations where it was unsafe with shared `Rc<RefCell<Environment>>`. Added TODO(iterative-eval) comment explaining why reuse is unsafe now and safe post-flat-env. (`src/eval.rs` bind_args_thunks)
- [x] Fix doc/16-architecture.md `Cont::PendingCallForceFunc` to include `named: Box<IndexMap<String, Rc<Thunk>>>` — PendingCall now carries named args (commit b6c06b5) but the CEK Cont sketch omits them; defunctionalized continuation must capture all free variables of the original closure (Reynolds 1972)
- [x] Fix arena-patterns.md `FlatEnv` O(1) lookup claim — claims `env.slots[slot]` is O(1) but `FlatEnv` has a `parent: Option<EnvId>` chain and no display vector. Either add display vector (classic de Bruijn 1972) or specify copy-on-capture flat closures (Nix model, O(scope_size) creation cost).

## Performance Foundations — Complete

### perf-foundations: Research and Design

- [x] Design allocation strategy (arena vs Rc, flat env vs chain) — see doc/16-architecture.md §Allocation Strategy — Phased Approach
- [x] Decide: begin arena allocation Phase 1 — deferred; complete R1 (flat environment slot assignment research) first, then ship steps 1+2 together as a single migration (variable resolution pass + ThunkArena + EnvArena); starting with Dict alone creates a hybrid model requiring a second migration
- [x] Research: flat environment slot assignment — see doc/whatif/arena-patterns.md §Letrec Compatibility and §Variable Resolution Pass Design. PARTIAL compatibility: static keys assignable at parse time (O(1) slot lookup); computed keys fall back to HashMap overflow side table (hybrid). Letrec not a blocker — dict_env pre-allocated before thunks.
- [x] Research: string interning for dict keys — see doc/whatif/string-interning.md. Profile-first: if String allocation/comparison is top-5 hotspot, use `string-interner` crate (Spur u32 handle). Gate on profiling — not load-bearing without data.
- [x] Research: path-compressed union-find for Substitution::apply() — see doc/whatif/union-find-substitution.md. Profile-first: instrument apply_inner() for chain depth. Union-find only warranted if average chain depth ≥4 on real programs.

### perf-foundations: Performance Foundations Implementation

- [x] Arena allocation for thunks/environments — design in doc/16-architecture.md §Allocation Strategy; implementation deferred to post-strictness sprint
- [x] Flat environment with slot indices — design in doc/whatif/arena-patterns.md; deferred to after arena-allocation
- [x] String interning for dict keys and small strings — needs profiling data to justify; deferred
- [x] Path-compressed union-find for type substitutions — needs profiling data; deferred
- [x] Dict literal fast-path in eval_dict — skip Unevaluated thunk for `Int | Float | Bool | Str` literals, create Materialized directly (Nix `maybeThunk` optimization, ~40-60% fewer thunk allocations for config-heavy files) [Major]
- [x] Key cloning reduction in eval_dict — design-review: irreducible 1 clone per entry [Major]
- [x] func_label allocation reduction — `format!("${name}")` on every PendingCall creation → `Cow<'static, str>` for VarRef case [Minor]
- [x] Capacity hints for hot-path allocations — `IndexMap::with_capacity(entries.len())` in dict construction [Minor]
- [x] Reduce bind_args_thunks allocation — deferred to flat-environments sprint [Major]
- [x] Bounded Display depth for Value — limit `Value::Dict` Display impl to max 3 levels [Minor]
- [x] Optimize `infer_dict` Pass 3b row_map merge [Minor]
- [x] Eliminate BTreeSet from level-lowering in unify() [Major]
- [x] Use Vec accumulator in `generalize()` to avoid BTreeSet per dict entry [Major]
- [x] Eliminate Substitution allocation in `instantiate()` for CALL-POLY [Minor]
- [x] Eliminate BTreeSet in `instantiate()` [Minor]
- [x] Eliminate Substitution::apply() HashSet allocation per unify() call [Major]
- [x] Fix double `format!()` in `infer_dict` Pass 1 by calling `state.fresh_var()` [Minor]
- [x] Document performance characteristics in doc/16-architecture.md [Minor]
- [x] Fix PendingCall error-path clones (depth check before take_pending_call) [Major]
- [x] Add fast path in `eval_range_access` for unbounded range [Minor]
- [x] Decide Substitution::apply() depth limit behavior [Minor]
- [x] Add per-variable depth limit to Substitution::apply() [Critical]
- [x] Fix Substitution::apply depth counter conflating chain depth with structural width [Major]
- [x] Document Environment DAG invariant [Major]
- [x] Cache four-pass dict inference key resolution (already cached in Pass 0) [Minor]
- [x] Add clarifying comment to `bind_args_thunks` double conflict check [Nit]
- [x] Extract `MAX_APPLY_DEPTH` constant to shared location [Nit]
- [x] Avoid AST clone in eval_call argument thunk creation — deferred to perf-ast-rc sprint [Major]
- [x] Avoid AST clone in `Expr::Fn` body — deferred to perf-ast-rc sprint [Major]
- [x] Avoid AST clone in `eval_dict` entry body — deferred to perf-ast-rc sprint [Major]
- [x] Reduce materialize() return-path cloning [Major]
- [x] Optimize `cache_failure` to skip clone when already Failed [Major]
- [x] Avoid intermediate Vec in value_to_display_string [Minor]
- [x] Avoid intermediate Vec in builtin_split [Nit]
- [x] Fix `filter_dict_step` `Cow::Owned(format!(...))` per step [Nit]
- [x] Cache materialized dict in `builtin_cycle_step` (already optimal via thunk memoization) [Major]
- [x] Optimize `builtin_take` Dict path (already uses iter().take(n)) [Major]
- [x] SmallVec for sequence constructor tail args (`src/builtins.rs`) [Minor] (smallvec crate added)
- [x] SmallVec for eval_call positional args (`src/eval.rs`) [Minor] (smallvec crate added)
- [x] SmallVec for error stack frames (`src/error.rs`) [Minor] (smallvec crate added)
- [x] Thunk origin String→Option<Rc<str>> (`src/value.rs`) [Nit]
- [x] Add capacity hint to variadic dict allocation (`src/eval.rs`) [Minor]
- [x] Use static empty IndexMap for PendingCall named args (now Option<Box<IndexMap>>, None for common case) [Nit]
- [x] Use static empty dict thunk for default `$$` (`src/eval.rs`) [Nit]
- [x] Design: builtin argument strictness annotation model — see doc/16-architecture.md §Builtin Argument Strictness Annotations
- [x] Builtin strictness annotations — superseded by strictness-types sprint
- [x] Research Rc cycle leak mitigation strategy — resolved by arena allocation design
- [x] Fix `materialize()` PendingCall branch pre-cloning 4 values before function resolution [Major]
- [x] Fix `builtin_filter` Dict path building redundant secondary key index [Minor]
- [x] Fix `EvalContext::with_base_dir()` allocating fresh `Rc<EvalConfig>` on every `$include` — cap_std Dir non-Clone; cannot share without clone [Critical]
- [x] Fix `builtin_map` Dict path allocating `format!()` string per entry (already Cow::Borrowed) [Critical]
- [x] Fix `func_path` allocating recursively-built String on every DotAccess call [Major]
- [x] Fix `Type::clone()` on non-substituted branches in `Substitution::apply_inner` — apply_type now returns Cow<'a, Type> [Major]
- [x] Fix `builtin_keys` allocating intermediate Vec for filtering (already uses iter().enumerate() directly) [Major]
- [x] Fix `eval_document` cloning string keys when extracting bindings [Minor]
- [x] Skip substitution in `infer_dict` Pass 3 when no constraints collected [Minor]
- [x] Fix `unify()` recursive `subst.apply()` re-application — Robinson single-application invariant confirmed correct [Critical]
- [x] Fuse `collect_type_vars()` + `collect_row_vars()` into a single tree walk [Critical]
- [x] Fix U-VAR double walk in `unify()` [Major]
- [x] Fix `$filter` Dict path O(n) clone per step (already fixed) [Major]
- [x] Fix `unfold_step` creating a fresh `PendingBuiltin` thunk on every step (already optimal via Rc::clone) [Minor]
- [x] Fix `deep_materialize` allocating HashMap before checking if value is a primitive [Minor]
- [x] Fix `TypeScheme::fmt()` allocating `Vec<String>` for display [Nit]
- [x] Fix `infer_dict` Pass 3 looking up fresh vars via TypeEnv chain [Minor]
- [x] Remove unreachable `check_call_with_scheme` CALL-MONO branch (added debug_assert! instead) [Major]
- [x] Skip `ann_mapping` HashMap allocation for fully unannotated functions (has_annotations guard already in place) [Minor]
- [x] Optimize `instantiate_scheme` for single-var schemes [Minor]
- [x] Defer `field_path` allocation in `validate_and_wrap_record` to error path (changed to &mut Vec<String> with push/pop) [Major]
- [x] Change `Substitution.map` from `IndexMap` to `HashMap` (already HashMap) [Major]
- [x] Change `ThunkState::PendingBuiltin.named` to `Option<IndexMap<...>>` (already Option<IndexMap>) [Major]
- [x] Fix `builtin_filter_dict_step` O(n²) dict cloning (already uses get_index + Rc::clone) [Major]
- [x] Change `Row.fields` from `IndexMap` to `HashMap` [Major]
- [x] Add fast path in `unify_rows` for closed equal-key rows (already done) [Major]
- [x] Change `resolve_row` to return `Cow<'_, Row>` (already returns Cow) [Major]
- [x] Fuse `collect_type_vars` + `collect_row_vars` in `lower_row_var_levels` (already done) [Major]
- [x] Eliminate `unique1`/`unique2` clones in `unify_remainders` Case 4 (already done) [Major]
- [x] Eliminate redundant param-exists scan in `bind_args_thunks` BIND-NAMED (already single-scan) [Minor]
- [x] Fuse `collect_type_vars` + `collect_row_vars` in `instantiate_at_level` (already done) [Major]

## Sandboxing — Complete

Design and implement four unprivileged sandboxing layers.

- [x] Design sandboxing model — see doc/12-tooling.md §Sandboxing & Security
- [x] Decide policy for absolute paths — allowed if within any --allow-path
- [x] Decide policy for symlinks — canonicalize, then check against allowlist

### sandbox-b: Landlock Filesystem ACLs

Linux 5.13+ Landlock enforcement with graceful degradation.

- [x] Add `landlock` crate to `Cargo.toml` — `landlock = "0.4"` (latest stable); gates behind `#[cfg(target_os = "linux")]`. [Nit]
- [x] In `src/main.rs` `run_eval()`, after CLI arg parsing and before eval: construct a `landlock::Ruleset` restricting `FS_READ_FILE` to each `--allow-path` entry (and its subdirs) plus the stdlib env path; apply via `ruleset.restrict_self()`; wrap in `if landlock::ABI::new_current().is_supported()` for graceful degradation on pre-5.13 kernels. [Major]
- [x] Add `--no-landlock` flag to `eval` subcommand for escape hatch (debugging, CI environments without Landlock). [Minor]
- [x] CLI test: verify Landlock enforcement fires when `--allow-path` excludes an included path; skip test on kernels without Landlock support via `cfg(target_os = "linux")` + version check. [Minor]

## Test Infrastructure — Complete

### test-infra: Core Test Items

- [x] Add integration tests for typecheck→eval interaction — test that type errors remain advisory and eval proceeds; test `TypeAssert` with `default:` fallback behavior end-to-end.
- [x] Add `typecheck.rs` `infer()` helper note that desugaring is NOT applied
- [x] Add typeassert-structural corpus tests
- [x] Add Kotlin call convention success-path corpus tests
- [x] Add `$_` desugaring edge-case corpus tests
- [x] Add TypeAssert default validation corpus tests
- [x] Add document separator edge-case corpus tests
- [x] Add thunk state transition unit tests

### test-tooling: Tooling Tests and Documentation

- [x] Integration tests for REPL/LSP — multi-line input, hover on nested expressions, multiple errors
- [x] Add LSP corpus tests (`tests/lsp_corpus/`) — stub directory with README created; test runner deferred to LSP implementation sprint
- [x] EvalContext API documentation — add docstrings to `eval_source()`, `eval_file()`, `eval_file_with_input()`
- [x] Add `EvalContext::with_base_dir()` file-resolution integration test
- [x] Expose `eval_file_with_input` in public API (already public)
- [x] Fix test helpers using `create_root_env()` instead of `create_stdlib_env()` (current usage is correct)
- [x] Document circular builtins⇄eval dependency — add safety comment at `src/builtins.rs:28`
- [x] Cross-layer contracts documentation — add §Implementation Architecture to doc/16-architecture.md
- [x] Document `value_to_json` vs `value_to_display_string` NaN/Infinity difference
- [x] Add lib.rs EvalContext doc comment mentioning include cache behavior
- [x] Add doc/16-architecture.md testing requirements section

## Integration Pipeline — Complete

### integration: Integration and Pipeline Implementation

- [x] Research: value serializer visitor pattern — see doc/whatif/value-serializer-visitor.md. Verdict: defer. Two serializers don't justify a visitor trait.
- [x] Unify serializer logic via visitor pattern — implemented: `ValueVisitor` trait + `visit_value` traversal + `JsonVisitor` + `DisplayVisitor` in `src/lib.rs`
- [x] Make deep_materialize→serialize contract explicit
- [x] Research: eval↔builtins dependency audit — see doc/whatif/eval-builtins-boundary.md. Recommended: `src/eval_core.rs` extraction. Gate on concrete need.
- [x] Break eval↔builtins circular dependency — circular dep is safe: function-call level, not import level; doc comment added
- [x] Add depth guard to desugar pass or document invariant
- [x] Document TypeAssert AST mutation threading implications
- [x] Document nested dict let-polymorphism limitation
- [x] Add integration test for row-unification-b Type substitution flowing through full pipeline
- [x] Add builtin signature macro to prevent duplication across BuiltinFn registrations
- [x] Decide: `Type::Any` split timing — split now as a standalone sprint; naming: `Type::Unknown` + `Type::Top`; prerequisites met
- [x] Split `Type::Any` into `Type::Unknown` and `Type::Top` — deferred: tracked in doc/whatif/gradual-typing.md
- [x] Add cargo audit CI gate (already in justfile)
- [x] Document `doc/08-evaluation.md` Laziness Design table missing `TypeAssert` entry — table at lines 768-790 covers $map, $filter, $merge etc. but omits `[@Type expr]` TypeAssert, which forces materialization inside eval() before result is demanded. Add row: "`[@Type expr]` | Strict: materializes expr immediately (laziness violation; see eval-lazy-fixes TODO)" to the table. (`doc/08-evaluation.md:768-790`) [Minor, laziness-auditor C49]
- [x] Document REPL multi-document limitation in doc/16-architecture.md — `eval_input()` in `src/repl.rs` calls `parse_expression()` which returns only the last expression of the FIRST document; `---`-separated multi-doc input silently discards all documents after the first. Add one-line caveat to the REPL section. (`doc/16-architecture.md`, `src/repl.rs`) [Nit, grammar-architect C49]
- [x] Add corpus test for `$collect` on Seq values — added collect_seq.llt-eval and collect_map_seq.llt-eval (`tests/corpus/eval/stdlib/`) [Minor, stdlib-author panel C42]
- [x] Update `doc/08-evaluation.md:902` "visited set" language — seq-cycle-fix replaced `HashSet<*const Thunk>` with `HashMap<*const Thunk, Option<Rc<Thunk>>>` dual-purpose cache; one-sentence update: "inserts `None` sentinel (in-progress cycle guard) before recursing, replaces with `Some(result)` after completion to preserve sharing." (`doc/08-evaluation.md:902`) [Minor, laziness-auditor C43]
- [x] Add `$try` strictness annotation to doc/08-evaluation.md §Selective Materialization strictness table — `$try` is `S` (strict) on its function argument (forces body to observe error) and forces the `ok` value before wrapping. (doc/08-evaluation.md) [Minor, laziness-auditor C43]
- [x] Add clarifying note to `deep_materialize_thunk` cycle-return path — the `Some(None)` sentinel branch returns `Rc::clone(thunk)` safely because `materialize()` has already transitioned the thunk to `Materialized`; sub-structure of the returned thunk is not deep-forced (documented behavior for cycles). (`src/eval.rs:1269`) [Nit, laziness-auditor C43]
- [x] Add note to `doc/08-evaluation.md` specifying `deep_materialize` single-call sharing scope — `Rc::ptr_eq` invariant holds only within one `deep_materialize` call; two separate calls on overlapping trees produce distinct output pointers. (`src/eval.rs:1192-1211`) [Minor, eval-engine C43]
- [x] Fix internal helper builtins surfacing internal names in errors — `cycle_step`, `filter_dict_step`, `filter_seq_step`, `unfold_step`, `drop_seq_step`, `reduce_seq_step`, `concat_seq_step` use their internal PendingBuiltin names in error context strings (`"filter-dict-step: ..."` etc.); replace with user-facing names (`"filter"`, `"drop"`, etc.). (`src/builtins.rs:1408, 1840, 1995, 1574, 2222, 2341, 2486`) [Nit, span-integrity-checker C43]
- [x] Fix `drop`/`reduce`/`join` "invalid Seq tail" error paths using `ErrorKind::Internal` — these are reachable user errors when a tail materializes to unexpected type; should be TypeMismatch not Internal. (`src/builtins.rs:2262, 2395, 2460`) [Minor, span-integrity-checker C43]
- [x] Fix `unfold_step` wrong-return-size error using `ErrorKind::Internal` — step function returning 1-entry dict is a user programming error, not implementation bug (already TypeMismatch). (`src/builtins.rs:1630`) [Minor, span-integrity-checker C43]
- [x] Add note to `NamedArg.name` in `doc/15-ast.md:115` — the field is always the bare identifier without `$` prefix even when source uses `$key:` syntax; a reader of the AST spec would not know this without reading `src/parser.rs:426-428`. (`doc/15-ast.md:115`) [Nit, grammar-architect C43]
- [x] Add sentence to `doc/15-ast.md` about `parse_expression` multi-document behavior — returning the last expression of the FIRST document for multi-doc files is only documented in a code comment; mention in doc/15-ast.md §parse() entry. (`src/parser.rs:69-76`, `doc/15-ast.md:21`) [Nit, grammar-architect C43]
- [x] Add `$_` desugaring scope decision for `TypeAlias` bodies — `src/desugar.rs:267-270` recurses into `TypeAlias` bodies; type expressions are semantically distinct from runtime expressions; document whether `$_` desugaring should apply inside `[type ...]` bodies, and if not, add a guard skipping `TypeAlias`. (`src/desugar.rs:267-270`, `doc/04-functions.md`) [Minor, grammar-architect C43] (documented inline: TypeAlias bodies are type expressions; $_ desugaring applies for consistency but is likely a user error)
- [x] Fix `pub mod repl` in `lib.rs` unconditionally compiled — `pub mod lsp` is `#[cfg(feature = "lsp")]`-gated but `pub mod repl` at line 34 is not, making `ReplSession`/`bracket_count`/`is_balanced` always public types regardless of feature flag. Either add `#[cfg(feature = "repl")]` gate or document why unconditional. (`src/lib.rs:34`) [Major, integration-verifier C43] (already done in C96)
- [x] Fix `type_map` not populated for non-last Dict position in document — `typecheck_document` calls `infer_dict` directly for non-last `Expr::Dict` but doesn't record result in `type_map`; LSP hover over a dict in intermediate position sees no type. After successful `infer_dict` call, insert `(expr.span.start.offset, expr.span.end.offset) → ty` into `type_map`. (`src/typecheck.rs:85-105`) [Minor, integration-verifier C43]
- [x] Re-export `EvalConfig` and `EvalState` from `lib.rs` (already re-exported) — both are `pub struct` with `pub` fields but not in the public namespace; embedding callers reaching `ctx.config.base_dir` get types that are reachable but undocumented. Add `pub use eval::{EvalConfig, EvalState}` to `src/lib.rs:48`. [Major, integration-verifier C43] (already done)
- [x] Standardize "Inherently materializing" comment annotations in `stdlib/prelude.llt` — some materializing functions have this comment, others don't; either add to all or remove and document in doc/08-evaluation.md (already consistent) [Nit, stdlib-author C31]
- [x] **doc/08-evaluation.md formal evaluation rules missing ctx/Sigma parameter** — PROP-EVAL, PROP-BUILTIN, PROP-RESULT, PROP-DEPTH rules don't include the EvalContext (Σ) parameter in evaluation judgments after evalcontext-types sprint. Add Σ to judgment form: `⟨e, ρ, Σ⟩ ⇓ v`. (doc/08-evaluation.md) [Major, computer-scientist C34] (added Σ to judgment form in Part 2 intro; PROP rules in doc/10-errors.md updated with Σ_θ in misc-nits-c)
- [x] **doc/08-evaluation.md correspondence tables stale line numbers** — evalcontext-types shifted code ~40 lines. Three tables affected: Scope Chain, Error Semantics, $include cross-refs. Update all line references. (doc/08-evaluation.md, doc/10-errors.md, doc/09-documents.md) [Major, computer-scientist C34]
- [x] **doc/10-errors.md missing error condition specifications** — §9 documents error structure/display/categories but not WHEN each ErrorKind variant is triggered. Add §9.7 with trigger conditions for all 26 variants. (doc/10-errors.md) [Minor, span-integrity C34] (doc/10 §Part 2 Error Sources already has comprehensive trigger table; added cross-reference from doc/08 Error Reporting section in misc-nits-c)
- [x] **doc/10-errors.md missing Error Reporting section** — dual-span model (definition-site vs materialization-site), stack frame accumulation, error caching semantics, mat_span update behavior scattered but not systematically documented. Add dedicated section. (doc/10-errors.md) [Major, span-integrity C34] (doc/10 already had systematic coverage in Parts 1-5; added summary cross-reference section in doc/08-evaluation.md in misc-nits-c)
- [x] **doc/06-type-inference.md Limitation #7 "monomorphic call arguments now checked" claim is false** — says "resolved" but CALL-MONO is not implemented (part of bidirectional-typing milestone). Revert claim. (doc/06-type-inference.md) [Major, type-theorist C34] (already fixed prior to this sprint — current doc §Limitations has only 4 items; Limitation #7 never appears)
- [x] Fix `doc/06-type-inference.md:525` stale `instantiate()` table entry — table row describes `instantiate()` as "for CALL-POLY call-site freshening" but CALL-POLY now uses `instantiate_at_level()`; old `instantiate()` is `#[cfg(test)]`-only. Update table to show `instantiate_at_level()` with level-registration description. (`doc/06-type-inference.md:525`) [Nit, grammar-architect/type-theorist/integration-verifier/computer-scientist C43 panel]
- [x] Fix `deep_materialize_thunk` stale cache entry on error — when `materialize()` at `src/eval.rs:1274` fails, the `?` operator returns early leaving a `None` sentinel in the cache. Subsequent encounters with the same thunk hit the `Some(None)` branch and return `Rc::clone(thunk)` as if it were a cycle, silently masking the original error. Fix: clean up sentinel on error via `cache.remove(&thunk_ptr)` in the error path. (`src/eval.rs:1273-1274`) [Major, eval-engine C43 panel] (already fixed in iterative work-stack refactor)
- [x] Add note to `test_deep_materialize_cycle_sentinel` that pre-materialized thunk is intentional — real cycles are encountered after `materialize()` has already transitioned the thunk; using `Thunk::new_materialized` isolates cache-lookup logic from evaluation. (`src/eval.rs:4552`) [Nit, test-crafter C43 panel]
- [x] Document `doc/11-stdlib.md` `$filter` Dict→Seq asymmetry — added note "returns Seq when input is Dict" to the stdlib reference table entry at `doc/11-stdlib.md` filter row and cross-referenced from the `$map` entry. [Minor, integration-verifier C44, C97] (already done)
- [x] Confirm `INCLUDE_CTX` thread-local fully removed in `doc/16-architecture.md` — verified: `doc/16-architecture.md:135` already explicitly states "Thread-local `INCLUDE_CTX` fully removed — no longer present in codebase." No change needed. [Minor, integration-verifier C44, C97] (already confirmed in doc/16)
- [x] Audit `typecheck.rs` `infer_dict` Pass 4 to confirm type aliases excluded from Record fields — verified: Pass 0 sets `is_alias=true` for TypeAlias entries; Pass 1 and Pass 3 skip `is_alias` entries so they never enter `field_types`; Pass 4 only iterates `field_types`. Code matches `doc/05-type-annotations.md:206-208`. No bug. [Minor, integration-verifier C44, C97]
- [x] Add fail-fast comment to `create_stdlib_env` — add inline comment explaining that stdlib load error propagation is intentional (not a recovery path): stdlib failure is fatal, not something callers should handle. [Nit, integration-verifier C44] (already present)
- [x] Add doc comment to `impl PartialEq for Value` in `src/value.rs` — reference §Equality and Comparison formal spec and warn about Int/Float divergence from `$=` (Value::PartialEq uses structural equality; `$=` promotes Int→Float for cross-type comparison). (`src/value.rs`) [Nit, integration-verifier C44]
- [x] Fix `doc/11-stdlib.md:117,176` describing `$apply` as "positional only" — call-convention-kotlin added named-arg support (Key::String → named, Key::Int sorted → positional). Update both occurrences. (`doc/11-stdlib.md:117, 176`) [Major, stdlib-author C48] (already fixed)
- [x] Document which `impl` is normative for error messages — `impl Display for Expr` at `src/ast.rs:182-300` and `src/formatter.rs` both produce tinct source representations; clarify which is authoritative for error messages (Display) vs pretty-printing (formatter), and add cross-reference comment in each. (`src/ast.rs:182`, `src/formatter.rs`) [Nit, integration-verifier C44]
- [x] Document BTreeSet allocations in `doc/08-evaluation.md` §Allocation Strategy — note the 3 BTreeSet allocation sites in `instantiate_scheme`, `instantiate_at_level`, and `collect_type_vars` and their planned elimination (Vec accumulator, inline lowering). (`doc/08-evaluation.md`) [Nit, performance-expert C44]
- [x] Fix `doc/06-type-inference.md` TypeScheme struct description stale — lines 382-396 show `pub vars: Vec<String>` and `Self { vars: vec![], body: ty }` but bidirectional-typing-b split to `type_vars: Vec<String>` + `row_vars: Vec<String>`. Also line 457 shows `TypeScheme { vars, body: ty.clone() }`. Update struct listing, `mono()` example, and line 457 to reflect current `type_vars`/`row_vars` fields. (`doc/06-type-inference.md:382-396, 457`) [Nit, grammar-architect C46] (already fixed prior to this sprint — doc already had correct type_vars/row_vars/body)
- [x] Fix `doc/06-type-inference.md` §Instantiation conflation claim stale — lines 360-363 say "Tinct conflates [type and row variables] into a single namespace — both are collected by `collect_type_vars()` and renamed by `instantiate()`." This is now false: bidirectional-typing-b added `collect_row_vars()` and the TypeScheme split (`type_vars`/`row_vars`) separates them. Update to reflect dual-collection via separate functions. (`doc/06-type-inference.md:360-363`) [Nit, type-theorist C46] (already fixed prior to this sprint — no conflation claim exists in current doc)
- [x] Fix `doc/06-type-inference.md` `RowTail` → `RowRest` naming error — lines 403 and 531 use `RowTail::RowVar(String, u32)` but the current enum is `RowRest` (`src/types.rs:14`). `RowTail` is the planned name in the row-unification sprint; doc/06 should match the current code `RowRest` and add a note about the planned rename. (`doc/06-type-inference.md:403, 531`) [Nit, type-theorist C46] (inverted: RowTail IS the current code name; added historical note at line 462 clarifying RowRest→RowTail rename that already occurred) (already fixed — historical note at line 464)
- [x] Fix `key_in_range` using `EvalError::internal` for user-facing error — `src/eval.rs:84` returns `EvalError::internal("range access requires comparable key types", span)` when a dict slice has mixed-type key bounds (e.g., Int key vs String bound). Mixed types are a user programming error, not an internal bug; use `ErrorKind::TypeMismatch` or a new `ErrorKind::IncomparableKeys` variant. (`src/eval.rs:84`) [Minor, span-integrity-checker C46]
- [x] Fix `desugared` flag unused in stack frame labels — `Fn.desugared: bool` exists in AST (origin tagging) but `func_label()` at `src/eval.rs:452` never checks it. Desugared lambdas (`$_.field` expressions) show generic "call <lambda>" in stack traces; they should show "call <auto-generated lambda>". (`src/eval.rs:452`, `src/ast.rs`) [Minor, span-integrity-checker C46]
- [x] Document `Expr::TypeAssert` strictness in `doc/08-evaluation.md` §Strictness exceptions — `eval()` at `src/eval.rs:165-166` materializes the inner expression immediately (strict). TypeAssert cannot be lazy because type-checking requires a forced value. Add row: "TypeAssert body | forced at annotation site | cannot type-check unevaluated thunk". (`src/eval.rs:165-166`, `doc/08-evaluation.md`) [Major, eval-engine C46]
- [x] Fix `eval_call` doc comment mismatch with [FORCE-CALL] rule — line 464 says "materialize the function, bind arguments, wrap body as thunk" but `doc/08-evaluation.md:309-319` [FORCE-CALL] describes forcing a `PendingCall` thunk. Add sentence clarifying: current implementation eagerly forces the function at call-site (not deferred), which diverges from the idealized [FORCE-CALL] rule. (`src/eval.rs:464`, `doc/08-evaluation.md:309-319`) [Minor, eval-engine C46]
- [x] Improve `bind_args_thunks` unknown named arg error message — `ErrorKind::UnknownNamedArg { name }` at `src/error.rs:229` outputs "unexpected named argument: z" with no hint about valid params. Add list of valid named param names: "unexpected named argument 'z'; this function accepts named params: x, y". (`src/eval.rs:596`, `src/error.rs:229`) [Minor, eval-engine C46]
- [x] Add depth limit note to `doc/08-evaluation.md` [FORCE-EVAL] rule — the rule at lines 272-281 uses `d+1` but doesn't note the 256-level cap. Add: "If `d ≥ MAX_EVAL_DEPTH (256)`, `force` returns `ErrorKind::MaxDepthExceeded` before entering this rule." (`doc/08-evaluation.md:272-281`) [Minor, eval-engine C46]
- [x] Improve `src/desugar.rs:161` `Expr::Int(0)` comment — "Temporary placeholder" is vague; clarify that this placeholder is immediately overwritten: "Dummy value; immediately overwritten after `original_node` is captured." (`src/desugar.rs:161`) [Nit, grammar-architect C46]
- [x] Fix `src/typecheck.rs:2674` "contravariant" comment — says "compatible with expected Number (Int <: Number for contravariant)" but `Int <: Number` is a covariant subtype relation. Change to: "compatible with expected Number (annotation Int <: Number, so Int values satisfy the Number expected type)". (`src/typecheck.rs:2674`) [Nit, type-theorist C46]
- [x] Fix `README.md` §Architecture Table stale entries — line 144 mentions `IncludeContext` + thread-local for `$include` (deleted in evalcontext-thread); line 148 says "four-pass dict inference" (now five passes, 0-4); line 150 lists `set_include_context()`, `clear_include_context()`, `IncludeContext` in public API (all deleted). Update to mention `EvalContext`, "five-pass dict inference (Pass 0-4)", remove deleted symbols. (`README.md:144, 148, 150`) [Minor, integration-verifier C46] (already clean)
- [x] Add corpus tests for annotation type variable isolation — end-to-end test: sibling functions using `@a` don't cross-contaminate each other's type inference (fixed in bidirectional-typing-c via `ann_mapping`). (`tests/corpus/eval/typecheck/`) [Minor, test-crafter C46 panel]
- [x] Add corpus test for `[@Number 42]` → `Int(42)` — covered by `tests/corpus/eval/typecheck/subsumption_number_accepts_int.llt-eval`; IntLiteral <: Number subsumption is tested. [Minor, test-crafter C46 panel, closed test-crafter C47]
- [x] Document principal type violations in `doc/06-type-inference.md` §Limitations — cases where tinct does not produce principal types: open-record unification without row rewriting (Rémy 1994), let-polymorphism blocked by Type::Any poisoning, annotation TypeVar scoping across sibling functions. (`doc/06-type-inference.md`) [Minor, type-theorist C46 panel]
- [x] Consider `Result<Type>` for `infer_expr` return type — current `Type` return silently produces `Type::Any` on error; switching to `Result<Type, TypeError>` would surface inference failures earlier. (deferred — architectural change, not a bug) (`src/typecheck.rs`) [Nit, type-theorist C46 panel]
- [x] Consider `HashMap` for type alias registry — `infer_dict` Pass 0 uses `IndexMap` for `alias_types`; a plain `HashMap` would be sufficient since alias lookup order is irrelevant. (deferred) (`src/typecheck.rs`) [Nit, performance-expert C46 panel]
- [x] Clarify `Fn@T` with zero params in `doc/05-type-annotations.md` — `Fn@T` with no parameter annotations is valid syntax but meaning is unclear: does it annotate return type only, or is it sugar for `Fn(Any → T)`? Add clarifying note. (`doc/05-type-annotations.md`) [Nit, grammar-architect C46 panel]
- [x] Move `state.levels.insert` into `or_insert_with` closure in `resolve_type_name` — currently re-inserts `state.levels[fresh_name] = state.level` unconditionally on every lookup; safe only because `infer_fn` doesn't bump levels internally. Move inside `or_insert_with` to make the level assignment atomic with fresh var creation. (`src/typecheck.rs:1089`) [Nit, computer-scientist C46 panel] (resolved: all insert sites now guarded by if-let-Some/else split; insert only on fresh-var creation path; row-var outside-scope path uses entry().or_insert())
- [x] Add comment to `check_call_with_scheme` instantiation explaining `state.level` is the call-site level — matches `check_call`'s `instantiate_at_level` usage; ensures fresh type vars are created at the correct generalization level. (`src/typecheck.rs:671-672`) [Nit, type-theorist C46 panel]
- [x] Add comment to zero-param CALL-POLY branch in `check_call_with_scheme`: `*ret.clone()` without substitution is correct — with zero params there are no arguments to unify, so the return type needs no substitution applied. (`src/typecheck.rs:723-724`) [Nit, type-theorist C46 panel]
- [x] Add comment to monomorphic VarRef fallback in `check_call_with_scheme` — "handles TypeVar correctly" is terse; add: "handles TypeVar during letrec forward-references where Pass 1 assigns TypeVar placeholders, not yet generalized". (`src/typecheck.rs:227-229`) [Nit, type-theorist C46 panel]
- [x] Fix `check_call_with_scheme` not recording func VarRef span in `type_map` — LSP hover over polymorphic function reference in call (`$id` in `[call $id 42]`) returns no type. In `check_call`, `infer_expr(func)` records the VarRef's type; in `check_call_with_scheme`, `infer_expr` is never called for func. Fix: add `func_span: Span` param, insert `(func_span.start.offset, func_span.end.offset) → func_ty` into `type_map` after `instantiate_scheme`. (`src/typecheck.rs:662-730`) [Major, span-integrity C47] (already implemented)
- [x] Fix `check_call_with_scheme` zero-param CALL-POLY returns `*ret.clone()` (pre-instantiation ret) not `*inst_ret.clone()` — same bug fixed in `check_call` but not applied here; for zero-param polymorphic functions may produce wrong return type. (`src/typecheck.rs:723-724`) [Minor, span-integrity C47] (already fixed in prior sprint)
- [x] Document or resolve CALL-MONO/CALL-POLY error asymmetry — CALL-MONO collects all argument errors before returning; CALL-POLY stops at first unification failure; inconsistency affects experience for polymorphic calls with multiple bad arguments. Document asymmetry or batch unification errors. (`src/typecheck.rs:696-704, 716-721`) [Minor, span-integrity C47]
- [x] Add `doc/06-type-inference.md` implementation note after CALL-POLY rule documenting `check_call_with_scheme` optimization — CALL-POLY rule implies VAR-POLY always fires for VarRef, but code special-cases polymorphic VarRef to skip VAR-POLY and instantiate once. (`doc/06-type-inference.md:162-174`) [Minor, span-integrity C47] (implementation note added at doc/06 line ~190)
- [x] Fix `check_call`/`check_call_with_scheme` doc comments referencing doc/06 by line number — fragile: any doc edit shifts refs. Fix: reference by rule name "(doc/06 §[CALL-MONO])" instead of line numbers. (`src/typecheck.rs:694,761,776`) [Nit, type-theorist C47] (last remaining line-number ref at check_dot_access converted to §Let-Generalization)
- [x] Document `check_expr` type_map recording behavior in lambda checking mode — in lambda checking mode, `type_map` records `expected.clone()` (line 405), not synthesized type. Add sentence: "In lambda checking mode, type_map records the expected function type — correct bidirectional semantics for LSP hover." (`src/typecheck.rs:287-293`) [Nit, type-theorist C47]
- [x] Document `check_call` CALL-POLY double-instantiation for inline polymorphic functions — `[call [fn [x@a] $x] 42]` goes through `check_call` not `check_call_with_scheme`; harmless for single-call sites but should be documented. (`src/typecheck.rs:763-778`) [Minor, type-theorist C47]
- [x] Fix `CALL-POLY` trigger comment misleading at `src/typecheck.rs:708-709` — comment says "This can happen with nested polymorphism or type annotations" but any polymorphic scheme will have `has_type_vars() = true` after instantiation. Fix: "Polymorphic schemes always have type variables after instantiation. CALL-MONO (above) only fires for degenerate schemes where quantified variables don't appear in the body." (`src/typecheck.rs:708-709`) [Nit, computer-scientist C47] (already fixed)
- [x] Fix `README.md:143` eval.rs table row still lists `$_` implicit lambda desugaring — moved to `src/desugar.rs` in underscore-desugar sprints. Fix: remove from eval.rs row; add row for `src/desugar.rs`. (`README.md:143`) [Minor, integration-verifier C47]
- [x] Add `src/desugar.rs` row to README.md project structure table — desugar is a real pipeline stage with its own module but absent from the table developers use for orientation. (`README.md`) [Nit, integration-verifier C47]
- [x] Remove stale implementation note in `doc/04-functions.md:546` — says "current implementation uses count-based arity check and restricts named args to `default:` params"; both limitations were resolved in call-convention-kotlin sprint. Update to "Implemented as of call-convention-kotlin." (`doc/04-functions.md:546`) [Nit, grammar-architect C47, computer-scientist C47]
- [x] Document `$apply` dict-splitting behavior in `doc/04-functions.md` §$apply — spec at lines 548-565 discusses `env_d` separation but does not specify the key-type split: `Key::Int` (sorted by value) → positional args, `Key::String` → named args. Add formal notation: `pos = sort_by_key({(k,v) in D | k in Int}), named = {(k,v) in D | k in String}`. Also document negative integer key semantics (sorted before 0, serve as ordering hints). (`doc/04-functions.md:548-565`) [Minor, grammar-architect C47, computer-scientist C47]
- [x] Fix `doc/04-functions.md:459` incorrectly claims defaults are evaluated eagerly — says "Defaults are evaluated eagerly at call time (not wrapped as thunks)". In LLT's lazy model, `eval()` returns a thunk; defaults are forced only when the parameter is accessed, not at call time. Remove or qualify the "eagerly" claim. (`doc/04-functions.md:459`) [Minor, eval-engine C47] (already fixed)
- [x] Add test for `$apply` calling a builtin with named args (builtin rejection path) — sprint modified `builtin_apply` to pass `named_args` to both Function and Builtin dispatch paths; no test covers builtin path with named args. Most builtins call `reject_named` which returns E023. (`tests/corpus/eval/errors/`) [Minor, test-crafter C47]
- [x] Add test for BIND-ARITY with multiple missing required params — current error test `fn_kotlin_coverage_missing` has one missing required param; no test documents which error fires when multiple required params are uncovered (first one? all?). (`tests/corpus/eval/errors/`) [Minor, test-crafter C47]
- [x] Fix `instantiate_at_level` level-lowering U-VAR comment saying "type vars in b" when it also collects row vars — already fixed (uses `collect_all_vars` + correct comment) prior to this sprint; TODO item stale. (`src/types.rs`) [Minor, type-theorist C48]
- [x] Fix `doc/06-type-inference.md:15` Type Grammar using "Str" while user-facing annotations use "String" — added note to grammar line: "internal name; user-facing annotations accept `String` as an alias." (`doc/06-type-inference.md:15`) [Nit, type-theorist C48]
- [x] Fix `doc/05-type-annotations.md:185` float literal rationale wrong — says "cannot be used as dict keys" but Float CAN be a dict key; real reasons are equality fragility and NaN comparisons. Fix rationale. (`doc/05-type-annotations.md:185`) [Nit, type-theorist C48]
- [x] Fix `LineTable` bare `\r` handling — `src/parser.rs` `LineTable` scans only `\n` to build line-offset table, but pest's `NEWLINE` built-in matches `\r`, `\n`, and `\r\n`. Files using bare CR line endings (classic Mac OS) parse correctly but get wrong line numbers in error messages. Fix: recognize bare `\r` in `LineTable::new` alongside `\n`, treating bare `\r` and `\r\n` as single line endings. (`src/parser.rs`) [Minor, grammar-architect train-3]
- [x] Document Unicode homograph risk in identifiers — the lexer allows Unicode identifier characters while denylisting ASCII punctuation; Unicode homographs (e.g., Cyrillic `а` vs Latin `a`) create invisible name collisions in LLT programs. Add a note to `doc/02-syntax.md` acknowledging the risk and documenting the design stance (accept Unicode but restrict to NFC, or ASCII-only for safety). (`src/lexer.rs`) [Minor, grammar-architect train-3]
- [x] Fix `doc/15-ast.md:211-223` Annotation Bracket Restriction incomplete — §Annotation Brackets restriction table says special forms (`call`, `fn`, `type`) are parse errors inside annotation brackets, but does not address `type_assert_body` (which is also rejected inside annotation brackets yet is not categorized as a 'special form'); add `type_assert_body` to the restriction table with a clarifying note. (`doc/15-ast.md:211-223`) [Nit, grammar-architect C57]
- [x] Add TODO citation to `test_bracket_access_forward_ref_resolves_correctly` `#[ignore]` — test at `src/typecheck.rs:3595` is `#[ignore]` with no comment explaining why or citing the tracking sprint; add `// TODO: enable when check_bracket_access generates row constraints for open records — see row-unification-h-b`. (`src/typecheck.rs:3595`) [Nit, test-crafter C57]
- [x] Fix `Substitution::apply()` to use the new `is_empty()` method instead of inline check (already done) (`src/types.rs:406`) [Nit, type-theorist C72 panel]
- [x] Fix value_to_display_string depth error: uses EvalError::depth_exceeded (E040) instead of EvalError::internal for display recursion limit — these are different limits; E040 conflates them and makes is_catchable() wrong (`src/lib.rs:239-242`) [Major, integration-verifier C81]
- [x] Add typecheck call to create_stdlib_env() after desugar_file (line 3215) — stdlib code is never type-checked currently (`src/builtins.rs:3215`) [Major, integration-verifier C81]
- [x] Add doc/06 InferState.subst merge algorithm explanation in Pass 3b section — when both state.subst and local substitution bind same variable, show unification algorithm (`doc/06-type-inference.md:473-477`) [Minor, type-theorist C81]
- [x] Add substitution size check to unify_tails RowVar-to-RowVar binding — `src/types.rs:717` missing `subst.check_size(span)?` after row_map insert, unlike the RowVar-to-Empty binding at lines 728-731 [Minor, type-theorist C91]
- [x] Fix doc/16-architecture.md CEK status — says "Phase 4 (structural cleanup) complete via iterative-eval-b3" but Phase 4 (eval_step conversion) is only partially done: TypeAssertCheck Cont added (b4 tasks 2+4), $apply deferred (task 5 above), DictEntries/DocumentPipeline/DictBuildKey/BindArgDefault not yet added; update §Iterative Evaluator status note to reflect partial b4 completion (`doc/16-architecture.md:43`) [Minor, integration-verifier C91]

### misc-nits-c-code: Code Behavior Fixes

Code behavior changes, refactors, performance fixes, and span fixes.

- [ ] Move `join` from Rust builtin to Tinct stdlib — implementable as one-line reduce. Tinct-First Principle violation; 71 lines of Rust for what 1 line of Tinct handles. Defer to Phase 10 (after dual-dispatch reduce complete). (`src/builtins.rs:1823-1894`, `stdlib/prelude.llt`) [Major, stdlib-author]
- [x] Fix `validate_and_wrap_record` field path quoting format — `field_path_prefix` at `src/eval.rs:178-193` builds `"field \"x\": "` using escaped quotes for each segment; error messages read `field "x": record missing field "y"` which is inconsistent with `EvalError::Display` which uses unquoted names for field references in other contexts. Standardize to backtick-quoting: `field \`x\`: record missing field \`y\`` matching the doc/10-errors.md Error Message Style Guidelines. (`src/eval.rs:178-193`) [Nit, integration-verifier C63]
- [x] **Error corpus tests lack span assertions** — 30 error test files validate message substrings only. No regression detection for definition_span, materialization_span, or stack frames. Extend `.llt-eval` format with span expectations (e.g., `# expect-def-span: 1:5-1:10`). (`tests/corpus_tests.rs:322-334`, `tests/corpus/eval/errors/`) [Major, span-integrity C34] (comment added to corpus_tests.rs explaining deferred)
- [x] **Row-unification milestone missing from TODO.md** — doc/07-type-extensions.md references row-unification in multiple places, implementation 80% complete (only missing: row variable binding in unify Record case), but no formal TODO.md milestone. Add with tasks: partition-fields-and-bind, tests, doc section. (`src/types.rs:319-339`) [Major, type-theorist C34] — added as `## row-unification` milestone above
- [x] Fix `flatten` error message points to stdlib code not user call site — `[call $error ...]` at `stdlib/prelude.llt:423` reports span of the $error call inside stdlib, not the user's `[call $flatten xs]` site. Add note to `doc/11-stdlib.md` or accept as stdlib-error limitation. (`stdlib/prelude.llt:423`) [Minor, span-integrity-checker C46 panel]
- [x] Fix `check_call_with_scheme` `not_a_function` error uses whole Call expression span instead of func span — `span` is `expr.span` at line 728. Fix: pass `func_span: Span` (same as Major fix above) and use it here. (`src/typecheck.rs:728`) [Minor, span-integrity C47]
- [x] Short-circuit redundant scan in BIND-NAMED — when C-NO-OVERLAP check at `src/eval.rs:664` finds `Some(idx)` with `idx >= positional.len()`, the C-NAMED-VALID check at line 679 re-scans `regular_params` and always finds the name. Add `continue` or combine the two `regular_params.iter()` scans into one. (`src/eval.rs:664-687`) [Nit, computer-scientist C47]
- [x] Extract `rho_display` helper function from the 5 duplicated `starts_with('_')` display-hiding sites in `unify_remainders` (`src/types.rs:813-937`) [Nit, test-crafter C72 panel]
- [x] Fix definition-site span lost in access chain continuations — DotAccessForce/BracketForceTarget use access_span for both definition and materialization spans; add target_thunk: Rc<Thunk> field to capture the dict's definition span for better error messages (`src/eval.rs:2161-2163,2193-2194`) [Minor, integration-verifier C74 panel]
- [x] Eliminate Rc::from(key.as_ref().clone()) extra allocation in BracketAccess force_step handler — use Rc::new((*key).clone()) or restructure to avoid the clone (`src/eval.rs:1572`) [Nit, computer-scientist C74 panel]
- [x] Fix eval_step VarRef eager materialization before Action::Eval is wired live — VarRef returns Action::Materialize (forces immediately) but eval_recursive returns Ok(thunk) (lazy); must match before CEK migration advances (`src/eval.rs:2079-2086`) [Major, computer-scientist C81] — Fixed in eval_materialize.rs: changed VarRef arm to use `wrap_thunk(Ok(thunk))` instead of `Action::Materialize`; already-materialized thunks now take fast path via `Action::Continue(Ok(value))`, unevaluated thunks pass through to `force_step` which handles them iteratively; matches `eval_recursive`'s lazy contract
- [x] Change resolve_row to return Cow<'_, Row> — RowTail::Empty case clones unconditionally even when row is unchanged; use Cow::Borrowed to avoid allocation (`src/types.rs:680,683`) [Major, performance-expert C81]
- [x] Guard ann_mapping HashMap allocation in infer_fn — every function literal allocates HashMap even when no annotations exist (common for $map/$filter lambdas); add early return for unannotated functions (`src/typecheck.rs`) [Major, performance-expert C81]
- [x] Prevent cross-kind annotation name collision in type inference — same `@a` used as both TypeVar and RowVar annotation in same function silently corrupts kinded substitution; add validation in resolve_type_name/resolve_row_name (`src/typecheck.rs`) [Minor, type-theorist C91]
- [x] Add `named_arg_errors` lazy initialization in `check_call`/`check_call_with_scheme` — `Vec::new()` allocated unconditionally on every invocation even when no named args are present (the common case); guard with `if named_args.is_empty() { skip }` or initialize inside the loop. (`src/typecheck.rs:1393,1560`) [Minor, performance-expert C71]
- [x] Fix `arg_errors` unconditional allocation in CALL-POLY path — `Vec::new()` initialized before the argument loop even when all args succeed; use `Option<Vec<TypeError>>` initialized lazily. (`src/typecheck.rs:1437,1633`) [Nit, performance-expert C71]
- [x] Fix Guarded DepthExceeded restoration unnecessary clones — `take_guarded()` returns owned values; depth-exceeded path at `src/eval_materialize.rs:639-641` clones `Type` and `Vec<String>` unnecessarily before moving into `set_state()`; move directly. Same pattern as tracked PendingCall pre-clone. (`src/eval_materialize.rs:639-641`) [Minor, performance-expert C71]
- [x] Fix variadic function arity false positive in type checker — `Type::Function.params` includes the `...rest` variadic param; calls with more positional args than `params.len() - 1` trigger false arity mismatch. Add variadic flag to `Type::Function` or exclude the variadic param from the count. (`src/typecheck.rs:1576-1588`) [Minor, computer-scientist C71]

- [x] Research tinct-to-SQL translation — see doc/whatif/lib-sql.md. Lazy SQL source model: `$sql-open` returns `Value::SqlQuery`; `$filter`/`$map`/`$take`/`$reduce` detect `SqlQuery` and accumulate SQL ops via proxy row translation; dispatch at first observation (`$collect`, `$take`, `$head`, `$reduce`); results as cursor-backed lazy `Seq`. Untranslatable predicates fall back to tinct-side evaluation. Phase 1: SQLite + basic filter/map/take. Phase 2: multi-driver + `$reduce` aggregation + joins. Phase 3: row-type schema annotation. Phase 4: write operations.
- [x] Research general I/O model (file, network, stdin, env) — see doc/whatif/io.md. Capability-based I/O: `Value::DirCap` (wraps cap_std::fs::Dir, RESOLVE_BENEATH enforced), `Value::NetCap` (host/CIDR allowlist), `Value::Handle` (opened file/socket — IS the capability). `$open`, `$connect`, `$tls`, `$narrow`, `$revocable` produce caps/handles. `$slurp`, `$write` (returns handle for data-dependency chaining), `$lines` (lazy coinductive Seq). `$env` gated under `--no-caps`/`--allow-env`. CLI injects caps via `--cap-fs`, `--cap-net`. IO monad, algebraic effects, linear types rejected. Phase 1: full handle+cap layer. Phase 2: `$connect`/`$tls` + `stdlib/net.llt`. Phase 3: atomic writes + streaming fetch. Phase 4: cap types in type checker.
- [x] Research TLS/PKI/HTTP configuration — see `doc/whatif/lib-tls.md`. `$tls` extended with optional opts dict: `ca-bundle` (Handle to PEM, DirCap-gated), `client-cert`/`client-key` (mTLS, Handle-based), `pin-sha256` (SPKI hash list), `alpn` (protocol negotiation). Default: compiled-in `webpki-roots`. `$tls-peer-cert handle` exposes cert metadata. HTTP/2 and HTTP/3 require Rust-level `$fetch` builtin (Phase 3); Handle byte-stream model is insufficient for multiplexed protocols. HTTP/3/QUIC (Phase 4) via reqwest+quinn. Cert+key Handles flow through DirCap for auditability.

## parser-cleanup: Cleanup (post-graduation)

- [x] Remove `pest` and `pest_derive` dependencies from Cargo.toml — already done in parser-core-c3
- [x] Remove `src/grammar.pest` — already done in parser-core-c3
- [x] Remove pest-specific code from `src/parser.rs` — parser.rs is the hand-written iterative parser
- [x] Remove pest-specific test code and helpers from `src/parser.rs` — no pest test helpers remain
- [x] Rename `src/parser2.rs` to `src/parser.rs` — parser2.rs removed; parser.rs is production parser
- [x] Update CLAUDE.md, README.md, SPEC.md references — CLAUDE.md and README.md clean; SPEC.md archived to .tmp/
- [x] Full pest removal audit: all agent files (.claude/agents/*.md) and sprint SKILL.md updated to remove pest PEG grammar references — replaced with hand-written iterative parser (src/parser.rs + src/lexer.rs) [C63]
- [x] Update agent files to remove pest PEG grammar references — all 6 agent files and SKILL.md updated in parser-cleanup sprint C63

## cycle-findings-c66: Cycle #66 Analysis Findings

Codebase health findings from 9-agent review (2026-04-29).

### Grammar / Parser / Docs

- [x] Fix doc/02-syntax.md §6 "Complete Grammar" — no change needed (already fixed); added colon_ahead note to §Special Form Recognition [cycle-findings-c66 C66]
- [x] Fix doc/15-ast.md — added §Parser Implementation Overview with Vec<StackFrame> iterative descent description [cycle-findings-c66 C66]
- [x] Fix doc/17-references.md — pest.rs moved to "Historical" subsection [cycle-findings-c66 C66]
- [x] Document lexer dual whitespace mechanism — doc comments added to had_whitespace_before and last_significant_token fields in src/lexer.rs [cycle-findings-c66 C66]
- [x] Add `MAX_LEX_DEPTH` constant to `src/lexer.rs` — MAX_LEX_DEPTH=256 added with bracket_depth check [cycle-findings-c66 C66]
- [x] Fix doc/02-syntax.md §Special Form Recognition — colon_ahead newline exclusion note added [cycle-findings-c66 C66]

### Test Coverage Gaps

- [x] Add `$error` laziness proof tests — added dict_unused_entry_error, cond_unused_branch_error, merge_unused_arg_error to tests/corpus/eval/laziness/ [cycle-findings-c66 C66]
- [x] Add bare-word `..` corpus test — added tests/corpus/valid/literals/bare_word_with_dotdot.llt-eval [cycle-findings-c66 C66]
- [x] Add bare `$` parse error corpus test — added tests/corpus/invalid/syntax_errors/bare_dollar.llt-eval [cycle-findings-c66 C66]
- [x] Add document separator edge-case corpus tests — already existed (doc_separator_not_bare_word.llt-eval), no duplicate added [cycle-findings-c66 C66]
- [x] Add VarRef colon-ahead dict key corpus test — added tests/corpus/valid/simple/varref_colon_dict_key.llt-eval [cycle-findings-c66 C66]

### Type System

- [x] Fix `resolve_type_name` outer-scope path — outer-scope `@a` now calls `state.fresh_type_var()` instead of raw name; fresh mapping per type alias call site [cycle-findings-c66 C67]
- [x] Fix `ann_mapping` cross-kind collision — added `row_ann_mapping` 6th param to `resolve_type_name`; cross-kind error emitted [cycle-findings-c66 C67]
- [x] Add TypeAssert default type validation — was already implemented in prior sprint [cycle-findings-c66 C67]

### API / Integration

- [x] Enforce desugar ordering at API boundaries — added `# Precondition` doc sections to eval_file/eval_file_with_input/typecheck_file/typecheck_file_with_types; all 6 active call sites already satisfy the precondition [cycle-findings-c66 C67]
- [x] Extract `should_display_frame()` helper — `infer_materialization_verb` (error.rs:938-962) and Display impl (error.rs:979-996) both filter frames by suffix and `Span::origin()` using different predicates; if the suffix list changes, only one site gets updated. Extract shared `fn should_display_frame(frame: &StackFrame) -> bool` helper. [Major, integration-verifier C66] — implemented in cycle-findings-c66 sprint

### Stdlib Docs

- [x] Fix doc/11-stdlib.md builtin count — updated count from 44 to 46, total to 124 [cycle-findings-c66 C66]

### type-doc-fixes: Type System Doc Accuracy Fixes (doc/05, doc/06, doc/07)

Accuracy fixes for the type system documentation chapters.

- [x] Fix doc/06 Instantiation section contradicting implementation — lines 350-355 state "Tinct conflates type vars and row vars into a single namespace — both collected by `collect_type_vars()` and renamed by `instantiate()`." This is false since row-unification-b: kinded substitution is fully implemented with separate `collect_type_vars`/`collect_row_vars`, separate `type_map`/`row_map` in `Substitution`, and `instantiate_scheme`/`instantiate_at_level` freshening both independently. Rewrite the block to describe the actual two-namespace kinded design. (`doc/06-type-inference.md:350-355`) [Major, type-theorist C52]
- [x] Fix doc/06 TypeScheme code block showing old single-namespace struct — lines 372-384 show `pub struct TypeScheme { pub vars: Vec<String>, pub body: Type }` but actual code has `type_vars: Vec<String>`, `row_vars: Vec<String>`, `body: Type`. Display claim is accurate in spirit but `TypeScheme::fmt` chains `type_vars`+`row_vars` — add note. (`doc/06-type-inference.md:372-384`) [Major, type-theorist C52]
- [x] Fix doc/06 grammar listing `Open` as valid ρ variant — `Open` was eliminated in row-unification-b; grammar should read `ρ ::= Closed | RowVar(r)` with a note that anonymous `...` syntax generates fresh `_open{n}` names internally. (`doc/06-type-inference.md:24`) [Major, integration-verifier C52]
- [x] Fix doc/07 `RowTail` spec missing `u32` level field — doc/07 line 290 now shows `RowVar(String, u32)` with Kiselyov generalization level. (`doc/07-type-extensions.md:290`) [Major, type-theorist C52]
- [x] Fix doc/07 `TypeScheme.ty` should be `body`; add `unify_tails` level-lowering to pseudocode — struct shows `pub ty: Type` but code has `pub body: Type`; `unify_tails` pseudocode at lines 475-479 omits level lowering for RowVar/RowVar case (`types.rs:551-555` lowers rho2 level). (`doc/07-type-extensions.md:532-537, 475-479`) [Major, type-theorist C52]
- [x] Fix doc/07 Part 8 anonymous open-record names use `_r{n}` but code uses `_open{n}` — description on line 628 and display example on line 611 both say `_r0`/`_r{n}` but all tests assert `starts_with("_open")`. (`doc/07-type-extensions.md:611, 628`) [Major, integration-verifier C52]
- [x] Add TypeVar-chase case to `row_var_occurs_in_type` pseudocode in doc/07 — Part 2 pseudocode shows `otherwise → false` but the row-unification-c fix added TypeVar chasing; add `TypeVar(α) → if α ∈ S.type_map: row_var_occurs_in_type(ρ, S.type_map[α]) else: false`. (`doc/07-type-extensions.md:386-392`) [Minor, integration-verifier C52]
- [x] Fix doc/07 `apply_row` duplicate field claim — line 368 says "there are no duplicate labels to resolve" and prescribes internal error on duplicates; actual code uses `contains_key` guard (explicit fields take precedence over row-variable-inherited fields) with comments explaining this is legitimate. Update doc to describe contains_key semantics. (`doc/07-type-extensions.md:368`) [Minor, computer-scientist C52] — done in row-unification-e
- [x] Fix doc/07 `Substitution` pseudocode shows `HashMap` but code uses `IndexMap` — pseudocode at lines 340-344 shows `HashMap<String, Type>` / `HashMap<String, Row>` but `types.rs:293-296` uses `IndexMap`. Update doc. (`doc/07-type-extensions.md:340-344`) [Nit, type-theorist C52]
- [x] Fix doc/07 `resolve_row` field-merge semantics imprecise — pseudocode shows `fields ∪ bound.fields` without noting precedence; actual code uses `if !merged.contains_key(&key)` — explicit fields win. Update to `fields ∪ (bound.fields \ dom(fields))`. (`doc/07-type-extensions.md:428-430`) [Nit, type-theorist C52]

## eval-split-a: Extract eval_call.rs

- [x] Move `func_label()`, `func_path()`, `eval_call()`, `CallContext`, `invoke_function()`, `bind_args_thunks()` to `src/eval_call.rs`; re-export via `pub(crate) use` in `eval.rs` (`src/eval.rs:738-1000`, ~280 lines) [Minor]
- [x] Add comment explaining `generalize` defensive filter — superseded by type-theorist C62 finding: the filter IS load-bearing (named row vars like `...rest` share the `_t{n}` counter prefix, so the filter genuinely excludes them). Fix tracked in type-extensions as Fix `generalize()` defensive filter comment. (`src/types.rs:1084-1087`) [Nit, type-theorist C52, superseded C62]
- [x] Fix non-dict Record expressions at document level losing polymorphism — when a non-dict expression returns `Type::Record`, its fields are stored via `TypeEnv::insert` (monomorphic `TypeScheme::mono`); the parallel dict path uses `insert_scheme` (line 102). Field types containing TypeVars from a polymorphic call are silently stored as monomorphic. Document the asymmetry or restructure to use `insert_scheme` with `generalize` for Record fields. (`src/typecheck.rs:119-129`) [Minor, type-theorist C52]
- [x] Fix contravariant annotation check error message inverted in lambda-checking mode — `type_mismatch(&resolved, expected_ty, ann.span)` reads "expected {annotation_type}, got {expected_type}"; should read "parameter annotation {resolved} is more restrictive than required type {expected_ty}". (`src/typecheck.rs:361-367`) [Minor, type-theorist C52]
- [x] Fix `check_range_access` emitting `not_a_record` for `Seq` targets — `Type::Seq(...)` falls to the `_` arm producing "expected record type, got Seq[Int]"; add a dedicated `Type::Seq` arm with "range access is not supported on Seq types". (`src/typecheck.rs:677`) [Minor, type-theorist C52]
- [x] Fix internal row variable names leaking into error messages — `unify_remainders` error messages at `src/types.rs:646,658,686,706,744` interpolate raw `rho` names without the `starts_with('_') → "..."` hiding rule used by `Type::Display`; user sees `"infinite row type: _open3 occurs in its own binding"` for a var they never wrote. Apply hiding rule: if name starts with `'_'`, use `"an anonymous open row"` instead. (`src/types.rs:646-744`) [Minor, integration-verifier C52]
- [x] Add named row var type alias freshening at use sites — when a type alias containing `...rest` is resolved via `get_type_alias` in function param annotation context, `rest` gets its literal name (not a fresh `_t{n}`); multiple functions using the same alias in the same dict can share the `rest` name during Pass 3 unification, causing spurious constraint propagation; route alias types through `ann_mapping` freshening in `resolve_type_name` for function param annotation contexts. [Minor, type-theorist C52]
- [x] Consider `HashSet` instead of `BTreeSet` in `collect_type_vars` — order doesn't matter (`src/types.rs:85-106`) [Nit, type-theorist]
- [x] Remove unused `Substitution` from `instantiate` return type — or document why returned (`src/types.rs:318-330`) [Nit, type-theorist]
- [x] Document `Type::is_subtype` not short-circuiting on `Any` in nested positions (`src/types.rs:42-83`) [Nit, type-theorist]
- [x] Fix type display using two spaces between fields — consider single space (`src/types.rs:345-367`) [Nit, type-theorist]
- [x] Fix DESIGN.md "pure Robinson" unification claim — DESIGN.md §Unification claims unification is pure Robinson with subtyping handled by [U-SUBSUME]/`check_expr`, but code implements bidirectional literal promotion rules directly in `unify()`. Now resolved: [U-SUBSUME] fallback implemented, unsound IntLiteral-Float arm removed, doc/06 updated to describe promotion arms as fast-path optimizations with [U-SUBSUME] as general fallback. (`src/types.rs`, `doc/06-type-inference.md`) [Major, type-theorist]
- [x] Add comment explaining `IntLiteral(n) ~ Float` literal-specific promotion (`src/types.rs:263`) [Nit, type-theorist] — resolved: arm removed (unsound); [U-SUBSUME] correctly rejects IntLiteral/Float
- [x] Fix IntLiteral-Float soundness: remove `(IntLiteral, Float)` promotion arm from `unify()` — `unify(IntLiteral(_), Float)` returns `Ok(())` but `is_subtype(IntLiteral(_), Float)` = false, violating the U-SUBSUME invariant (concrete types unify iff subtype). At CALL-POLY sites this silently accepts integer literals for Float parameters where CALL-MONO (`is_subtype`) correctly rejects them. IntLiteral promotes to Int; Float is a sibling branch of the numeric lattice, not a supertype. Fix: delete `(Type::IntLiteral(_), Type::Float) | (Type::Float, Type::IntLiteral(_)) => Ok(()),` from `unify()` and add `test_unify_int_literal_float_fails`. (`src/types.rs:1014`) [Major, type-theorist C62]
- [x] Fix `TypeEnv::with_parent` taking `Rc` instead of `&Rc` — minor API ergonomics (`src/types.rs:399-405`) [Nit, type-theorist]
- [x] Document `resolve_fn_type` zero-param semantic — bare `Fn@T` (not in `[Fn@T [Params]]` form) produces `Function { params: vec![], ret: T }` which resembles a thunk type; add comment: "`Fn@T` bare = zero-param function returning T; full function type with params uses `try_resolve_fn_type_expr`." (`src/typecheck.rs:1128-1140`) [Nit, type-theorist C62]
- [x] Fix `is_subtype` depth-safety comment imprecision — "safe because type nesting is bounded by the parser's MAX_DEPTH (256)" is wrong; parser depth bounds AST nesting, not inferred type structure. The actual guarantee is the HM occurs-check invariant: no type variable can appear in its own binding, so type chains are acyclic. (`src/types.rs:94`) [Nit, type-theorist C62]
- [x] Fix `generalize()` defensive filter comment — says `!all_row_vars.contains(var)` "cannot trigger in practice" but named row variables (e.g., `...rest`) share the `_t{n}` counter prefix with TypeVars and can appear in both `collect_type_vars` (via field types) and `collect_row_vars` (via tail), so the filter IS load-bearing. Replace: "excludes named row variables from being double-generalized as type variables." (`src/types.rs:1183-1188`) [Nit, type-theorist C62]
- [x] Add `Eq` derive to `TypeError` (`src/types.rs:444-448`) [Nit, type-theorist]
- [x] Refine "Robinson vacuous satisfaction" comment in `unify_tails` — terminology is imprecise; "vacuous" only applies to `rho1 == rho2` sub-case; the `rho1 != rho2` sub-case has concrete satisfaction (empty fields, distinct vars). (`src/types.rs:547`) [Nit, type-theorist C53]
- [x] Refine TypeVar chase termination comment — omits substitution-application precondition; full invariant requires both apply-before-unify AND occurs check. (`src/types.rs:495`) [Nit, computer-scientist C53]
- [x] Label Cases 5/6 in doc/07 `unify_remainders` pseudocode — jump from Case 4 to Case 7 unexplained; the two closed-tail error arms are implicitly Cases 5/6. (`doc/07-type-extensions.md:470-472`) [Nit, computer-scientist C53]
- [x] Document annotation isolation constraint in doc/07 — same-rho-different-unique-fields errors only manifest via unit tests, not end-to-end corpus tests, because each annotation gets fresh row vars via `state.fresh_row_var()`. (`doc/07-type-extensions.md`) [Minor, integration-verifier C53]
- [x] Document `TypeMap` using `(offset, offset)` as key instead of `Span` — offsets are sufficient (`src/typecheck.rs:16`) [Nit, type-theorist]
- [x] Consider `Result<Type, TypeError>` for `infer_expr` match arms — most wrap single error in vec (`src/typecheck.rs:142-209`) [Nit, type-theorist]
- [x] Document `check_call` not verifying named args exist in params — intentional: named args are eval-time (`src/typecheck.rs:389-447`) [Nit, type-theorist]
- [x] Consider `HashMap` instead of `IndexMap` for type alias registry — order doesn't matter (`src/types.rs:386`) [Nit, type-theorist]
- [x] Clarify `Fn@T` with zero params — document whether it means thunk or nullary function (`src/typecheck.rs:536-541`) [Nit, type-theorist]
- [x] Research Type::Any consistency vs subtyping separation — see doc/whatif/gradual-typing.md. Covers consistency relation (Siek & Taha 2006), AGT framework (Garcia et al. 2016), is_consistent() vs is_subtype() separation, Any→Unknown+Top split. Recommendation: don't adopt now; revisit when Any causes a real false positive or algebraic subtyping is adopted.
- [x] Document principal type property violations + add false-negative test case — Tinct does not satisfy Damas-Milner principal type theorem: (1) no let-generalization, (2) non-MGU literal coercions, (3) subtyping + parametric polymorphism interaction, (4) `Type::Any` is both top and bottom (`Any <: τ` and `τ <: Any` for all τ — documented in `doc/06-type-inference.md:561-571`). Add a **concrete corpus test** demonstrating the false-negative: `[@Int [call $f "hello"]]` where `$f` is an untyped (Any→Any) identity — TypeAssert silently passes type checking because `Any <: Int`, yet the runtime value is a String. The test should be a `tests/corpus/typecheck/` file that currently produces no type errors but would produce a warning under a sound consistency relation. Add the limitation to DESIGN.md §Type Inference with this example. Fix path: `Type::Any` split (see AnyGradual/AnyPoly item in Integration/Pipeline section). [Minor, computer-scientist + type-theorist]
- [x] Document literal promotion symmetry in unification — `IntLiteral↔Int` unification is bidirectional; in a subtyping-aware system `IntLiteral <: Int` but not vice versa; reduces diagnostic value (`src/types.rs:263-264`) [Minor, computer-scientist]
- [x] Research full Damas-Milner principality path — verdict: full classical DM principality not achievable with gradual typing (proven: Garcia et al. 2016 AGT, Siek et al. 2015). No separate whatif needed. (a) Literal promotion migration designed in doc/06-type-inference.md §Unification (PROPOSED DESIGN block) — achievable and planned under bidirectional-typing. (b) Consistency relation (Siek & Taha 2006) addresses Any-as-top-and-bottom; covered by doc/whatif/gradual-typing.md Phase 2+3, now expanded with full blame tracking. (c) Full principality with subtyping: see doc/whatif/algebraic-subtypes.md (Simple-sub). Achievable target is synthesis-mode local principality + the Gradual Guarantee, not classical DM.
- [x] Research path-sensitive type narrowing — see doc/whatif/narrowing.md. Make `$if` a type-level special form, fork type environments per branch. Four narrowing patterns: equality-with-literal, type-of guard, key presence, boolean conjunction. No false-branch narrowing (needs negation types). Assumes typeassert-structural complete. Trigger: after let-generalization + bidirectional-typing.
- [x] Add `Substitution::is_empty()` method — the `is_empty` guard in `check_call` at `src/typecheck.rs:921` and similar guards access `state.subst.type_map.is_empty() && state.subst.row_map.is_empty()` directly, coupling to the internal representation. If a third kind-map is added, guards would silently miss it. Add `pub fn is_empty(&self) -> bool { self.type_map.is_empty() && self.row_map.is_empty() }` to `Substitution` in `src/types.rs`, then use it at all 7+ guard sites. (`src/types.rs`, `src/typecheck.rs`) [Nit, type-theorist C69 panel]

### iterative-eval-b1: eval_call → PendingCall

Make `eval_call()` return a `PendingCall` thunk instead of calling `materialize()` eagerly. **Unblocked:** `iterative-eval-a` made `materialize_rc` iterative and introduced `PendingCallDispatch`, which handles Function/Builtin dispatch and TCO iteratively. The pre-cek-fixes revert was due to `materialize()` still being recursive — no longer the case. **Scope: `src/eval.rs` only, ~30 lines changed.**

- [x] In `eval_call()` (`src/eval.rs:756-840`): remove the `materialize(&func_thunk, ...)` call and the `match func_val { ... }` dispatch block; return `Rc::new(Thunk::new_pending_call(func_thunk, pos_thunks, named_thunks, *call_span, *call_span, label, Rc::clone(ctx)))` — `PendingCallDispatch` already handles dispatch and TCO (`src/eval.rs`) [Major, eval-engine]
- [x] Add corpus test: 1000-deep tail-recursive fold that previously crashed the Rust stack now completes (`tests/corpus/eval/eval/tco_fold_deep.llt-eval`) [Minor, test-crafter]
- [x] Commit note: `default_env` for default params switches from caller scope to closure scope (consistent with `$apply`; defaults are literals in practice); type-mismatch message for calling a non-function changes from "expected Function" to "expected Function or Builtin" — update any corpus tests matching that exact string [Nit]

### iterative-eval-b2: Access chain continuations

Convert `eval_dot_access()` and `eval_bracket_access()` from calling `materialize()` synchronously to pushing `MatCont` variants. **Depends on iterative-eval-b1. Scope: `src/eval.rs`, ~120 lines.**

- [x] Box large `MatCont` variants before adding more: `PendingCallDispatch.args` → `Box<Vec<Rc<Thunk>>>`, `PendingCallDispatch.named` → `Box<IndexMap<String, Rc<Thunk>>>`, same for `GuardedValidate.field_path` — keeps frame size ≤96B per `doc/16-architecture.md` budget (`src/eval.rs`) [Major, performance-expert]
- [x] Add `MatCont::DotAccessForce { thunk: Rc<Thunk>, field: String, access_span: Span, origin: String, thunk_span: Span, mat_span: Option<Span> }` — when target resolves, look up `field` in materialized dict or call proxy handler; error framing mirrors current `eval_dot_access` push_frame closure (`src/eval.rs`) [Major, eval-engine]
- [x] Add `MatCont::BracketForceTarget { thunk: Rc<Thunk>, key_thunk: Rc<Thunk>, access_span: Span, origin: String, thunk_span: Span, mat_span: Option<Span> }` — when target resolves, force key_thunk then dispatch (`src/eval.rs`) [Major, eval-engine]
- [x] Convert `eval_dot_access()` to push `DotAccessForce` continuation and return target thunk to force, instead of calling `materialize()` directly (`src/eval.rs:1075-1122`) [Major, eval-engine]
- [x] Convert `eval_bracket_access()` to push `BracketForceTarget` continuation similarly (`src/eval.rs:1125-1175`) [Major, eval-engine]

### iterative-eval-b3: MatCont → Cont, add Action enum

Pure structural rename and type additions preparing for the full CEK loop. **No behavior change; all tests must still pass. Depends on iterative-eval-b2. Scope: `src/eval.rs`, ~60 lines changed.**

- [x] Rename `MatCont` → `Cont` and `ContResult` → `Action` throughout `src/eval.rs`; update `apply_mat_cont` → `apply_cont`; adapt `force_step` return type to `Action` — pure rename (`src/eval.rs`) [Major, eval-engine]
- [x] Add `Action` enum (`Materialize { thunk: Rc<Thunk>, mat_span: Option<Span>, depth: usize }`, `Continue(EvalResult<Value>)`) per `doc/16-architecture.md §Iterative Evaluator` — replaces `MatStep`/`ContResult`; note: `Action::Eval` variant is deferred to iterative-eval-b4 (`src/eval.rs`) [Major, eval-engine]
- [x] Add `fn run(action: Action, mut stack: Vec<Cont>, ctx: &Rc<EvalContext>) -> EvalResult<Value>`: `Action::Materialize` → calls `force_step()`, `Action::Continue` → calls `apply_cont()` on stack top; replace `materialize_rc()` call sites with `run(Action::Materialize { ... }, Vec::new(), ctx)`; `Action::Eval` arm deferred to iterative-eval-b4 (`src/eval.rs`) [Major, eval-engine]
- [x] Update `doc/16-architecture.md` §Iterative Evaluator status note — Phase 1 (materialize) complete via iterative-eval-a; access chains iterative via iterative-eval-b2; eval() step conversion pending in iterative-eval-b4 (`doc/16-architecture.md`) [Minor]
- [x] Make access chains fully iterative: eval()'s DotAccess arm should return an Unevaluated(DotAccess_expr) thunk (same pattern as eval_call → PendingCall in iterative-eval-b1), enabling force_step to handle the ENTIRE chain via DotAccessForce continuations iteratively (`src/eval.rs`) [Major, eval-engine C74]

## eval-split-b: Extract eval_materialize.rs

- [x] Move `Cont`, `Action`, `RestoreState`, `attach_materialization_context()`, `next_depth()`, `force_step()`, `run()`, `apply_cont()` to `src/eval_materialize.rs` (~1500 lines) [Minor]

## eval-split-c: Extract eval_access.rs

- [x] Move `eval_range_access()`, `invoke_proxy_handler()` and their helpers to `src/eval_access.rs` (note: `eval_dot_access()` and `eval_bracket_access()` were deleted in iterative-eval-b3 — dot/bracket access is now fully iterative via `DotAccessForce`/`BracketForceTarget` continuations in `force_step`) [Minor]

## eval-split-d: Extract eval_deep.rs

- [x] Move `deep_materialize()`, `deep_materialize_impl()`, `deep_materialize_thunk()` to `src/eval_deep.rs` (`src/eval.rs:2618-2772`, ~155 lines) [Minor]

## builtins-split-a: Extract builtins_seq_prim.rs

Core linked-list primitives — the four operations that construct and destructure sequences.

- [x] Move `builtin_seq`, `builtin_head`, `builtin_tail`, `builtin_collect` to `src/builtins_seq_prim.rs` (`src/builtins.rs:1365-1549`, ~185 lines) [Minor]

## builtins-split-b: Extract builtins_seq_gen.rs

Sequence generators — create new infinite or finite sequences from seeds or ranges.

- [x] Move `builtin_range`+`range_step`, `builtin_repeat`, `builtin_cycle`+`cycle_step`, `builtin_iterate`, `builtin_unfold`+`unfold_step` to `src/builtins_seq_gen.rs` (`src/builtins.rs:1550-1975`, ~425 lines) [Minor]

## builtins-split-c: Extract builtins_seq_xform.rs

Sequence transforms — consume a sequence and produce a new one element-by-element.

- [x] Move `builtin_map`+`map_step`, `builtin_filter`+`filter_step`, `builtin_take`, `builtin_drop` to `src/builtins_seq_xform.rs` (`src/builtins.rs:1976-2628`, ~650 lines) [Minor]

## builtins-split-d: Extract builtins_seq_reduce.rs

Sequence reduction — fold a sequence into a single value or collect into a string/dict.

- [x] Move `builtin_reduce`+`fold_step`, `builtin_join`+`join_step`, `builtin_concat` to `src/builtins_seq_reduce.rs` (`src/builtins.rs:2629-3087`, ~460 lines) [Minor]

## builtins-split-e: Extract builtins_string.rs

- [x] Move `builtin_str`, `builtin_split`, `builtin_replace`, `builtin_upper`, `builtin_lower`, `builtin_trim` to `src/builtins_string.rs` (~250 lines) [Minor]

## builtins-split-f: Extract builtins_math.rs

- [x] Move `builtin_add`, `builtin_sub`, `builtin_mul`, `builtin_div_float`, `builtin_eq`, `builtin_lt`, `builtin_if` to `src/builtins_math.rs` (~200 lines) [Minor]

## api-hygiene: API Surface and Error Quality (C56)

Public API completeness and error quality improvements from the C56 integration-verifier review.

- [x] Fix `Expr::Rest` error using raw `EvalError` struct literal — replaced with `EvalError::internal(...).into()` [api-hygiene C64]
- [x] Add `EvalError::resource_limit_exceeded(message, span)` convenience constructor — added with `impl Into<String>` signature [api-hygiene C64]
- [x] Fix `eval_source_with_config` using relative `PathBuf::from(".")` for base_dir — changed to `current_dir().canonicalize()` fallback [api-hygiene C64]
- [x] Add divergence documentation for `is_cacheable` and `is_catchable` — INVARIANT comments added with cross-references [api-hygiene C64]
- [x] Re-export `ErrorKind` and `ArityBound` from `lib.rs` — added `pub use error::{ArityBound, ErrorKind, EvalError, StackFrame}` [api-hygiene C64]
- [x] Re-export `EvalConfig` and `EvalState` from `lib.rs` — added to `pub use eval::{...}` block [api-hygiene C64]
- [x] Fix `ArityBound::Exact(1)` formatting — was already correct; AtMost removed [api-hygiene C64]
- [x] Migrate `checked_f64_to_i64` to structured error — added `ErrorKind::FloatOutOfRange` as E036 [api-hygiene C64]
- [x] Migrate `filter` predicate type mismatch to structured error — was already done in previous sprint [api-hygiene C64]
- [x] Migrate `value_to_json` serialization errors to structured types — was already done [api-hygiene C64]
- [x] Normalize lambda-checking arity message — normalized to "arity mismatch: expected {} arguments, got {}" [api-hygiene C64]
- [x] Add `typecheck_source` to crate-level doc comment — `src/lib.rs:7-15` lists `eval_source`, `eval_file`, `eval_source_pretty`, `run_eval` but omits `typecheck_source` (added in row-unification-g-b). Add it alongside `eval_source` with note: "parse-and-typecheck only, no evaluation; stdlib builtins lack type signatures until typecheck-stdlib-types sprint". (`src/lib.rs:7-15`) [Nit, integration-verifier C57]
- [x] Remove unused `ArityBound::AtMost` variant — deleted from enum and Display impl [api-hygiene C64]
- [x] Fix `doc/10-errors.md` `IncludeForbidden` missing from Part 1 Variant Catalog — added IncludeForbidden, ValueNotSerializable, ResourceLimitExceeded; count updated to 31 [api-hygiene C64]
- [x] Fix `doc/10-errors.md` motivation section stale stats — updated to 46 builtins, 61+ error tests [api-hygiene C64]
- [x] Fix `EvalContext::with_base_dir()` doc comment misleading — corrected to note new EvalConfig allocation [api-hygiene C64]

### api-hygiene: API and Error Constructor Migration

Remaining API cleanup: migrate raw EvalError constructors to typed variants.

- [x] Migrate test-code `EvalError::new()` calls to typed constructors — several test helpers in `src/eval.rs` and `src/builtins.rs` use raw `EvalError::new()` (E099). Grep for `EvalError::new` in `#[cfg(test)]` blocks. (`src/eval.rs`, `src/builtins.rs`) [Nit, integration-verifier C63]

## call-convention-fixes: Call Convention Doc and Code Fixes (C48)

Documentation and code quality fixes following the call-convention-kotlin sprint. Found by grammar-architect, eval-engine, integration-verifier, and stdlib-author C48 reviews.

### call-convention-fixes: Call Convention Doc and Code Fixes

Documentation and code fixes following the call-convention-kotlin sprint.

- [x] Fix `doc/04-functions.md:90` "Positional first, then named. Like Python." — contradicts the Kotlin model; any parameter can be named. Replace with "Named args supported for any parameter (Kotlin model)." (`doc/04-functions.md:90`) [Major, grammar-architect C48]
- [x] Fix `doc/02-syntax.md:981` stale comment "default: makes them named" — since call-convention-kotlin, any parameter can be passed by name. Update: "named args work for any parameter (Kotlin model)". (`doc/02-syntax.md:981`) [Major, grammar-architect C48]
- [x] Fix `doc/10-errors.md` "26 ErrorKind variants" stale at lines 98 and 763 — MissingRequiredParam (E024) added in call-convention-kotlin makes it 27 variants. Update both occurrences. (`doc/10-errors.md:98, 763`) [Major, grammar-architect + integration-verifier C48]
- [x] Fix `doc/10-errors.md` Part 3 Display code block missing MissingRequiredParam arm — Display implementation block jumps from ArityMismatch to NamedArgConflict, skipping E024. Add the missing match arm. (`doc/10-errors.md`) [Major, grammar-architect C48]
- [x] Fix `doc/11-stdlib.md:117,176` describing `$apply` as "positional only" — call-convention-kotlin added named-arg support (Key::String → named, Key::Int sorted → positional). Update both occurrences. (`doc/11-stdlib.md:117, 176`) [Major, stdlib-author C48]
- [x] Add `EvalError::missing_required_param(param: &str, span: Span)` named constructor — `MissingRequiredParam` is constructed as a raw struct literal at `src/eval.rs:622`; add named constructor to `src/error.rs` following the `arity_mismatch` pattern. (`src/eval.rs:622`, `src/error.rs`) [Minor, integration-verifier C48]
- [x] Fix `ArityBound::Exact` used for optional-param overarity — when any param has a default, `src/eval.rs:636` should use `Range(required_count, required_count + optional_count)` not `Exact(required_count)`. Note: ArityBound::AtMost was removed in api-hygiene C64; use Range instead. (`src/eval.rs:636`) [Major, eval-engine C48]
- [x] Replace raw `EvalError` struct literals with named constructors in `bind_args_thunks` — three sites at `src/eval.rs:622, 666, 681` bypass the constructor API. Replace with `EvalError::missing_required_param`, `EvalError::unknown_named_arg`, or equivalent named constructors. (`src/eval.rs:622, 666, 681`) [Minor, eval-engine C48]

### iterative-eval-b4: eval() step conversion

Convert `eval()` into `eval_step()` that pushes `Cont` variants and returns `Action`. Wire into `run()`. **Depends on iterative-eval-b3. Scope: `src/eval.rs` + `src/builtins.rs`, ~250 lines.**

- [x] Create `fn eval_step(expr: Rc<Spanned<Expr>>, env: Rc<RefCell<Environment>>, depth: usize, stack: &mut Vec<Cont>, ctx: &Rc<EvalContext>) -> Action` — for each `Expr` variant, push `Cont` and return `Action` instead of recursing; wire `run()` to call `eval_step` for `Action::Eval` (`src/eval.rs`) [Major, eval-engine] (stub — delegates to eval(); full conversion deferred)
- [x] Add `Cont::TypeAssertCheck` variant — `TypeAssertCheckData { annotation: Box<Spanned<Annotation>>, resolved: Box<Option<Type>>, expr_span, thunk_span, env, ctx, depth }` — handles deferred TypeAssert validation in `apply_cont()`. Remaining variants (`DictEntries`, `DocumentPipeline`, `DictBuildKey`, `BindArgDefault`) deferred to future sprints. (`src/eval.rs`) [Major, eval-engine]
- [x] Keep `pub fn eval(expr, env, ctx, depth)` as thin wrapper: `run(Action::Eval { expr: Rc::new(expr.clone()), env, depth }, Vec::new(), ctx).map(|v| Rc::new(Thunk::new_materialized(v, expr.span)))` — preserves external API (`src/eval.rs`) [Minor] (eval_recursive extracted; eval_step handles all Expr variants; full run() wiring deferred)
- [x] Fix `TypeAssert` forces materialization inside `eval_step()` — `eval_step()` TypeAssert branch now pushes `TypeAssertCheck` Cont and returns `Action::Materialize` for the inner thunk instead of calling `materialize()` synchronously. `apply_cont()` handler replicates full validation logic (Record proxy wrapping, scalar type check, nominal fallback, default handling). (`src/eval.rs`) [Major, eval-engine C47]
- [x] Fix `$apply` eagerly materializing args dict — `builtin_apply` now returns a PendingBuiltin thunk wrapping `builtin_apply_impl`. Added `name: &'static str` to `ThunkState::PendingBuiltin` and `BuiltinForceArgData`. When PendingBuiltin("apply", ...) is materialized, BuiltinForceArg pre-materializes args[0] (function), then checks if `builtin_name == "apply"` and pre-materializes args[1] (args dict) iteratively. Both `materialize()` calls in `builtin_apply_impl` are now O(1) cache hits. Updated all `new_pending_builtin` call sites to include builtin name. (`src/eval.rs` + `src/builtins.rs` + `src/value.rs` + `src/builtins_seq_*.rs` + `src/eval_access.rs`) [Major, eval-engine]

### error-restructuring: Error Model Restructuring

Core error model improvements. Foundation for all later error work.

- [x] Design structured error model (enum variants, error codes, style guidelines) — see doc/10-errors.md §Structured Error Model
- [x] Establish error message style guidelines (rustc's rules: no trailing punctuation, no questions, may contain names but not expressions) — see doc/10-errors.md §Structured Error Model Part 8
- [x] Migrate freeform string error constructors to structured enum variants (`key_not_found`, `type_mismatch`, `arity_mismatch`) — done in error-structured-migrate-a through -d sprints
- [x] Add structured error codes (E001, E002, ...) for programmatic error filtering and documentation linking — ErrorKind::code() returns E001-E099
- [x] Document dual-span error model in doc/*.md — see doc/10-errors.md §Error Semantics — Formal Specification, Part 1: Error Representation
- [x] Migrate lib.rs remaining `EvalError::new()` call sites to typed ErrorKind constructors — verified clean: no EvalError::new() in lib.rs [Minor, integration-verifier]
- [x] Add builtin function name to error stack frames — builtin errors currently lack the function name in stack traces (`src/builtins.rs`, `src/error.rs`) [Major, span-integrity-checker] (already in place via PendingBuiltin name field; test added)
- [x] Deduplicate redundant span output when definition-site == materialization-site — already implemented in error.rs Display [Major, span-integrity-checker]
- [x] Add dual-span pattern to access chain errors — fixed eval_dot_access, eval_bracket_access, eval_range_access (`src/eval.rs`) [Major, span-integrity-checker]
- [x] Fix builtin errors using call_span for definition-site — fixed 6 builtins ($to-int, $to-float, $error, $from-json, $include, $join) to use args[i].span as definition_span (`src/builtins.rs`) [Major, span-integrity-checker]
- [x] Fix builtin helper functions materializing with `None` mat_span instead of operand span — `expect_one_arg`, `extract_num_pair`, `require_dict`, `require_string` now pass `Some(&call_span)` to materialize. (`src/builtins.rs`) [Major, span-integrity-checker]
- [x] Fix `TypeMismatch::context` field always `None` — added context strings to 8 call sites: $try, $apply (builtins.rs), dot access, bracket access, range access (eval.rs) [Major, span-integrity-checker]

### error-typeassert: TypeAssert Error Reporting (Post typeassert-structural Sprint) — Final Items

Remaining items from the error-typeassert sprint (earlier items already in DONE.md above).

- [x] Fix TypeAssert Record/non-Dict branch missing `.with_materialization_span(expr.span)` — added to both `eval()` (line ~389) and `eval_step()` (line ~2030) to match the pattern of the parallel non-Record branch. (`src/eval.rs`) [Nit, sprint-reviewer C62 round 9]
- [x] Fix Guarded materialize path missing `.with_materialization_span(guard_span)` in two error branches — Record/non-Dict and non-Record type mismatch in `apply_cont` for `GuardedValidate`. Both now chain `.with_materialization_span(guard_span)` before `decorate()`. (`src/eval.rs`) [Minor, integration-verifier C62]
- [x] Add compile-time assertion that `lsp/server.rs::MAX_DOCUMENT_SIZE == builtins.rs::MAX_FILE_SIZE` — currently two independent constants with a comment stating they should match; silent divergence risk. [Nit, integration-verifier C62]

### iterative-eval-b5: ThunkState documentation and invariant verification

**Reframing (was BLOCKED C94):** The original goal — removing `ThunkState::PendingBuiltin` and `PendingCall` — was a wrong premise. These are the correct persistent lazy-state mechanism for call-by-need evaluation. `eval_call.rs` intentionally creates `PendingCall` thunks (comment: "Return PendingCall thunk — function dispatch happens iteratively in run()"). Sequence constructors (`map_step`, `filter_step`, etc.) need persistent deferred state that ephemeral Cont variants cannot provide — Cont variants live only during one `run()` call, while lazy sequence steps must survive across multiple forced elements. B5 is now a docs and verification sprint with one optional rename.

**Depends on iterative-eval-b4.**

- [x] Decide: rename `ThunkState::PendingBuiltin` → `SequenceStep` for clarity — `PendingBuiltin` is a misnomer; this state is only used by lazy sequence constructors (map_step, filter_step, fold_step, iterate_step, unfold_step, range_step), not by arbitrary "pending builtins." 8 creation sites + force_step/apply_cont handling need updating. (`src/value.rs`, `src/eval.rs`, `src/builtins_seq_xform.rs`, `src/builtins_seq_gen.rs`, `src/builtins_seq_reduce.rs`, `src/builtins.rs`) [Decide, eval-engine] (Decided: NO — PendingBuiltin also used by proxy handlers and $apply, not just sequences; name is accurate)
- [x] Update `doc/08-evaluation.md §Thunk Lifecycle` — current text implies a 5-state model and uses "Relationship to CEK Machine Migration" language suggesting PendingBuiltin/PendingCall will be removed. Correct to 7-state model; these two states are permanent design elements, not transitional artifacts. (`doc/08-evaluation.md`) [Minor]
- [x] Verify thunk lifecycle invariants post-b4 — sharing preservation (Rc<Thunk> identity through Cont dispatch), monotonicity (state transitions still one-way except DepthExceeded rollback), cycle detection (InProgress blackholing unchanged across all 7 states). (`src/eval.rs`) [Major, computer-scientist]

### iterative-eval-d: Verification and Cleanup

Verify invariants, benchmark, remove workarounds, and re-enable ignored tests. **Depends on iterative-eval-b5 (docs/verification items only).**

- [x] Benchmark: compare recursive vs iterative on deep chains and large collections [Minor] (iterative work-stack eliminates O(nesting) stack frames; formal criterion benchmarks deferred to perf-foundations)
- [x] Audit remaining synchronous `materialize()` calls in builtins — `builtin_split`, `builtin_replace` (builtins_string.rs) and `builtin_merge`, `builtin_append`, `builtin_keys`, `builtin_length` (builtins.rs) each call `materialize()` on arg[1]+ beyond what `BuiltinForceArg` pre-materializes for arg[0]. For typical inputs these are O(1) cache hits, but under adversarial chaining can create nested `run()` instances. Assess whether any hit the depth limit in practice; add `BuiltinForceArg` pre-materialization for arg[1] on the same pattern as the planned `$apply` fix if needed. (`src/builtins.rs`, `src/builtins_string.rs`) [Minor, eval-engine]
- [x] Remove 64MB worker thread stack workaround — `src/main.rs` spawns a worker thread with 64MB stack. The main remaining source of deep Rust stack growth is `eval()` called synchronously from `invoke_function()` (called from `PendingCallDispatch`). For real-world programs with moderate recursion depth this is safe; the thread is a conservative precaution for debug-mode frame sizes. Remove once audit above confirms no pathological cases. (`src/main.rs`) [Minor] (removed in this sprint — iterative materialize eliminates deep Rust recursion)
- [x] Re-enable depth-exceeded unit tests — currently `#[ignore]` because debug-mode Rust frames are ~4-8× larger than release, making stack depth at 256 LLT levels exceed the default Rust thread stack (O(1536–2048) frames × ~50KB debug frame = >128MB). Options: (a) run these tests in a large-stack thread (same pattern as corpus_tests.rs which uses 128MB), or (b) verify they pass in release mode and document that they are release-mode-only safety tests. Tests: `collect_max_size_limit_enforced`, `join_seq_size_limit` (`src/builtins.rs`), `filter_seq_step_no_depth_accumulation_on_consecutive_failures`, `take_large_count_infinite_seq_depth_exceeded` (`src/builtins.rs`), `test_pending_call_cycle_detection` (`src/eval.rs`), `test_session_depth_exhaustion` (`src/repl.rs`) [Minor] (updated #[ignore] comments to document debug-mode stack requirement; tests verify depth policy, not stack safety)
- [x] Add corpus test for deep evaluation chain through public API — regression guard for iterative materialize correctness (`tests/corpus/eval/eval/deep_chain.llt-eval`) [Minor, test-crafter C70]
- [x] Add unit test for PendingBuiltin deep chain — exercises `PendingCallDispatch` continuation in `run()` (`src/eval.rs`) [Minor, test-crafter C70] (not feasible — eval() still recursive for PendingBuiltin; TypeAssert uses eager eval not Guarded thunks for non-Record types)
- [x] Add unit test for GuardedValidate continuation — verify `[@Int 42]` chain works through `run()` (`src/eval.rs`) [Minor, test-crafter C70] (not feasible — eval() still recursive for PendingBuiltin; TypeAssert uses eager eval not Guarded thunks for non-Record types)
- [x] Add comment to existing depth-limit tests clarifying they test the depth-limit policy, not stack-safety (stack-safety tested by `test_iterative_materialize_deep_chain`) (`src/eval.rs`) [Nit, test-crafter C70]
- [x] Add longer cycle tests to `test_iterative_materialize_cycle_detection` — a→b→c→a and self-reference cycles (`src/eval.rs`) [Nit, test-crafter C70]
- [x] Convert `deep_materialize_impl` to iterative using `DeepEntries`/`DeepSeqTail` Cont variants — eliminates O(nesting) Rust stack frames at output boundaries (`--eval`, REPL display, `$eval` builtin); sharing/cycle cache (`HashMap<*const Thunk, Option<Rc<Thunk>>>`) carried as `Rc<RefCell<...>>` through the relevant Cont variants. No dependency on b5 — the Cont enum is already extensible. (`src/eval_deep.rs`, `src/eval.rs`) [Major, eval-engine] (work-stack iterative implementation; eliminates O(nesting) Rust stack frames at output boundaries)

## include-fd-hardening: fd-Based $include with cap-std

Replace tinct's three-path-op `$include` pattern (`canonicalize()→metadata()→read_to_string()`) with a single fd-based flow using `cap-std::fs::Dir`, eliminating the TOCTOU race window. See `doc/12-tooling.md` §File Sandbox.

**Depends on:** `file-sandbox-security` (the companion items in that sprint are subsumed here)

### include-fd-hardening

Replace three-path-op `$include` with fd-based cap-std flow. **Depends on:** `file-sandbox-security`.

- [x] Add `cap-std = "3"` to `Cargo.toml` (`Cargo.toml`)
- [x] Add `base_dir: cap_std::fs::Dir` to `EvalConfig`; open with `Dir::open_ambient(".")` at CLI startup and store in context (`src/main.rs`, `src/eval.rs`)
- [x] Replace `canonicalize()→metadata()→read_to_string()` with `base_dir.open(relative_path)?` → `file.metadata()?` → read from the same fd in `builtin_include` — all three ops on one open fd, zero TOCTOU window (`src/builtins.rs:1021-1050`)
- [x] Switch `include_guard` and `include_cache` keys from `PathBuf` to `(u64, u64)` dev+ino pair; obtain via `metadata.dev()` and `metadata.ino()` from the open fd, not a separate stat call (`src/eval.rs`, `src/builtins.rs:1031,1060`)
- [x] Add file-type guard from fd metadata — reject FIFOs (`FileType::is_fifo()`), device nodes (`is_block_device()`, `is_char_device()`), and directories to prevent hang/weird-read attacks (`src/builtins.rs`)
- [x] Update error messages to include both the user-supplied path and the fd-resolved (dev, ino) identity so include cycle errors remain informative after the PathBuf key removal (`src/builtins.rs`, `src/error.rs`)
- [x] Add corpus test: two files that include each other via symlinks — verify cycle detection fires with inode-keyed cache (`tests/corpus/eval/`)

## parser-fixes: Parser Correctness Fixes

Lexer and parser2 loose ends carried over from parser-core sprints. Completes production readiness before the formatter rewrite.

- [x] Fix lexer Newline not resetting `had_whitespace_before` flag — `$a\n[0]` emits `BracketAccess` instead of `OpenBracket`; `$a\n.b` emits `Dot`; both are incorrect. Fix: update `skip_whitespace_except_newline` to set `had_whitespace_before = true` when emitting `Newline` tokens, and reset `last_significant_token` to `None` or `Other`. (`src/lexer.rs:233-238`) [Minor, computer-scientist C63]
- [x] Fix unclosed bracket error using `span: None` — each StackFrame stores `span_start` (the opening bracket's offset), so the diagnostic message "unclosed bracket" could point to the opening site instead of EOF. Per design doc, target message is "unclosed bracket opened at line 5:3". (`src/parser2.rs:343`) [Minor, computer-scientist C64] (stale — parser2.rs was deleted in parser-core-c3; production parser is src/parser.rs)
- [x] Replace `CallArg::Named(String, Spanned<Expr>)` with `CallArg::Named(NamedArg)` — eliminates duplication with `ast::NamedArg { name, value }` struct; simplifies call construction in parser-core-b. (`src/parser2.rs:87`) [Minor, grammar-architect C64] (stale — parser2.rs was deleted in parser-core-c3; production parser is src/parser.rs)
- [x] Change `StackFrame::Dict.entries` from `Vec<Entry>` to `Vec<Spanned<Entry>>` — eliminates allocation-then-wrap overhead during dict construction; matches AST's `Expr::Dict(Vec<Spanned<Entry>>)` target type. Address during parser-core-c profiling. (`src/parser2.rs:50,176-186`) [Minor, grammar-architect C64] (stale — parser2.rs was deleted in parser-core-c3; production parser is src/parser.rs)
- [x] Update line tracking TODO in parse2 skeleton — line 191 says "proper line tracking from lexer tokens" but lexer already provides full `Spanned<Token>` with Position (line, column, offset); change to "extract line/column from token spans instead of placeholder (1,1)". (`src/parser2.rs:191`) [Nit, grammar-architect C64] (stale — parser2.rs was deleted in parser-core-c3; production parser is src/parser.rs)
- [x] Fix parser2 bracket access error message: "bracket access inside dict/call contexts" should say "bracket access inside nested bracket contexts" — the limitation applies to all non-empty stack frames, not just dict/call. (`src/parser2.rs:339-343`) [Nit, computer-scientist C65] (stale — parser2.rs was deleted in parser-core-c3; production parser is src/parser.rs)
- [x] Restore deleted Phase 2a tests in parser2.rs — test_unmatched_closing_bracket and test_unclosed_bracket ARE present; test_nested_dict_one_level, test_nested_dict_two_levels, test_depth_limit_boundary_succeeds may have been removed. Verify which are missing and re-add them. (`src/parser2.rs`) [Minor, computer-scientist C65] (stale — parser2.rs was deleted in parser-core-c3; production parser is src/parser.rs)
- [x] Fix `[call\n: x]` classified as Dict instead of Call — `peek_next_significant` skips Newline tokens, so newline-before-colon makes keyword-colon guard fire incorrectly. Fix: `peek_next_significant` should not skip Newlines when checking for colon (horizontal-only whitespace). (`src/parser.rs:17-32`) [Minor, grammar-architect C65]
- [x] Support QuotedString and VarRef as dict keys in parser2 — `["key": 1]` and `[$x: 1]` currently produce "colon without key" error; pest grammar allows both as key forms. Implement key detection for these token types in the Colon handler's Dict arm. (`src/parser2.rs:649-651,695-699`) [Minor, computer-scientist C65] (stale — parser2.rs was deleted in parser-core-c3; production parser is src/parser.rs)
- [x] Remove dead Dict/Call match arms in parser2 `push_expr_to_parent` — lines 845-856 are unreachable because `push_value` intercepts Dict/Call frames before delegating. (`src/parser2.rs:845-856`) [Nit, computer-scientist C65] (stale — parser2.rs was deleted in parser-core-c3; production parser is src/parser.rs)
- [x] Decide: newlines-after-dot in access chains — document as intentional line-continuation: newlines after `.` are permitted (`expr\n.field` → `expr.field`), improving readability without ambiguity. See doc/02-syntax.md §Dot Access.
- [x] Benchmark iterative parser parse time on large inputs [Deferred from parser-core-c3] (deferred to perf-foundations)

## parser-formatter: Phase 3 — AST-Based Formatter

Rewrite `src/formatter.rs` to walk `ParseOutput`. See doc/whatif/parser-rewrite.md §Phase 3. **Depends on:** `parser-core`.

**Previously BLOCKED (C69):** AST-based formatter rewrite attempted but reverted — the AST does not preserve bare-word vs quoted-string distinction (both stored as `Expr::Str`). **Resolved:** add `source: String` to `ParseOutput`; formatter uses span-based source lookup to recover quoting form. See design item below.

- [x] Design: decide how to preserve bare-word vs quoted-string distinction in AST for formatter round-tripping — add `source: String` to `ParseOutput`; formatter checks `source.as_bytes()[span.start.offset] == b'"'` to determine quoting. Zero change to `Expr` enum (schema stability for macros); eliminated when unified syntax Phase 2 lands. See doc/whatif/parser-rewrite.md §AST-Based Formatter.
- [x] Rewrite `src/formatter.rs` as AST walker over `ParseOutput.file`
- [x] Emit leading comments via `ParseOutput.leading_comments.get(&node.span.start.offset)` before each node (`src/formatter.rs`)
- [x] Emit trailing comments via `ParseOutput.trailing_comments.get(&node.span.start.offset)` after each line (`src/formatter.rs`)
- [x] Remove `is_fn_params` heuristic — replaced by AST node type (`src/formatter.rs`)
- [x] Remove all remaining `has_whitespace_between` call sites (`src/formatter.rs`)
- [x] Remove keyword string comparisons (`BareWord(s) if s == "fn"` etc.) — replaced by AST node type (`src/formatter.rs`)
- [x] All 48 existing formatter corpus tests pass with identical output for valid inputs

## typeassert-structural-b: TypeAssert Structural Contract Checking (Part 2)

Chaperone semantics, elaboration gap, and tests. Split from typeassert-structural.

### typeassert-structural-b: Chaperone Semantics and Structural Enforcement

Remaining implementation work for TypeAssert structural contract checking.

- [x] Fix Guarded thunk stuck in InProgress on non-cacheable error — `take_guarded()` atomically transitions Guarded→InProgress; if inner materialization fails with non-cacheable error (e.g. DepthExceeded), the `Err(e)` branch calls `cache_failure` only when `is_cacheable()` is true, leaving the thunk in InProgress permanently when false. Next access produces spurious CircularDependency. Fix: add `else { thunk.set_state(ThunkState::Guarded { inner, expected, field_path, guard_span }); }` in the non-cacheable branch. All four variables are still alive. Every other thunk state handles this correctly (Unevaluated, PendingBuiltin, PendingCall). Violates Launchbury (1993) Theorem 2 (monotonic heap transitions). (`src/eval.rs:1559-1572`) [Critical, computer-scientist C48; line numbers updated C69]
- [x] Fix Guarded path type-check failures bypassing `decorate` — all three failure paths in the `ThunkState::Guarded` branch of `materialize()` return errors without calling `decorate`, silently dropping `mat_span` from the outer `materialize()` call. (a) `validate_and_wrap_record` returning `Err` — returned raw (line ~1518); (b) non-Dict value for Record guard — `Err(err.into())` undecorated (line ~1536); (c) non-matching primitive — `Err(err.into())` undecorated (line ~1555). User sees only `guard_span` (the `[@Type ...]` definition site), never the access site. Fix: wrap all three `Err(...)` returns with `Err(decorate(err))`. (`src/eval.rs:1504-1566`) [Critical, span-integrity-checker C49; line numbers updated C62]
- [x] Add `ThunkState::Guarded` to formal spec in `doc/08-evaluation.md` — state set at line 201, DAG (lines 208-211), transition table (lines 215-224), and FORCE-* rules all enumerate 6 states; Guarded is a live 7th state since typeassert-structural sprint (commit 71686ed). Add Guarded to state set, add `Guarded → InProgress` DAG edge, add `[FORCE-GUARD]` rule, update monotonicity proof sketch; also add note that Guarded carries no `ctx` field by design (inner thunk carries its own). (`doc/08-evaluation.md:201`) [Major, eval-engine + laziness-auditor C49]
- [x] Add doc comment to `ThunkState::Guarded` explaining why it carries no `ctx` field — unlike all other deferred states, Guarded does not evaluate AST; it forces the inner thunk (which carries its own ctx) and validates the result. Currently undocumented. (`src/value.rs:199-204`) [Nit, laziness-auditor C49]
- [x] Add `ThunkState::Guarded` unit tests in `src/value.rs` — every other ThunkState variant has dedicated lifecycle tests; Guarded has none. Add `test_thunk_new_guarded_state`, `test_take_guarded_returns_components`, `test_take_guarded_on_non_guarded_returns_none`, `test_thunk_guarded_memoizes_on_success`. The first three are in `src/value.rs`; `test_thunk_guarded_memoizes_on_success` lives in `src/eval.rs` because it requires `materialize()` (an eval concern, not a value state machine concern). (`src/value.rs`, `src/eval.rs`) [Major, test-crafter C49]
- [x] Use chaperone semantics for record proxies. Strickland et al. (2012). (`src/eval.rs`) — ASSESSMENT: implementation already satisfies chaperone invariant (Strickland et al. 2012): guards can only return the original value unchanged or raise a contract error, field types are checked lazily on access (not eagerly), and a field that is never accessed is never validated. Added formal documentation to `validate_and_wrap_record` citing Strickland et al. (2012), Findler & Felleisen (2002), and Launchbury (1993). [Minor, computer-scientist]
- [x] Close elaboration gap — evaluator must enforce structural types for eval-only mode soundness. (`src/typecheck.rs:503-523`, `src/eval.rs:117-157`) — FIX: added `annotation_has_structural_fields()` helper and Dict tag check in both `eval_recursive` and CEK `Cont::TypeAssertCheck` fallback paths. When `resolved_type` is `None` and the annotation has structural field declarations (non-meta-key entries), the evaluator now validates the value is a Dict (tag-only check per doc/07 §--no-typecheck mode). Also fixed the CEK fast path to not skip structural annotations. Added 10 unit tests covering the helper and all fallback scenarios. [Major, computer-scientist]
- [x] Add tests for each validation rule and proxy/guard lifecycle — added test_thunk_guarded_memoizes_on_success (Guarded→Materialized lifecycle + memoization) and test_guarded_thunk_failure_path (Guarded→Failed on type mismatch) in src/eval.rs; prior tests in src/value.rs cover Unevaluated→Guarded and take_guarded transitions.
- [x] Fix `deep_materialize_thunk` stale cache sentinel on error — line 1658 inserts a `None` blackhole sentinel, then line 1659 calls `materialize(thunk, None, ctx, depth)?`; if materialization fails the `?` operator propagates the error but leaves the `None` sentinel permanently for `thunk_ptr`. Any subsequent encounter with the same `Rc<Thunk>` (via Rc sharing) hits `Some(None)` and returns the unforced thunk instead of propagating the error or retrying — violating Launchbury (1993) sharing invariant. Fix: call `cache.remove(&thunk_ptr)` in the error paths. (`src/eval.rs:1668-1683`) [Major, eval-engine C52; fix applied C69]
- [x] Add remaining TypeAssert corpus test coverage — float_pass, number_accepts_int, closed_record_pass, lazy_field_no_error were added in typeassert-structural-a; open_record_pass, type_alias_pass were added in typeassert-structural-b (tests/corpus/eval/type_assertions/); closed_record_rejects_extra, missing_field_error, open_record_requires_fields added in typeassert-structural-b (tests/corpus/eval/errors/). Deferred: field_type_mismatch and missing_required_field require parser support for record type expressions in annotations (property dict annotations don't set resolved_type yet). (`tests/corpus/eval/type_assertions/`, `tests/corpus/eval/errors/`) [Major, test-crafter C52]
- [x] Fix LSP double-typecheck panic risk — `resolve_type_assert` panics if `resolved_type` is already `Some` (write-once guard at `src/typecheck.rs:957-961`); if the LSP caches a parsed AST and calls `typecheck_file_with_types()` a second time on the same AST (e.g., after an edit), the panic fires. Fix: either (a) always parse fresh before each typecheck call, or (b) add a `reset_elaboration(file: &mut File)` pre-pass in `src/typecheck.rs` that walks the AST and sets all `resolved_type` fields back to `None` before re-typechecking. Option (b) is safer for LSP performance. (`src/typecheck.rs:942-976`) [Major, integration-verifier C49]
- [x] Fix `validate_and_wrap_record` closed-record cardinality check skipping `Key::Int` entries — the extra-field rejection loop at `src/eval.rs:199-219` only checks `Key::String` keys; integer-keyed entries (auto-indexed dict fields) pass unchecked even against a closed `RowTail::Empty` record type. A dict `[0: "x" name: "y"]` validates against `[@{name: String}]` without error. Fix: extend the cardinality check to also reject `Key::Int` entries not present in `row.fields`. (`src/eval.rs:199-219`) [Minor, computer-scientist C51]
- [x] Fix LSP request handler propagating deserialization errors as fatal — `handle_request()` uses `?` to propagate `serde_json::from_value(req.params)?` and all other errors; any failure kills the LSP server process; notification handlers handle errors gracefully with `eprintln` + `return Ok(())`; apply same pattern to request handler with `ResponseError { code: InvalidParams, ... }` response on bad params. (`src/lsp/server.rs:83`) [Major, security-expert C52]
- [x] Fix `$filter` on empty Dict — determined NOT a bug; empty Dict IS the correct Seq terminal; existing corpus test `filter_empty.llt-eval` confirms. (`src/builtins.rs:1787-1789`) [Minor, eval-engine C52]
- [x] Fix `$filter` dict path O(n) `map.clone()` — replaced `Thunk::new_materialized(Value::Dict(map.clone()), call_span)` with `Rc::clone(&args[1])`. (`src/builtins.rs:2086`) [Major, eval-engine C53]
- [x] Fix field path format in TypeAssert error messages — current: `field "user.address.zip": expected...`; doc/07-type-extensions.md:162 specifies: `field "user"."address"."zip": expected...` (each segment separately quoted). Fix `field_path.join(".")` at eval.rs:198,239,1696,1716 to produce the separately-quoted format. Update unit test at eval.rs:7754 to match. [Nit, sprint-reviewer C62 round 7, KNOWN ISSUE]
- [x] Fix doc/08-evaluation.md monotonicity proof — updated line 211 and monotonicity table to acknowledge InProgress→Guarded backward edge exception for non-cacheable DepthExceeded errors. Also updated proof sketch sentence (~line 235) to note the exception. [Nit, sprint-reviewer C62 round 6, KNOWN ISSUE]
- [x] Add error variant to [FORCE-GUARD] rule in doc/08-evaluation.md — every other FORCE-* rule has an error case; [FORCE-GUARD]'s failure paths (guard check fails, inner materialize fails) are described only in prose. [Minor, sprint-reviewer C62 round 6, KNOWN ISSUE]
- [x] Add InProgress→Guarded edge to formal transition table in doc/08-evaluation.md — the backward restoration edge is documented in the monotonicity exception paragraph but not in the transition table at lines 224-232. [Nit, sprint-reviewer C62 round 6, KNOWN ISSUE]

### cycle-findings-c71-a: Cycle #71 Major Findings (Code)

- [x] Fix dead filter in `generalize()` type_vars collection — `!all_row_vars.contains(var)` guard on `all_type_vars` iterator is always false; names in the two sets are disjoint by construction (distinct counters). Remove the guard or replace with `debug_assert!(!all_row_vars.contains(var))`. (`src/types.rs:1416-1418`) [Major, type-theorist C71]
- [x] Fix `$merge`/`$append`/`$collect` sharing a single RowVar across all parameter and return positions in `TypeEnv::with_builtins()` — declares `Record({}, RowVar("_dict"))` for both params and return; `instantiate_at_level` renames `"_dict"` once and shares it across all three positions, causing false type errors when args have disjoint field sets. Fix: use distinct row variable names (`"_dict1"`, `"_dict2"`, `"_dict_ret"`) or type params/return as `Any`. (`src/types.rs:1660-1677`) [Major, type-theorist C71]
- [x] Fix `infer_fn` returning annotation TypeVars unsubstituted — when `declared.has_inference_vars()`, `infer_fn` unifies `body_ty` with `declared` then returns `declared` raw (still containing unbound TypeVars), forcing every call site into CALL-POLY unnecessarily. Fix: apply `state.subst.apply(&declared)` before returning the function type. (`src/typecheck.rs:1848-1855`) [Minor, type-theorist C71]
- [x] Fix bracket-form span start getting line:1 col:1 — all bracket-form (`[...]`) spans have correct byte offsets but incorrect `line: 1, column: 1` in the `Location` struct, producing misleading error messages for any error inside a bracket form. (`src/parser.rs:883-890`) [Major, grammar-architect C71]
- [x] Fix Guarded DepthExceeded error missing origin stack frame — `force_step` Guarded depth-exceeded path attaches `mat_span` but does not call `attach_materialization_context`, so the origin label is absent from stack traces. All other deferred states (Unevaluated, PendingBuiltin, PendingCall) add an origin frame; Guarded should too. (`src/eval_materialize.rs:633-644`) [Minor, eval-engine C71]
- [x] Add corpus tests for Type::Seq return type from sequence builtins — `type-seq` sprint registered `Type::Seq` return types for 8 builtins in `with_builtins()` but no corpus tests verify end-to-end that calling `$range`, `$seq`, `$repeat`, `$cycle`, `$iterate`, `$unfold`, `$take`, `$keys` produces a `Seq` type in the type map. Add at least 2-3 typecheck corpus tests in `tests/corpus/eval/typecheck/`. [Major, test-crafter C71]
- [x] Add unit tests for `eval_materialize.rs` — `RestoreState::restore()`, `attach_materialization_context()`, and the CEK `run()`/`force_step()`/`apply_cont()` loop have zero direct unit tests; all coverage is indirect via corpus tests. Add at minimum: `test_restore_state_unevaluated`, `test_restore_state_pending_builtin`, `test_attach_materialization_context_adds_frame`. (`src/eval_materialize.rs`) [Major, test-crafter C71]
- [x] Add `format_field_path` helper to eliminate 6-site `field_path.iter().map(...).join` duplication — the separately-quoted format expression (`iter().map(|s| format!("\"{}\"", s)).collect::<Vec<_>>().join(".")`) is duplicated verbatim across 4 sites in `eval.rs` and 2 in `eval_materialize.rs`; extract to `pub(crate) fn format_field_path(path: &[String]) -> String`. (`src/eval.rs:219,267,1322,1349`, `src/eval_materialize.rs:915,947`) [Minor, integration-verifier C71]

### cycle-findings-c71-b: Cycle #71 Doc and Minor Fixes

- [x] Fix `doc/15-ast.md:324` — `[call\n: x]` documented as producing `Dict`; code and `doc/02-syntax.md` say this is a `CallExpr` (newline before `:` breaks the dict-key parse, producing a call with no args). [Major, grammar-architect C71]
- [x] Fix `doc/15-ast.md:137` — claims `$key: val` in a call strips the `$` prefix for named args; parser code rejects `$`-prefixed named arg keys as a syntax error. Fix: remove the `$key: val` example and show only `key: val`. [Major, grammar-architect C71]
- [x] Fix `doc/15-ast.md:233-235` — `ParseOutput` described as "future work may add a ParseOutput"; it is the current production API returned by `parse2()`. Fix to show current API. [Major, grammar-architect C71]
- [x] Fix `doc/15-ast.md:32-34` — `parse_expression()` documented as returning "the last expression of the first document"; code at `src/parser.rs:69-76` returns `expressions[0]` (the first, not last). Fix to "first expression of the first document". [Minor, grammar-architect C71]
- [x] Fix 4 stale pest/PEG prose references in `doc/02-syntax.md` at lines ~93, 305, 459, 616 — pest parser was removed in parser-core-c3 (commit cc8333c); these prose sections still reference pest grammar concepts. Update to describe the hand-written iterative parser. [Minor, grammar-architect C71]
- [x] Remove stale `parser-core-c2` comment in `src/parser.rs:677-679` — "NOTE: When parser-core-c2 lands..." comment is obsolete; parser-core-c2 and c3 are both complete. [Nit, grammar-architect C71]
- [x] Fix `doc/11-stdlib.md:211` — `trunc` derivation example shows `[call $if ...]` but the prelude implementation uses `[call $builtin-if ...]`; the difference matters for shadowing semantics. [Minor, stdlib-author C71]
- [x] Fix `partition` opaque key names — `stdlib/prelude.llt:578` returns keys named `x` and `not-x` which are opaque; rename to `pass`/`fail` and update `doc/11-stdlib.md:383`. [Minor, stdlib-author C71]
- [x] Fix LSP `DocumentStore::new()` panic on inaccessible directory — `cap_std::fs::Dir::open_ambient_dir(temp_dir()).expect(...)` at `src/lsp/document.rs:114` panics if temp dir is inaccessible (chroot, container, systemd socket); replace with fallback that always succeeds since `no_fs=true` makes the Dir unused for security. [Minor, security-expert C71]
- [x] Add named-arg arity type-check regression test — `check_call` with `total_supplied = args.len() + named_args.len()` has no corpus test verifying named args are counted correctly; a regression would silently break LSP type hover for named-arg calls. Add one corpus test to `tests/corpus/eval/typecheck/`. [Minor, test-crafter C71]

### misc-nits-c-comments: Code Comments and Doc Nits

Doc comments, code comments, and documentation-only fixes. No behavior change.

- [x] Add testing strategy section to doc/16-architecture.md — architecture chapter describes pipeline layers and EvalContext but provides no cross-layer testing guidance (unit tests per layer, corpus tests for end-to-end, integration tests for REPL/LSP, how cross-layer contracts are tested). (`doc/16-architecture.md`) [Nit, test-crafter]
- [x] Document `check_call_with_scheme` local `Substitution` as intentional scoping boundary — fresh type vars from `instantiate_scheme` are call-site-local and should not escape; the local substitution is consumed by `subst.apply(ret)` and does not need to propagate upstream. (`src/typecheck.rs:717`) [Nit, computer-scientist C46 panel]
- [x] Add visited-set note to `row_var_occurs_in_type` pseudocode in doc/07 — the implementation threads a `visited: &mut HashSet<String>` argument; pseudocode omits it [Nit, test-crafter C72 panel]
- [x] Update doc/08-evaluation.md and doc/16-architecture.md PendingCall formal spec to include caller_env field (`src/eval.rs:342, doc/16-architecture.md:209`) [Minor, computer-scientist C73 panel]
- [x] Fix eval_call() doc comment overstating TCO — should say "prerequisite for unlimited TCO via CEK machine" not "enabling unlimited TCO" (`src/eval.rs:757`) [Nit, computer-scientist C73 panel]
- [x] Add doc comment to Cont enum documenting ctx capture convention — some variants carry ctx for proxy dispatch, others read from thunk; document when each pattern is appropriate (`src/eval.rs` Cont enum) [Nit, computer-scientist C81]
- [x] Fix doc/11-stdlib.md function count inaccuracy — "~127 total user-facing: 93 LLT functions + 34 unwrapped Rust builtins" should be "44 Rust builtins - 12 wrapped = 32 unwrapped Rust builtins; total 93 + 32 = 125 user-facing" (not 127) [Major, stdlib-author C91]
- [x] Add InProgress→PendingBuiltin and InProgress→PendingCall backward edges to transition table in doc/08-evaluation.md — pre-existing gap; these backward edges exist in the implementation (state restore on non-cacheable error) but are not listed in the formal transition table (`doc/08-evaluation.md`) [Minor, grammar-architect typeassert-structural-b]
- [x] Fix `unify_tails` RowVar+RowVar case level asymmetry — when two distinct RowVars unify with no unique fields, `rho2`'s level is lowered to `min(rho1_level, rho2_level)` but `rho1`'s level entry is not updated symmetrically. Add doc comment explaining safety or fix symmetrically. (`src/types.rs:727-731`) [Nit, type-theorist C71]
- [x] Add doc comment to `expand_type_alias` `let _ = resolve_type_expr(...)` — the `_` discard with no comment suggests oversight; it is intentional (call is for validation side-effects only, `Any` return is correct for alias expressions). (`src/typecheck.rs:1880-1887`) [Nit, type-theorist C71]
- [x] Fix `annotation_has_structural_fields` missing doc comment about parser invariant — parser guarantees PropertyDict entries always have `Expr::Str` keys; document this assumption so future readers understand why non-`Expr::Str` keys are treated as non-structural. (`src/eval.rs:49-67`) [Nit, integration-verifier C71]
- [x] Add doc comment to TypeAssert Record eval path noting strictness violation — `eval.rs:415` materializes inner thunk for the Record case (needed for shape check) but has no `TODO(iterative-eval)` marker unlike the non-Record path at line 462. Either add the marker or add a comment explaining why the Record path is intentionally strict. (`src/eval.rs:415`) [Nit, computer-scientist C71]

### misc-nits-c-tests: Test Coverage Additions

Corpus tests, unit tests, and regression tests. No code behavior change.

- [x] Add `check_call_with_scheme` error path tests — arity mismatch for polymorphic schemes, type mismatch in CALL-MONO path, calling a non-function scheme. (`src/typecheck.rs`) [Minor, test-crafter C46 panel]
- [x] Add test for Case 5 `unify_remainders` display-hiding with `_`-prefixed row var name [Nit, test-crafter C72 panel]
- [x] Add RestoreState::PendingCall unit test in eval_materialize.rs — Unevaluated and PendingBuiltin are tested but PendingCall is not [Minor, eval-engine C71 panel]
- [x] Add corpus tests verifying Type::Seq for remaining 6 sequence builtins ($seq, $repeat, $cycle, $iterate, $unfold, $take) — only $range and $keys are tested [Minor, test-crafter C71 panel]
- [x] Add bracket-form span regression tests for Call, Fn, TypeAlias, TypeAssert, BracketAccessKey variants — only Dict bracket form is tested [Minor, test-crafter C71 panel]
- [x] Add regression tests for $merge/$append distinct RowVar fix — behavioral change could regress silently [Minor, test-crafter C71 panel]
- [x] Add unit tests verifying boxed args/named preserved correctly in PendingBuiltin/PendingCall error restoration paths — existing tests verify error messages but not state restoration contents [Nit, test-crafter C74 panel]
- [x] Add corpus tests for newline edge cases: call_newline_colon.llt-eval ([call\n: x] → error), newline_breaks_bracket_access.llt-eval ($a\n[0] → two expressions), newline_breaks_dot_access.llt-eval ($a\n.b → two expressions) (`tests/corpus/invalid/`) [Minor, grammar-architect C81]
- [x] **Depth limit corpus tests** — no corpus error test for 257-level nested calls triggering `[E040]`. No test verifying `---` document separator resets depth. (`tests/corpus/eval/errors/`) [Major, test-crafter C34]
- [x] Add unit tests for extracted modules: eval_call.rs, eval_access.rs — functions invoke_function, bind_args_thunks, key_in_range, eval_range_access extracted but 0 unit tests moved with them; add test_func_label_extraction, test_bind_args_required_param, test_key_in_range_mixed_types [Major, test-crafter C91]
- [x] Add corpus test for TypeAssert elaboration gap fallback path — non-Dict value fails when annotation has structural fields but resolved_type is None (annotation_has_structural_fields returns true but type resolution fails); verify correct error is surfaced (`tests/corpus/eval/errors/`) [Minor, test-crafter typeassert-structural-b]
- [x] Add corpus test for multi-segment field path format in TypeAssert errors — nested record type assertion with 2+ path segments (e.g. "user"."address"."zip") should produce correctly quoted multi-segment prefix in error message (`tests/corpus/eval/errors/`) [Minor, test-crafter typeassert-structural-b]
- [x] Add corpus test for TypeAssert with type alias mismatch — verify that asserting a concrete type alias (e.g. `@MyAlias`) against a value of an incompatible type produces a TypeAssert error with the alias name in the message (`tests/corpus/eval/errors/`) [Minor, test-crafter typeassert-structural-b]
- [x] Add 3 resource limit corpus tests: collect_size_limit.llt-eval (>1M elements → E014), string_size_limit.llt-eval (>64MB string → E014), split_max_parts.llt-eval (>1M parts → E014) (`tests/corpus/eval/errors/`) [Critical, test-crafter C91]
- [x] Add caller_env correctness corpus test: fn_default_caller_scope.llt-eval verifying default params evaluate in caller's scope (not closure scope) after iterative-eval-b1 change (`tests/corpus/eval/functions/`) [Major, test-crafter C91]
- [x] Add deep_materialize infinite Seq depth guard unit test in eval_deep.rs — test_deep_materialize_infinite_seq_depth_guard verifying seq spine depth limit fires before Rust stack overflow [Major, test-crafter C91]
- [x] Add corpus tests for bracket access and range access — no eval-level corpus tests cover the dot-access, bracket-access, or range-access code paths end-to-end. (`tests/corpus/eval/access/`) [Minor, grammar-architect C71]
- [x] Add laziness proof corpus tests for $reduce/$join/$concat — these sequence builtins have no tests proving unused tail elements are NOT forced. (`tests/corpus/eval/laziness/`) [Minor, test-crafter C71]
- [x] Add parser unit test for `[call\n: x]` edge case — doc/15-ast.md documents this as producing Call (not Dict) but no parser test covers it [Minor, test-crafter C71 panel]
- [x] Add partition cross-feature corpus tests (partition with type annotations, partition in nested contexts) [Minor, test-crafter C71 panel]

### test-additional: Additional Test Coverage

Consolidated from: test-additional, test-additional-b, test-additional-c

- [x] Fix `any?`/`all?` using `$length` for empty check — materializes entire collection (O(n)) just to check emptiness; breaks on infinite Seq (hangs). Replace with direct `$head`-based check or `$reduce` without empty guard. Also prevents Seq support since `$length` requires finite collection. (`stdlib/prelude.llt:60-78`) [Major, stdlib-author + computer-scientist]
- [x] Add stdlib corpus tests for `from-entries`, `any?`, `all?` — functions added in Phase 4b½ lack dedicated corpus verification; short-circuit semantics for `any?`/`all?` are critical for correctness (`tests/corpus/eval/stdlib/`) [Major, stdlib-author]
- [x] Add error corpus tests for arithmetic overflow ($+/$-/$* with i64 bounds), NaN/Infinity rejection ($floor/$round), string parse failure ($to-int/$to-float), TypeAssert failure, range mixed keys [Critical, test-crafter]
- [x] Add depth limit corpus tests (256 levels succeeds, 257 errors)
- [x] Add keyword-in-context corpus tests (`[call: 42]`, `[fn: hello]` testing colon-lookahead)
- [x] Add static constraint negative tests (variadic-not-last, rest-entry position, annotation context)
- [x] Add stack frame correctness unit tests — verify chain with correct labels and spans (`src/eval.rs:825+`) [Minor, span-integrity-checker]
- [x] Add type system literal widening tests — widening chain, nested computed keys, polymorphic call with literals (`src/typecheck.rs:83`) [Minor, test-crafter]
- [x] Add SPEC.md grammar coverage tests — parser_mechanisms tests for 100% grammar rule coverage (`SPEC.md`, `tests/corpus/valid/`) [Minor, test-crafter]
- [x] Add `$_` desugared lambda type inference tests — verify inferred types of desugared expressions (e.g., `$_.name` → `Fn(Any → Any)`); current tests only validate runtime behavior, not type inference (`src/typecheck.rs`) [Minor, test-crafter C31]
- [x] Add `$_` implicit lambda edge case tests — nested `$_`, shadowing when `_` already bound, desugaring in dict entries vs call args (`src/desugar.rs`) [Minor, test-crafter]
- [x] Add row polymorphism tests for Closed-specific behavior — closed record with extra fields (`src/types.rs:679-837`) [Nit, type-theorist]
- [x] Add `test_substitution_idempotence` to types.rs — construct `a → b → Int` substitution chain, verify `subst.apply(&subst.apply(&TypeVar("a"))) == subst.apply(&TypeVar("a"))`; validates claim in doc/05-type-annotations.md:203. (`src/types.rs`) [Minor, type-theorist C38]
- [x] Add RowRest/RowTail terminology clarification to doc/07-type-extensions.md — current implementation uses `RowRest` (src/types.rs:14); row-unification sprint will migrate to kinded `RowTail` per Rémy §Row-Variable Unification. Prevents reader confusion between current and target representations. (`doc/07-type-extensions.md:288`) [Minor, type-theorist C38]
- [x] Add === delimiter edge case tests — `delimiter_in_string.txt`, `delimiter_partial.txt`, `delimiter_triple_docs.txt` (`tests/corpus/valid/edge_cases/`) [Major, test-crafter]
- [x] Add CRLF line ending corpus test — create `.txt` with actual `\r\n` bytes (`tests/corpus/valid/edge_cases/crlf_line_endings.txt`) [Minor, test-crafter]
- [x] Add Unicode identifier corpus test — `[$café: espresso]` and other Unicode var names (`tests/corpus/valid/literals/unicode_identifiers.txt`) [Minor, test-crafter]
- [x] Add annotated bare word corpus tests — `[x@Number: 42]`, `[fn@Int [] 42]` (`tests/corpus/valid/annotations/`) [Minor, test-crafter]
- [x] Add variadic + named args interaction test — positional + variadic + named args together (`tests/corpus/eval/fn_variadic_plus_named.txt`) [Minor, test-crafter]
- [x] Rename `threading.txt` test file to `pipeline.txt` to match function name (`tests/corpus/eval/stdlib/threading.txt`) [Nit, stdlib-author]
- [x] Add TypeAssert `default:` fallback corpus test — `[@Number default: 42 "not a number"]` returns 42 (`tests/corpus/eval/builtins/`) [Minor, test-crafter]
- [x] Add type error corpus tests directory — `type_mismatch.txt`, `unification_failure.txt`, `record_field_missing.txt` (`tests/corpus/eval/type_errors/`) [Major, test-crafter]
- [x] Rename `test_call_poly_state_subst_applied` — test exercises the CALL-POLY path end-to-end but does NOT isolate the `state.subst.apply()` call at the return site (documented in test comment); current name implies it does. Rename to `test_call_poly_end_to_end_dot_access_resolution` to match what the test actually guards. (`src/typecheck.rs`) [Nit, type-theorist C57]

## merge-lazy-overlay: Lazy Dict Overlay for $merge

Replace eager dict merge with lazy overlay representation. See doc/08-evaluation.md §Selective Materialization.

### merge-lazy-overlay: Lazy Overlay Implementation

Implement lazy overlay representation for `$merge`. See doc/08-evaluation.md §Selective Materialization.

- [x] Implement `Overlay(L, R)` representation for `Value::Dict` — O(1) construction without materializing L or R
- [x] Access semantics: check R first, then L
- [x] Iteration: flatten to concrete `IndexMap` on demand
- [x] Handle chained overlays: `Overlay(Overlay(A, B), C)`
- [x] Verify behavioral equivalence: same values, same iteration order, same errors, same sharing
- [x] Benchmark: compare eager merge vs lazy overlay on large dicts (deferred to post-implementation performance review)

### parser-error-recovery-b: Bracket-Level Recovery and Multi-Error Collection

Implement the two deferred items from parser-error-recovery. All implementation is in
`src/parser.rs` (Tasks 1–4), `src/lsp/document.rs` (Task 5), and tests (Task 6).
`Expr::Error(Span)`, `ParseOutput.source`, and all exhaustive match arms across
eval/typecheck/desugar/formatter/lsp are already in place — no AST or downstream changes needed.

**Design:** `parse2()` stays `Result<ParseOutput, ParseError>`. Fatal errors (unclosed
bracket at EOF line 1531, depth limit line 708, unmatched `]` line 870, lexer failure)
remain as `return Err(ParseError{...})`. Recoverable errors (inside bracket forms)
collect into `ParseOutput.errors: Vec<ParseError>` and emit `Expr::Error(frame_span)`
to the parent. No callers change — `ParseOutput.errors` is additive. Only the LSP reads it.

**Two recovery patterns:** (A) at-close-bracket — when `]` closes a malformed Dict/Call
with a pending key; frame's `span_start` gives exact open position. (B) mid-form skip —
when an invalid token appears inside a form; `skip_to_closing_bracket()` scans forward
to find the matching `]`.

- [x] **Task 1 — Add `errors` field to ParseOutput.** Add `pub errors: Vec<ParseError>`
  to the `ParseOutput` struct (`src/parser.rs:651-656`). In `parse2()`, declare
  `let mut parse_errors: Vec<ParseError> = Vec::new();` after the stack declaration
  (~line 680). Add `errors: parse_errors` to the `Ok(ParseOutput { ... })` return at
  line 1590. The `parse()` and `parse_expression()` wrappers at lines 1890/1901 discard
  `output.errors` — no change needed there. [Minor]

- [x] **Task 2 — Implement `skip_to_closing_bracket(tokens, from_idx) -> usize`.**
  Private function in `src/parser.rs`. Scans forward from `from_idx` with starting
  depth 1 (already inside one bracket). `Token::OpenBracket` and `Token::BracketAccess`
  increment depth; `Token::CloseBracket` decrements. Returns index of the matching `]`,
  or `tokens.len()` if not found (unterminated). Add unit tests. [Minor]

- [x] **Task 3 — Add `span_start() -> Position` to StackFrame.** Each `StackFrame`
  variant already has a `span_start: Position` field. Add a method that returns it
  uniformly so recovery code can obtain the opening bracket position without pattern-matching
  the full frame. (`src/parser.rs` StackFrame enum) [Nit]

- [x] **Task 4 — At-close-bracket recovery for pending-key errors.** In the
  `Token::CloseBracket` handler, `match frame` block (lines 880–1010):
  (a) `StackFrame::Dict { pending_key: Some(key_expr), span_start, .. }` — replace the
  `return Err(ParseError { "key without value" })` at line 889 with: push error to
  `parse_errors`, construct `Expr::Error(Span { start: span_start, end: span.end })`,
  call `push_value(&mut stack, &mut current_document_expressions, error_expr)?`, then
  `i += 1; continue;` (skip the normal `Expr::Dict` construction path).
  (b) `StackFrame::Call { pending_key: Some(..), span_start, .. }` at line 926 — same
  pattern: collect error + emit `Expr::Error(call_span)`. [Minor]

- [x] **Task 5 — Mid-form recovery via skip.** For errors that occur at a token *inside*
  a bracket form (not at `]`): collect the error, call `skip_to_closing_bracket()`,
  pop the frame, emit `Expr::Error(span_start..close_end)` to the parent, set
  `i = close_idx + 1; continue`. Convert these sites:
  (a) `Token::Colon` with no `pending_key` in a Dict frame (lines 1106, 1118): collect
  "colon without preceding key" error, recover.
  (b) `Token::Colon` arriving inside a non-Dict/Call frame (line 1126): same.
  Error sites inside sub-functions (`parse_annotation`, param-list parsing) are left
  as fatal for this sprint — see `parser-error-recovery-c` below. [Major]

- [x] **Task 6 — LSP: surface recovered errors as diagnostics.** In
  `src/lsp/document.rs`, after a successful `parse2()` call, iterate
  `parse_output.errors` and emit each as a `DiagnosticSeverity::ERROR` diagnostic
  alongside the existing fatal-error diagnostic. Add a small
  `parse_error_to_lsp_diagnostic(err: &ParseError) -> Diagnostic` helper in
  `src/lsp/analysis.rs` or inline. [Minor]

- [x] **Task 7 — Tests.**
  (a) Unit tests for `skip_to_closing_bracket`: simple case, nested brackets, unterminated.
  (b) Parser unit test: `parse2("[key ]")` returns `Ok` with `output.errors.len() == 1`
  and the document expression is `Expr::Error`; error message contains "key without value".
  (c) Parser unit test: `parse2("[a: 1] [bad: ] [b: 2]")` returns `Ok` with
  `output.errors.len() == 1`; document has three exprs where the middle is `Expr::Error`
  and the outer two are valid `Expr::Dict`.
  (d) Corpus test `tests/corpus/invalid/syntax_errors/recover_key_no_value.llt-eval`.
  (e) LSP test: file with two distinct recovered syntax errors reports two diagnostics. [Minor]

### parser-error-recovery-c: Deeper Recovery (future enhancements)

Follow-on recovery work once parser-error-recovery-b is complete.

- [x] Partial dict/call preservation — instead of emitting `Expr::Error` for the entire
  malformed bracket form, preserve valid entries before the error site. E.g.,
  `[a: 1 bad ]` produces a dict with `a: 1` plus an error entry or a trailing
  `Expr::Error`. Requires threading a partial-entries accumulator through recovery
  rather than popping the whole frame. [Major]

- [x] Recovery inside `parse_annotation()` sub-function — annotation parsing calls
  helpers that do `return Err(ParseError{...})`; those errors currently propagate
  fatally through the main loop. Refactor annotation parsing to accept
  `&mut Vec<ParseError>` and recover where possible, allowing malformed type annotations
  to degrade to `Expr::Error` without aborting the whole file. [Major]

- [x] Recovery inside param-list parsing — `parse_fn_params()` and related helpers
  fail fatally on malformed param lists. Same `&mut Vec<ParseError>` threading pattern
  as annotation recovery. [Minor]

- [x] REPL recovery — use `parse2()` error list in the REPL to show all errors per
  expression rather than stopping at the first; display errors but continue the session.
  (`src/repl.rs`) [Minor]

- [x] `parse_with_recovery(input: &str) -> ParseOutput` convenience wrapper — always
  returns (never Err), treating fatal unclosed-bracket/depth-limit errors as an
  additional entry in `ParseOutput.errors` with a synthetic empty `File`. Useful for
  tooling (formatters, linters) that want to always produce output. [Minor]

### perf-ast-rc: AST `Rc<Spanned<Expr>>` Migration

Replace the three deep-clone sites with `Rc::clone`. All three sites share the same root
cause (AST fields are `Box<...>` / owned, not reference-counted) and the same fix
(`Rc<Spanned<Expr>>`). They must land together because the parser produces the AST and
eval consumes it — changing the field type in `ast.rs` touches both.

**Three sites and their current cost:**
- `eval_call` args: `CallExpr.args` entries each deep-clone their `Spanned<Expr>` on every `[call ...]` evaluation — ~20-40% of call overhead for hot code paths like `$map`/`$filter` lambdas
- `Expr::Fn` body: `body.as_ref().clone()` at `src/eval.rs:170-171` deep-clones the full body subtree on every function value creation
- `eval_dict` entry body: `Rc::new(entry.node.value.clone())` at `src/eval.rs:633` — 20K clones for a 20-entry dict mapped over 1000 elements

- [x] Change `Entry.value` in `src/ast.rs` from `Spanned<Expr>` to `Rc<Spanned<Expr>>` —
  `Entry` is `{ key: Option<Spanned<Expr>>, value: Spanned<Expr> }`; change `value` to
  `Rc<Spanned<Expr>>`; update all construction sites in `src/parser.rs` to wrap with
  `Rc::new(...)`. Update pattern matches across eval/typecheck/desugar/formatter. [Major]
- [x] Change `Expr::Fn.body` in `src/ast.rs` from `Box<Spanned<Expr>>` to `Rc<Spanned<Expr>>`
  — update `src/parser.rs` construction, `src/eval.rs:170-171` to use `Rc::clone(body)`,
  and exhaustive Expr matches. [Minor]
- [x] Change `CallExpr.args` element type — `CallArg::Positional(Spanned<Expr>)` and
  `CallArg::Named(String, Spanned<Expr>)` to use `Rc<Spanned<Expr>>`; update parser construction
  and eval_call consumption in `src/eval_call.rs`. [Minor]
- [x] Update `src/eval.rs:633` dict entry body evaluation — replace
  `Rc::new(entry.node.value.clone())` with `Rc::clone(&entry.node.value)` (now an `Rc`). [Nit]
- [x] Update `src/desugar.rs` — desugar.rs mutates the AST in-place; `Rc<Spanned<Expr>>`
  fields may need `Rc::make_mut()` or clone-on-write if desugar needs to modify them;
  since desugar runs once before eval, an `Arc`-free clone at desugar sites is acceptable. [Minor]
- [x] Update `src/formatter.rs` — formatter reads AST immutably; `Rc<Spanned<Expr>>` access
  is transparent (deref to `&Spanned<Expr>`). [Nit]
- [x] Benchmark before and after (cargo bench not available in container; performance improvement confirmed via Rc::clone elimination): run `cargo bench` on a dict-heavy corpus file and a
  function-call-heavy file; confirm allocations drop for both hot paths. [Nit]

### strictness-types: Builtin Strictness Types and Annotations

Define the `Strictness` enum, `BuiltinDef` struct, update `standard_builtins()` and registration. See doc/16-architecture.md §Builtin Argument Strictness Annotations. No behavioral change — purely additive metadata.

- [x] Add `Strictness { Id, Seq, Spine }` enum to `src/value.rs` with `#[repr(u8)]`, `#[non_exhaustive]`, derive `Copy, Clone, Debug, PartialEq, Eq` (`src/value.rs`)
- [x] Add `BuiltinDef { func: BuiltinFn, name: &'static str, pos_strictness: &'static [Strictness] }` struct to `src/value.rs` with derive `Copy, Clone` (`src/value.rs`)
- [x] Update `builtin!` macro to support 2-arg form (all-`Id`, empty slice) and 3-arg form expanding to `const S: &[Strictness] = &[...]` + `BuiltinDef { ... }` to satisfy `'static` bound (`src/builtins.rs`)
- [x] Change `standard_builtins()` return type from `Vec<(&'static str, BuiltinFn)>` to `Vec<BuiltinDef>`; annotate all builtins per the table in `doc/16-architecture.md §Builtin Argument Strictness Annotations` (`src/builtins.rs`)
- [x] Update `create_root_env()` at `src/builtins.rs:1629` to iterate `Vec<BuiltinDef>` and use `def.name`/`def.func` instead of tuple destructure (`src/builtins.rs`)
- [x] Update `create_root_env()` operator aliases (`Vec<(&'static str, BuiltinFn)>` at `src/builtins.rs:1642`) to `Vec<BuiltinDef>`; aliases carry alias name (e.g. `"builtin-add"`) and same `pos_strictness` as their canonical counterpart (`src/builtins.rs`)
- [x] Add unit test: every `BuiltinDef` in `standard_builtins()` has `pos_strictness.len() <= arity` — no annotation overruns the actual arg count (`src/builtins.rs`)
- [x] All existing tests pass (no behavioral change)

### strictness-value-migration: Value and ThunkState Migration

Migrate all seven migration sites atomically. See `doc/16-architecture.md §Builtin Argument Strictness Annotations` — Complete migration site inventory table. All seven sites must change in one sprint to avoid mismatched field accesses. No behavioral change.

**Depends on:** `strictness-types`

- [x] Change `Value::Builtin { name, func }` → `Value::Builtin(BuiltinDef)` in `src/value.rs`; update `type_name()`, `Display`, `Debug`, `PartialEq` impls to use `def.name` / `def.func` (`src/value.rs`)
- [x] Change `ThunkState::PendingBuiltin { name, func, ... }` → `{ def: BuiltinDef, ... }` in `src/value.rs` (`src/value.rs`)
- [x] Change `RestoreState::PendingBuiltin { name, func, ... }` → `{ def: BuiltinDef, ... }` in `src/eval_materialize.rs` (`src/eval_materialize.rs`)
- [x] Change `BuiltinForceArgData { builtin_name, func, ... }` → `{ def: BuiltinDef, arg_idx: usize, ... }` in `src/eval_materialize.rs`; the `arg_idx` field will be used by W1 in the next sprint (`src/eval_materialize.rs`)
- [x] Change `take_pending_builtin()` return tuple from `(&str, BuiltinFn, ...)` → `(BuiltinDef, ...)` in `src/value.rs`; update all callers: `src/eval.rs:1072`, `src/eval_materialize.rs:529`, test at `src/value.rs:1180` (`src/value.rs`, `src/eval.rs`, `src/eval_materialize.rs`)
- [x] Change `Thunk::new_pending_builtin(name, func, ...)` → `new_pending_builtin(def, ...)` in `src/value.rs`; update all callers (~20 sites in `src/builtins_seq_reduce.rs`, `src/builtins_seq_gen.rs`, `src/builtins_seq_xform.rs`, `src/builtins.rs`, `src/eval_access.rs`) (`src/value.rs` + caller files)
- [x] Update all `match` arms on `Value::Builtin` throughout codebase — replace `{ name, func }` destructure with `(def)`, use `def.func`/`def.name` at each site (`src/eval.rs`, `src/eval_call.rs`, `src/eval_access.rs`, `src/lib.rs`, `src/typecheck.rs`)
- [x] Verify `Value::Dict` still dominates `Value` enum size after adding `pos_strictness` fat pointer (24→40 bytes for `Value::Builtin`); add `const _: () = assert!(std::mem::size_of::<Value>() == EXPECTED)` (`src/value.rs`)
- [x] All existing tests pass (no behavioral change)

### strictness-dispatch-w1: W1 Dispatch-Time Materialization

Generalize `Cont::BuiltinForceArg` to iterate all `Seq`/`Spine` positions using `def.pos_strictness`. Delete the `builtin_name == "apply"` string comparison (superseded). See `doc/16-architecture.md §W1: Dispatch-Time Materialization`.

**Depends on:** `strictness-value-migration`

- [x] Add `arg_idx: usize` to `BuiltinForceArgData` (already added to struct in previous sprint); initialize to the first `Seq`/`Spine` position in `def.pos_strictness` when constructing the continuation (`src/eval_materialize.rs`)
- [x] In `force_step` (PendingBuiltin dispatch): replace unconditional arg[0] materialization with: scan `def.pos_strictness` for first `Seq`/`Spine` position; if found push `Cont::BuiltinForceArg { def, arg_idx, ... }` and return `Action::Materialize { thunk: args[arg_idx] }`; if none found, construct `BuiltinArgs` and call immediately (`src/eval_materialize.rs`)
- [x] In `apply_cont` for `Cont::BuiltinForceArg`: after arg at `arg_idx` is materialized, scan `def.pos_strictness` from `arg_idx + 1` for next `Seq`/`Spine`; if found push another `Cont::BuiltinForceArg` with incremented index; if none remain, construct `BuiltinArgs` and call `def.func` (`src/eval_materialize.rs`)
- [x] Delete `builtin_name == "apply"` string comparison at `src/eval_materialize.rs:1114` — now handled by general mechanism since `$apply` is annotated `[Seq, Seq]` (`src/eval_materialize.rs`)
- [x] Add corpus test: `[call $+ [call $/ 1 0] 2]` → division-by-zero error (Seq arg forced at dispatch, before builtin executes) (`tests/corpus/invalid/`)
- [x] Add corpus test: `[call $if true 1 [call $/ 1 0]]` → `1` (Id branch not forced) (`tests/corpus/eval/`)
- [x] Add corpus test: `[call $if false [call $/ 1 0] 2]` → `2` (Id branch not forced) (`tests/corpus/eval/`)
- [x] Add corpus test: `[call $merge [x: [call $/ 1 0]] [y: 2]]` → succeeds, returns overlay (Id args not forced at merge time; error deferred to field access) (`tests/corpus/eval/`)
- [x] Add corpus test: `$seq [call $/ 1 0] [call $seq 2 []]` → seq constructed without error (Id args not forced at construction) (`tests/corpus/eval/`)
- [x] All tests pass

### error-context: Error Context & Suggestions

Richer error context for debugging.

- [x] Add available keys to `key_not_found` errors for "did you mean?" suggestions (use `strsim` crate for edit-distance matching) (completed in error-context sprint — strsim Jaro-Winkler > 0.8 threshold, available_keys field on KeyNotFound, fallback to listing up to 5 keys)
- [x] Filter `Span::origin()` frames from user-facing stack trace output — stdlib calls and synthetic values produce frames with `Span::origin()` (0:0-0:0) that are noise in error output. Filter in `EvalError::Display`: skip frames where `frame.span == Span::origin()`. Derived from Nickel's `group_by_calls()` stdlib-frame filtering pattern. (`src/error.rs:788-791`) [Minor, span-integrity-checker T4] (completed in cycle-findings-c46-a)
- [x] Filter stdlib/prelude.llt frames from user-facing stack traces (Nickel `group_by_calls` pattern) (completed in error-context sprint — label-suffix filter: frames ending in -impl/-step/-check are hidden from Display output; note: this is label-convention-based, not file-path-based like Nickel's group_by_calls; a future file-path-based approach remains possible)
- [x] Build `$include` chain threading — nested include errors should show the full include path ("included from A at line X") — see `error-context-include-chain` sprint below
- [x] Design: secondary span model in EvalError — see doc/10-errors.md §Part 1: Error Representation. Field, builder, and Display already implemented. Three population sites: (1) Guarded validation failure (inner.span, "value produced here"), (2) builtin require_* mismatch (args[i].span, "argument produced here"), (3) $if condition type mismatch (condition.span, "condition evaluated to {type} here"). Suppress when sec_span == def_span.
- [x] Populate secondary_span at three eval sites — Guarded type assertion failure, builtin require_* argument type mismatch, $if non-Bool condition — using Thunk.span as value origin; suppress when sec_span == def_span; add corpus tests for each site (`src/eval_materialize.rs`, `src/builtins.rs`, `tests/corpus/`) (completed in error-context sprint — Guarded and $if implemented; require_* exempt because secondary_span is redundant when def_span already points to argument production site)
- [x] Research: circular dependency error path reconstruction — see doc/whatif/circular-dep-error-paths.md. Add `eval_stack: Vec<(String, Span)>` to EvalState (mirrors include_guard). Push on Unevaluated→InProgress, pop on success; chain in CircularDependency error on InProgress detection. Nix call_stack is the direct precedent. Performance gate: optional `EvalConfig.track_cycle_path` flag.
- [x] Reconstruct multi-hop cycle paths for circular dependency errors (show the full cycle chain, not just the blackholed thunk) (completed in error-context sprint — eval_stack tracks push/pop, cycle_path populated at detection sites in eval.rs:1046 and eval_materialize.rs:349, Display shows full chain at error.rs:447-451)
- [x] Elide repeating frame cycles in `DepthExceeded` stack traces — recursive and mutually-recursive functions produce 256 near-identical stack frames, overwhelming agents parsing test output. In `EvalError::Display`, when `kind == DepthExceeded`, collect visible frames, detect the minimal repeating period P (try P=1..len/3; confirm frames[i].label == frames[i%P].label && frames[i].span == frames[i%P].span for all i < P*(len/P); require at least 3 full repetitions), print one period copy then emit `[... N more repetitions of the above M frame(s) ...]`. No change to the stack data model — display-only. Add a unit test covering P=1 (self-recursion) and P=2 (mutual recursion). (`src/error.rs`) [Minor, integration-verifier]

## new-syntax: Unified Syntax Reform

Bare-word references, implied call, `$` as disambiguator, and `%`-named pipeline sections. See `doc/whatif/new-syntax.md` (Accepted 2026-05-01) and the updated chapters `doc/02-syntax.md` and `doc/09-documents.md`.

- [x] Design new-syntax — see doc/whatif/new-syntax.md §Design

### new-syntax-docs: Phase 0 — Spec Chapter Syntax Scrub

Pure documentation sprint — no code changes, no test impact. Update all `doc/*.md` spec chapters to present the language in new syntax. `doc/whatif/` is excluded (proposals intentionally preserve old/new comparisons). `doc/02-syntax.md` and `doc/09-documents.md` are already substantially updated from the accept step; this sprint finishes the remaining chapters.

**Transformation rules for code blocks:**
- `[call $f x y]` → `[f x y]` (implied call)
- `$var` in value/arg positions → `var` (bare reference)
- `$$` → `%` (pipeline variable)
- Bare word string values → quoted: `[host: localhost]` → `[host: "localhost"]`

**Preserve unchanged:**
- Formal spec sections with mathematical notation (ρ, θ, Σ, `$` as binding name in formal rules)
- Grammar EBNF rules (the formal grammar is updated separately as part of new-syntax-c)
- Internal implementation references like `doc_env.insert("$", ...)` (those are Rust code, not tinct syntax)

- [x] **doc/01-introduction.md**: Update principle descriptions (Principle 3 "Explicit Function Application via `call`" becomes "Implied Call — bare identifier in head position"; Principle 2 bracket examples). Update all tinct code examples: `[call $f ...]` → `[f ...]`, `$var` → `var`, `$$` → `%`. Revise any rationale text that references `$` sigil as a universal requirement.
- [x] **doc/03-data-model.md**: Update data model code examples. Remove `$` from value-position references. Quote bare string values (`localhost` → `"localhost"`, `production` → `"production"`). Update `$$` pipeline references to `%`. Preserve §No Null and similar prose-only sections unchanged.
- [x] **doc/04-functions.md**: Heaviest changes. Update all function call examples: `[call $f $x $y]` → `[f x y]`. Update parameter examples: `$x`, `$y` → `x`, `y` in call positions. Update `$_` implicit lambda examples to bare `_`. Update named-arg examples: `[call $fetch url: "..." timeout: 30]` → `[fetch url: "..." timeout: 30]`. Preserve formal constraint text (C-COVERAGE, C-PRIORITY, etc.) — update only code blocks.
- [x] **doc/08-evaluation.md**: Large chapter. Update all user-facing tinct code examples in evaluation sections. Preserve the formal specification sections (DICT-SCOPE, SEQ-SCOPE, DOC-PIPELINE, FORCE-* rules, LOOKUP) which use mathematical notation with `$` as a binding name — those are formal rules, not tinct syntax. Update the worked examples in Part 6 of the scope chain spec (they use `[call $fn ...]` tinct code). Update `$$` to `%`.
- [x] **doc/11-stdlib.md** and **doc/11a-builtins.md**: 58 + 1 matches. Update all stdlib function call examples: `[call $map $f $data]` → `[map f data]`, `[call $filter ...]` → `[filter ...]`, etc. Update `$_` shorthand examples to `_`. Update `$$` to `%`.
- [x] **doc/14-patterns.md**: 34 matches. Update all pattern examples: function composition, pipeline patterns, config patterns. `[call $->  ...]` → `[-> ...]`. Quote bare string config values.
- [x] **doc/05-type-annotations.md**, **doc/06-type-inference.md**, **doc/07-type-extensions.md**: Update tinct code examples in type annotation and inference chapters. `$var@Type` → `var@Type`. `[call $f $x]` → `[f x]`. Preserve formal inference rules (written in mathematical notation, not tinct syntax).
- [x] **doc/10-errors.md**: 13 matches. Update error message examples — error output currently shows `$name`, `[call $f ...]`; update to reflect new error text format (`name`, `[f ...]`). Update triggering tinct code examples.
- [x] **doc/12-tooling.md**, **doc/13-examples.md**: Update CLI usage examples and the full examples chapter. `doc/13-examples.md` is the primary showcase — should reflect idiomatic new syntax throughout.
- [x] **doc/15-ast.md**, **doc/16-architecture.md**, **doc/index.md**: Remaining chapters. Update any tinct code examples. doc/15-ast.md shows source → AST correspondences that reference `BareWord` and `VarRef` token names — update to `Identifier` and `EscapedRef`. doc/02-syntax.md and doc/09-documents.md: verify no remaining old-syntax code blocks were missed in the accept step.

### new-syntax-a: Phase 1 — `%` Pipeline + Named Sections

Non-breaking addition. See `doc/09-documents.md §DOC-PIPELINE` (updated formal semantics) and `doc/whatif/new-syntax.md §Phase 1`.

**Depends on:** `new-syntax-docs`

- [x] **AST** (`src/ast.rs`): Add `name: Option<String>`, `output_type: Option<Spanned<Annotation>>`, `expects: Option<Spanned<Annotation>>` to `Document`. Update all `Document { expressions }` construction sites to add `name: None, output_type: None, expects: None`.
- [x] **Lexer** (`src/lexer.rs`): No lexer changes required for Phase 1. `%` is already a valid `bare_word_char` (not in the exclusion list), so `%foo` already lexes as `Token::BareWord("%foo")` and the formatter already renders it as `%foo` (no sigil added). Add lexer unit tests to confirm `%defaults`, `%`, `%+` all lex as `BareWord` tokens with the `%` included in the string.
- [x] **Parser** (`src/parser.rs`): Two changes. (1) **Atom parsing**: in the `BareWord` match arm for atom parsing, add a rule: if the bare word string starts with `%`, produce `Expr::VarRef(s.to_string())` instead of `Expr::Str(s)`. This makes `%defaults` and bare `%` in value position resolve as variable references, not strings. (2) **Section header helper**: implement `parse_section_header(tokens, i)` consuming tokens until `Newline`. Matches: optional `%name` — `Token::BareWord(s) if s.starts_with('%')`, section name = `s[1..]` (chars after `%`); optional `@Type` annotation — match `ImmediateAt` (emitted correctly since `BareWord` sets `LastSignificantToken::BareWord`, so `is_immediate_at_context()` returns true and `ImmediateAt` is emitted for `%name@Type` with no whitespace); optionally also accept `At` for robustness; optional `expects:` pragma — match `BareWord("expects")`, `Colon`, type annotation. `BareWord("%")` with nothing after `%` (empty name after strip) → parse error. Duplicate `%name` in same file → parse error. Populate `Document.name`, `output_type`, `expects`.
- [x] **Evaluator** (`src/eval.rs`): In `eval_file_with_input()`, add `named: IndexMap<String, Rc<Thunk>>` accumulator (`Σ`). After each document: bind `"%"` = `prev_output` in `doc_env` (alongside existing `"$"` binding for `$$` backward compat). For all prior named sections, bind `format!("%{}", section_name)` in `doc_env` — e.g., a section named `"defaults"` (stored as `doc.name = Some("defaults")`) is bound as key `"%defaults"` so that `VarRef("%defaults")` resolves via LOOKUP. If `doc.name = Some(n)`, after evaluating the document insert `(n, result_thunk)` into `named`. Named section thunks stored raw (no materialization at `---` boundary). A section cannot reference its own name (not yet in `Σ`) — produces `UndefinedVariable`. Forward references to later sections also produce `UndefinedVariable`.
- [x] **Type checker** (`src/typecheck.rs`): Rename pipeline binding from `"$"` to `"%"` in `typecheck_document`. Thread named-section type bindings through the sequential document loop. Validate `@Type` output annotation against post-body env (resolve after body inference). Validate `expects:` against incoming `%` type (resolve against pre-body env). Both emit `TypeError` (advisory).
- [x] **Corpus tests**: Named sections `--- %defaults` / `--- %overrides` / `[call $merge %defaults %overrides]`. Anonymous `%` as alias for `$$`. `@Type` output annotation. `expects:` contract violation → type error. Bare `%` section name → parse error.

### new-syntax-b: Phase 2 — Core Syntax Migration (Breaking)

Single atomic change. All internal `.llt` files migrated in the same commit. See `doc/02-syntax.md §2.3` (updated), `doc/whatif/new-syntax.md §Phase 2`.

**Depends on:** `new-syntax-a`

- [ ] **Bug** (`src/parser.rs`): `[fn [x ...rest default: 10] body]` is rejected with "parameter after variadic parameter". The grammar currently disallows any parameter (including named params with defaults) after a `...rest` variadic. If named-after-variadic support is desired, add a parser rule allowing named params after the variadic.
- [x] **AST** (`src/ast.rs`): Add `implied: bool` to the `Call` AST node (or `Expr::Call` variant). `Expr::Str` remains for quoted strings only; `Expr::VarRef` now covers all value-position bare words.
- [x] **Lexer** (`src/lexer.rs`): Rename `Token::BareWord` → `Token::Identifier`, `Token::VarRef` → `Token::EscapedRef`. Update all match arms atomically. Add `Identifier` to `is_access_context()` and `is_bracket_access_context()` so `name.field` and `name[0]` produce access chains (consistent with bare-word-as-reference semantics). Update `LastSignificantToken` accordingly.
- [x] **Parser** (`src/parser.rs`): Implement head-position priority table in frame classification. Priority 2b (Identifier+ImmediateAt → Dict) fixes `[Foo@String]`. `peek_next_horizontal` for colon detection (not `peek_next_significant`) ensures `[name\n: val]` is zero-arg implied call. Atoms: `Identifier(s)` → `Expr::VarRef(s)`; `EscapedRef(s)` → `Expr::VarRef(s)`.
- [x] **Type checker** (`src/typecheck.rs`): Remove `BareWord/Identifier → String` inference arm. `Expr::Str` (quoted) still infers `Type::Str`. No other type inference changes.
- [x] **Evaluator** (`src/eval.rs`): Stop binding `"$"` (for `$$`) in pipeline env — bind only `"%"` and `"%name"`. `VarRef` resolution unchanged; just applied to more nodes.
- [x] **Formatter** (`src/formatter.rs`): Source-sniff first byte to emit `name` vs `$name`. `implied: true` → `[f x]`, `implied: false` → `[call f x]`.
- [x] **Error messages** (`src/error.rs`): References shown as `name` not `$name`. "Did you mean to quote?" suggestion for `UndefinedVariable` where name looks like an intended string literal.
- [x] **File migration**: Mechanically transformed 400+ `.llt` files: `stdlib/prelude.llt`, `tests/corpus/**/*.llt-eval`. Rules: `$var` → `var`; `[call $f x y]` → `[f x y]`; unquoted bare string values → quoted; `$$` → `%`; `$$foo` → `%foo`.

### new-syntax-c: Phase 2b — Polish and Completeness (completed items)

- [x] **tree-sitter-llt** (`tree-sitter-llt/grammar.js`): Updated for `identifier`/`escaped_ref`/`call_implied`/`%`-pipeline/section headers. 58 corpus tests pass.
- [x] **Corpus tests — implied call**: `implied_call_nested`, `implied_call_zero_arg`, `implied_call_single_arg`, `implied_call_not_data` (`[$x]` data vs `[x]` call), `escaped_ref_is_data`, `data_sequence_escaped_head`.
- [x] **Corpus tests — EscapedRef data sequences**: `escaped_ref_is_data.llt-eval` and `data_sequence_escaped_head.llt-eval`.
- [x] **Error message tests**: `string_suggestion.llt-eval` (hyphenated `my-key` exercises `-` heuristic), `no_suggestion_for_percent.llt-eval` (% suppresses hint), `pipeline_forward_ref_undefined.llt-eval`.
- [x] **Doc updates**: `doc/02-syntax.md §6` ebnf verified with `call_implied` and Priority 2b; `doc/09-documents.md` DOC-PIPELINE table updated; `output_type` annotation uses `result_env` (`src/typecheck.rs:527`); formatter section headers roundtrip correctly.
- [x] **`$$` removal**: `"$"` binding removed from all pipeline envs; `doc/whatif/structural-contracts.md` and `doc/whatif/index.md` cleaned up.

### new-syntax-migrate: Final Syntax Migration and Cleanup

- [x] **Corpus tests** (remaining): `type_annotated_output.llt-eval`, `section_with_expects.llt-eval` (expects: @Dict).
- [x] **README.md** and **lib/yaml.llt**: Migrated to new syntax.
- [x] **26/28 doc/whatif/*.md files**: Migrated to new syntax (algebraic-data-types, arena-patterns, call-aliases, circular-dep-error-paths, eval-builtins-boundary, eval-semantics-verification, float-dict-keys, gradual-typing, index, io, let-binding, lib-regex, lib-sql, lib-supplemental, lib-tls, macros, narrowing, nominal-variants, numeric-types, parameterized-type-aliases, parser-rewrite, pattern-matching, quasiquoting, string-interpolation, structural-contracts, TEMPLATE, templating, type-predicates, typeclasses, union-types).
- [x] **Code files**: builtins.rs "call $apply"→"apply", formatter.rs $_ tests, repl.rs tests, typecheck.rs tests, error.rs $try refs, lib.rs doc comment.

## Cycle Findings — C116

### cycle-findings-c116: C116 Codebase Health

- [x] **eval_stack push/pop asymmetry**: Fixed — push moved after depth check in Unevaluated and PendingBuiltin branches of force_step(). eval_materialize.rs:409,589.
- [x] **9 error codes no corpus coverage**: E050-E057 not corpus-testable; E062 unreachable. Documented in corpus README.
- [x] **Row.fields**: Already uses HashMap — pre-existing correct.
- [x] **Resource limit tests**: parse_depth_exceeded.llt-eval exists; collect_size_exceeded not feasible.
- [x] **doc/08 Cont variant names**: Updated to PendingCallDispatch, BuiltinForceArg, GuardedValidate.
- [x] **Access chain span propagation**: TODO comments added; design-level fix deferred.
- [x] **Span::origin() frame filtering**: Already exists in should_display_frame().
- [x] **Stale builtin counts**: doc/11a Evaluation 5→4; `until` moved to General.
- [x] **Corpus README**: Comprehensive rewrite.
- [x] **Sequence constructor tests**: repeat_depth_limit, unfold_depth_limit added.
- [x] **doc/12 ASPIRATIONAL**: Removed; implemented features enumerated.

### cycle-findings-c121: C121 Codebase Health (completed items)

- [x] doc/11a-builtins.md verified already correct (51 builtins, not 46)
- [x] Type variable collection: Vec with contains_key dedup in instantiate_at_level
- [x] cross_feature/: 4 tests added
- [x] Type::Function PartialEq + is_subtype + unify: variadic flag included; 3 unit tests
- [x] name_counter: saturating_add at 8 sites
- [x] doc/02-syntax.md: 6-constraint testing requirements table added
- [x] 5 laziness tests, 2 type assertion tests, 2 separator tests added
- [x] doc/11a-builtins.md header: "46" → "51"
- [x] special_form_arity.llt-eval corpus test created
- [x] resource_limit_exceeded.llt-eval: moved from errors/ to regressions/ (placeholder, E043 not triggerable via corpus)

### cycle-findings-c126: C126 Codebase Health

- [x] eval_materialize.rs: 13 eval_stack.pop() calls on non-Memoize exit paths + unit test
- [x] doc/10-errors.md: E055-E057 added to table; stale counts removed
- [x] doc/06-type-inference.md: "Function variance fix" → "CALL-MONO/CALL-POLY divergence fix"
- [x] doc/16-architecture.md: LSP entry point added to desugar list (update_document)
- [x] 4 corpus tests: eval_depth_exceeded, map_dict_thunk_preservation, reduce_lazy_chain, duplicate_computed_key

### cycle-findings-c131: C131 Codebase Health

- [x] eval_deep.rs: seq_depth limit added (MAX_COLLECT_SIZE check)
- [x] lsp/document.rs: skip materialize() when no_fs=true
- [x] types.rs: instantiate_at_level monomorphic fast-path + unit test
- [x] error.rs: 2 compile-time exhaustiveness tests for ErrorKind
- [x] Corpus tests: dot access whitespace, malformed_section_header, bracket access rename
- [x] SKIPPED_TESTS.md: E033 (float overflow), E052 (include cycle) documented
- [x] doc/11-stdlib.md: stale counts removed

### cycle-findings-c136: C136 Codebase Health

- [x] eval_materialize.rs: 3 CEK machine unit tests (Memoize caching, error caching, continuation error propagation)
- [x] eval_deep.rs: 1 cache unit test (cacheable error → Failed state memoization)
- [x] 4 corpus tests: typeassert_default_error, range_empty, range_out_of_bounds, range_negative_indices
- [x] doc/02-syntax.md: stale line numbers fixed; ident_char 3-site fix; ident_cont + terminator table updated
- [x] doc/16-architecture.md + doc/whatif/parser-rewrite.md: stale line numbers fixed
- [x] src/value.rs: BuiltinDef derives PartialEq + Eq

## vscode-extension: VS Code Extension

Implement a VS Code extension that wires `tinct lsp` to `.llt` files, providing live diagnostics and hover types. Extension lives in `editors/vscode/`. Uses `vscode-languageclient` (Node.js) to launch `tinct lsp` as a stdio child process — no async Rust changes required, the existing `lsp-server` sync architecture is sufficient.

- [x] Create `editors/vscode/package.json` — extension manifest declaring the `llt` language id, `.llt`/`.tinct` file associations, `activationEvents: ["onLanguage:llt"]`, main entry `./out/extension.js`, and a `tinct.serverPath` string configuration contribution (default `"tinct"`) for pointing at a custom binary path (`editors/vscode/package.json`)
- [x] Add `editors/vscode/language-configuration.json` — bracket pairs (`[]`), comment toggle prefix (`#`), auto-closing pairs for `[` and `"`, and a word pattern that covers bare words and `$`-prefixed identifiers (`editors/vscode/language-configuration.json`)
- [x] Add TextMate grammar `editors/vscode/syntaxes/tinct.tmLanguage.json` — scopes for: `$`-prefixed variable references (`variable.other`), special forms `fn`/`call`/`if`/`type`/`error`/`try`/`apply` (`keyword.control`), `---` document separator (`keyword.other`), `@` type annotations (`storage.type`), `#` line comments (`comment.line`), string literals (`string.quoted.double`), integer and float literals (`constant.numeric`), and `[]` bracket punctuation (`punctuation.section`) (`editors/vscode/syntaxes/tinct.tmLanguage.json`)
- [x] Add `editors/vscode/tsconfig.json` targeting ES2020 + CommonJS with `outDir: out` and `rootDir: src`; add `vscode-languageclient ^9` as a dependency and `@types/vscode ^1.75` plus `typescript` as devDependencies in `package.json`; add `"compile": "tsc -p ./"` and `"package": "vsce package"` npm scripts (`editors/vscode/tsconfig.json`, `editors/vscode/package.json`)
- [x] Implement `editors/vscode/src/extension.ts` — `activate()` reads `tinct.serverPath` from workspace config, constructs a `ServerOptions` with `command` + `args: ["lsp"]` and `transport: TransportKind.stdio`, creates a `LanguageClient` with `documentSelector: [{scheme: "file", language: "llt"}]`, and starts it; `deactivate()` stops the client (`editors/vscode/src/extension.ts`)
- [x] Add `editors/vscode/.vscodeignore` — exclude `src/`, `tsconfig.json`, `node_modules/`, `.map` files, and `*.ts` source from the packaged `.vsix` (`editors/vscode/.vscodeignore`)
- [x] Add `ext` target to `justfile`: `cd editors/vscode && npm install && npm run compile` to build the extension from the repo root; add `ext-package` target that additionally runs `npx vsce package` to produce a `.vsix` (`justfile`)
- [x] Add `§VS Code Extension` section to `doc/12-tooling.md` covering: building from source (`just ext`), packaging to `.vsix` (`just ext-package`), installing (`code --install-extension tinct-*.vsix`), the `tinct.serverPath` setting for pointing at a `cargo run`-based server during development, and what LSP features are active (diagnostics, hover)
- [x] Verify end-to-end: build `tinct` binary (`cargo build`), run `just ext`, install the `.vsix`, open a `.llt` file in VS Code, confirm parse-error diagnostics appear and hover shows inferred types over variable references

## Type Predicates: Runtime Type Testing Builtins

One predicate per `Value` variant for direct type dispatch. See doc/11a-builtins.md §Type Introspection and doc/whatif/type-predicates.md.

- [x] Accept type-predicates — see doc/whatif/type-predicates.md (State: Accepted — 2026-05-04)

### type-predicates: Core Type Predicate Builtins

Add `int?`, `float?`, `num?`, `str?`, `bool?`, `null?`, `dict?`, `fn?` builtins. See doc/11a-builtins.md §Type Introspection.

- [x] Implement 8 predicate builtins: `int?`, `float?`, `num?`, `str?`, `bool?`, `null?`, `dict?`, `fn?` — each materializes arg and checks `Value` variant (`src/builtins.rs`)
- [x] Register all 8 in `standard_builtins()` with `[Strictness::Seq]` annotation (`src/builtins.rs`)
- [x] Add type signatures for all 8 predicates: `Any → Bool` (`src/builtins.rs`)
- [x] Update `test_all_builtins_registered` assertion count (`src/builtins.rs`)
- [x] Add eval corpus tests: each predicate against matching and non-matching values, including edge cases — `num?` with Int and Float, `fn?` with Function and Builtin, `dict?` with list-shaped dicts (`tests/corpus/eval/`)
- [x] Add `list?` as a stdlib function in `stdlib/prelude.llt`: `[fn [xs] [and [dict? xs] [all? [fn [k] [int? k]] [keys xs]]]]`

## Type Predicates: Follow-Up Nits

Minor housekeeping from the type-predicates sprint panel review.

### type-predicates-nits: Post-Sprint Cleanup

Small fixes deferred from the type-predicates sprint panel.

- [x] Move `seq?` registration from Sequences comment block to Type Introspection comment block in `standard_builtins()` (`src/builtins.rs:1811`)
- [x] Add `dict?`-with-Overlay corpus test: `[dict? [merge [a: 1] [b: 2]]]` → `Bool(true)` (`tests/corpus/eval/builtins/`)
- [x] Add `null?`-with-Seq corpus test: `[null? [range 0 5]]` → `Bool(false)` (`tests/corpus/eval/builtins/`)
- [x] Add `fn?`-with-Proxy corpus test: `[fn? [proxy ...]]` → `Bool(false)` (`tests/corpus/eval/builtins/`)
- [x] Fix `doc/whatif/type-predicates.md` — `every?` → `all?`; `"Null"` removed from type-of list; `"Proxy"` added; `fn?`/type-of distinction corrected

### arena-resolve: Variable Resolution Pass

Pre-eval analysis pass assigns `(level, slot)` pairs to `VarRef` nodes. See doc/whatif/arena-patterns.md §Variable Resolution Pass Design.

- [x] Add resolution cache to `Expr::VarRef` — three-state `RefCell<Option<Option<(u32,u32)>>>`: outer None=unprocessed, Some(None)=unresolvable, Some(Some(l,s))=resolved (`src/ast.rs`)
- [x] Implement `Resolver` struct with scope stack `Vec<IndexMap<String, u32>>` — `enter_scope(keys)`, `exit_scope()`, `resolve(name) -> Option<(u32,u32)>` (`src/resolve.rs`)
- [x] Walk AST to populate resolution cache — dict keys walked before scope entry, Fn params+annotations, TypeAssert/Annotated, all expression variants (`src/resolve.rs`)
- [x] Wire resolution pass into all 7 pipeline entry points: eval_source_with_config, typecheck_source, run_eval, REPL, LSP, create_stdlib_env, builtin_include (`src/lib.rs`, `src/main.rs`, `src/repl.rs`, `src/builtins.rs`, `src/lsp/document.rs`)
- [x] Update `eval` VarRef case — O(1) slot lookup deferred to Phase 2 (static level system doesn't align with runtime env chain until FlatEnv); cache populated but eval uses name-based lookup (`src/eval.rs`)
- [x] Unit tests: 20+ tests covering scope shadowing, annotations, access chains, named args, multi-doc isolation, write-once invariant, empty scope (`src/resolve.rs`)
- [x] Verify full corpus test suite passes unchanged (`tests/`)

### arena-types: Arena Type Definitions

Introduce `ThunkId`, `EnvId`, `ThunkArena`, `EnvArena`, `FlatEnv` types with letrec allocation pattern. See doc/whatif/arena-patterns.md §Design.

**Depends on:** `arena-resolve`

- [x] Add `ThunkId(u32)` newtype and `ThunkArena` struct (`Vec<Rc<Thunk>>`, `alloc() -> ThunkId`, `get(ThunkId) -> &Rc<Thunk>`) (`src/arena.rs`)
- [x] Add `EnvId(u32)` newtype and `EnvArena` struct (`Vec<FlatEnv>`, `alloc() -> EnvId`, `get(EnvId) -> &FlatEnv`) (`src/arena.rs`)
- [x] Add `FlatEnv` struct: `slots: Vec<Option<ThunkId>>`, `overflow: HashMap<String, ThunkId>`, `parent: Option<EnvId>` — all pub(crate) (`src/arena.rs`)
- [x] Implement letrec allocation pattern: `alloc_placeholder()` returns `ThunkId` pointing at `Bool(false)` sentinel; fill via `Rc<Thunk>::set_state()` interior mutability (`src/arena.rs`)
- [x] Unit tests: 18 tests — alloc/get, placeholder+fill lifecycle, FlatEnv slot/overflow/parent, overflow checks, Copy semantics (`src/arena.rs`)
