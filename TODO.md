# Implementation Roadmap

Extracted from DESIGN.md. Tracks what's built, what's next, and what's deferred.

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

## Sequences and Fully Lazy Operations

The original design calls for everything to be lazy. Several operations are currently eager due to the `BuiltinFn` return type (`Value` instead of `Rc<Thunk>`) and the absence of lazy function application. This milestone adds `Value::Seq` for lazy computation and `PendingCall` thunk state to restore laziness across the language. See DESIGN.md "Sequences and Lazy Computation" for full design.

**Note:** This milestone can proceed in parallel with the Parser Rewrite since the parser is independent of runtime semantics.

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

- [ ] `$str`, `$split`, `$replace`, `$upper`, `$lower`, `$trim` -- must inspect string content
- [ ] `$join`, `$words` -- derived string ops, inherently materializing
- [ ] `$to-int`, `$to-float`, `$floor`, `$round` -- must convert
- [ ] `$type-of` -- must inspect value variant
- [ ] `$from-json` -- must parse entire JSON string

### doc-eagerness-control: Document Inherent Eagerness — Control Flow

Document why these control flow operations must materialize.

- [ ] `$eval` -- deep-forces all thunks by definition
- [ ] `$error` -- constructs error value (structural, but the error itself is concrete)
- [ ] `$try`, `$try-or` -- must materialize body to catch errors
- [ ] `$assert` -- must materialize condition to check
- [ ] `$any?`, `$all?` -- short-circuit but materializes elements until condition met/failed

### eval-correctness-2: Eval Correctness Fixes (Cycle 1 Review)

Correctness issues found by eval-engine and computer-scientist reviews (2026-04-20 cycle 1).

- [ ] Fix cycle detection leaving thunk in InProgress instead of Failed — after circular dependency error, thunk stays InProgress permanently; should transition to Failed for error caching and consistent subsequent access (`src/eval.rs:897-907`) [Major, eval-engine]
- [ ] Add named args support to PendingCall — `ThunkState::PendingCall` only stores positional args (`Vec<Rc<Thunk>>`); named args with defaults lost in lazy function application. Add `named: IndexMap<String, Rc<Thunk>>` field and thread through `materialize()` (`src/value.rs:186-190`, `src/eval.rs:983`) [Minor, eval-engine]
- [ ] Add origin parameter to `new_pending_builtin()` — always sets `origin: String::new()`, making builtin calls invisible in stack traces; inconsistent with `new_pending_call` which accepts explicit origin (`src/value.rs:228-246`) [Nit, eval-engine]

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


## Parser Rewrite (E2)

Replace pest's recursive descent with a hand-written lexer + iterative parser using an explicit stack. The pest parser stays as a reference implementation for comparison until the new parser graduates.

**Goal:** Identical AST output from both parsers, selectable at parse time. Once the new parser passes the full test suite and matches pest output on all corpus files, it becomes the default and pest is removed.

### lexer: Lexer (`src/lexer.rs`)

Tokenizer producing a flat token stream. Whitespace-sensitivity for access chains handled here.

- [ ] Token enum: OpenBracket, CloseBracket, Colon, Semicolon, Dot, Range, At, Ellipsis, DocSeparator, Newline, Comment(String), Int(i64), Float(f64), BareWord(String), QuotedString(String), VarRef(String), BoolLit(bool)
- [ ] Single-pass tokenization with source spans on every token
- [ ] Whitespace-sensitive access detection: Dot/OpenBracket immediately after VarRef or CloseBracket (no whitespace) emits access-context tokens
- [ ] Comment tokens (`#` to EOL, preserves text for formatter)
- [ ] Newline tokens (significant whitespace for blank line detection; consecutive Newlines encode blank lines)
- [ ] String escapes (`\"`, `\\`, `\n`, `\t`, `\r`)
- [ ] Bare word denylist matching grammar.pest rules
- [ ] CRLF line ending support: track line boundaries correctly for `\r\n` (pest and LSP convert.rs both assume `\n` only)
- [ ] Add inline comments to grammar.pest explaining why access_expr/access_chain are compound-atomic (`grammar.pest:137-148`) [Major, grammar-architect]
- [ ] Fix grammar.pest COMMENT rule misleading NEWLINE comment (`grammar.pest:8`) [Nit, grammar-architect]

### formatter: Formatter/Pretty-Printer (`tinct fmt`)

Uses the hand-written lexer's token stream (comment-preserving, unlike pest). See DESIGN.md §Formatter for full design.

- [x] Design formatting rules (single-line threshold, comment attachment, semicolon handling) — see DESIGN.md §Formatter
- [ ] Formatting engine (`src/formatter.rs`)
  - Indent nested `[]` by 2 spaces per depth
  - One entry per line (unless bracket expr fits within 80 chars AND has ≤4 entries)
  - Comments: line-affinity attachment (trailing = same line, leading = own line)
  - Semicolons always removed (canonical whitespace-separated style)
  - Consistent spacing: one space after `:`
  - `---` separators get blank lines above and below
  - Collapse multiple blank lines to one
  - Access chains never broken across lines
  - Strip trailing whitespace, ensure trailing newline
- [ ] CLI subcommand: `Fmt` variant with `--check`, `--in-place`, `--stdin` (zero config)
- [ ] Tests (idempotency, comment preservation, indentation, single-line vs multi-line, edge cases)
- [ ] Just recipes: `just fmt-llt FILE`, `just fmt-llt-check FILE`

### iterative-parser: Iterative parser (`src/parser2.rs`)

Explicit `Vec<StackFrame>` for bracket nesting. Atoms and access chains parsed without recursion.

- [ ] StackFrame enum: Dict, Call, Fn, TypeAlias, TypeAssert (one variant per bracket form)
- [ ] On `[`: push frame, determine form from first token (keyword detection)
- [ ] On `]`: pop frame, construct AST node
- [ ] Between brackets: parse atoms, access chains, annotations (all non-recursive)
- [ ] MAX_DEPTH check on `stack.len()` (policy, not safety)
- [ ] Static constraints: positional-before-named, duplicate keys, variadic rules
- [ ] Error messages with precise context ("expected value after `:`", "unclosed bracket at line 5")
- [ ] Document `$_` desugaring AST shape mismatch between type checker and evaluator — or implement shared desugaring pass (`src/eval.rs:70-74`, `src/typecheck.rs:137-209`) [Major, integration-verifier]
- [ ] Document type alias entries returning empty dict at runtime — add comment explaining compile-time-only behavior (`src/eval.rs:182-185`) [Minor, integration-verifier]

### include-refactor: IncludeContext Refactor

Refactor thread-local IncludeContext to parameter-passing for LSP multi-file support (integration-verifier review).

- [x] Design eval-stack parameter threading for IncludeContext — see DESIGN.md §EvalContext
- [ ] Replace thread-local `INCLUDE_CTX` with parameter passed through eval stack
- [ ] Enable LSP multi-file support where multiple files need separate include guards

### parser-integration: Integration

- [ ] `parse()` API accepts parser selection (enum or feature flag)
- [ ] Both parsers produce identical `Spanned<File>` output
- [ ] Comparison test: parse every corpus file with both parsers, assert AST equality
- [ ] Benchmark: compare parse time on large inputs

### parser-graduation: Graduation criteria

- [ ] Full test suite passes (all unit + corpus tests)
- [ ] AST output matches pest parser on every corpus file
- [ ] Error messages are equal or better quality
- [ ] No stack overflow on any nesting depth up to MAX_DEPTH

### parser-cleanup: Cleanup (post-graduation)

- [ ] Remove `pest` and `pest_derive` dependencies from Cargo.toml
- [ ] Remove `src/grammar.pest`
- [ ] Remove pest-specific code from `src/parser.rs`
- [ ] Remove pest-specific test code and helpers from `src/parser.rs`
- [ ] Rename `src/parser2.rs` to `src/parser.rs`
- [ ] Update CLAUDE.md, README.md, SPEC.md references (remove pest notation, update grammar description)
- [ ] Full pest removal audit: verify no remaining pest references in docs, tests, or comments

## Performance: Stdlib Rust Reimplementations

Nearly all accumulator-based stdlib functions are O(n^2) due to `merge`/`append` materializing and cloning the growing accumulator IndexMap on every iteration. Sort is O(n^2 log n) because `sort-merge` uses `cons` (O(n)) per element.

**Note:** The Sequences milestone addresses much of this by moving `map`, `filter`, `range`, `take`, `drop`, `reduce` to Rust builtins with lazy dispatch. The items below track remaining performance work not covered by that milestone.

Remaining Rust reimplementations (all currently in `stdlib/prelude.llt`):
- [ ] `rest`, `cons`, `conj`, `concat`, `reverse` -- list primitives, used by sort (O(n) each due to cloning)
- [ ] `sort`, `sort-by` / `sort-merge` -- single Rust builtin using Vec::sort_by would be O(n log n) (laziness-auditor review: $sort uses eager $cons per element)
- [ ] `zip`, `flatten`, `find-deep` -- recursive traversal or lazy seq versions for perf

## perf-foundations: Performance Foundations

Performance improvements identified by performance-expert review (2026-04-19) that don't depend on the Parser Rewrite.

- [x] Design allocation strategy (arena vs Rc, flat env vs chain) — see DESIGN.md §Allocation Strategy — Phased Approach
- [ ] Arena allocation for thunks/environments — reduce per-thunk Rc<RefCell<ThunkState>> overhead, improve cache locality
- [ ] Flat environment with slot indices — replace O(n) chain walk with O(1) slot lookup (requires compile-time slot assignment)
- [ ] String interning for dict keys and small strings — reduce allocation pressure, improve key comparison
- [ ] Path-compressed union-find for type substitutions — make Substitution::apply() O(α(n)) amortized instead of O(size)
- [ ] Dict literal fast-path in eval_dict — skip Unevaluated thunk for `Int | Float | Bool | Str` literals, create Materialized directly (Nix `maybeThunk` optimization, ~40-60% fewer thunk allocations for config-heavy files) [Major, eval-engine + performance-expert]
- [ ] Key cloning reduction in eval_dict — string keys cloned 2× per entry (once into dict_env bindings, once into dict_map). Use entry_mut() pattern or restructure insert order. ~30% of dict allocation cost. (`src/eval.rs:346-352`) [Major, performance-expert, design-review]
- [ ] func_label allocation reduction — `format!("${name}")` on every PendingCall creation → `Cow<'static, str>` for VarRef case (most common), only allocate for DotAccess. ~5-10% call overhead. (`src/eval.rs:387-396`) [Minor, performance-expert, design-review]
- [ ] Capacity hints for hot-path allocations — `IndexMap::with_capacity(entries.len())` in dict construction, `String::with_capacity()` in `builtin_str` [Minor, performance-expert]
- [ ] Reduce bind_args_thunks allocation — deferred to iterative-eval: env reuse is unsafe in current `Rc<RefCell<Environment>>` model (recursive calls share closure_env, param bindings leak between calls). Flat environments in Phase 2 make reuse trivially safe. (`src/eval.rs:527-529`) [Major, performance-expert, deferred]
- [ ] Bounded Display depth for Value — limit `Value::Dict` Display impl to max 3 levels, print `...` beyond that to prevent deep traversal in error messages (`src/value.rs:113-143`) [Minor, performance-expert]
- [ ] Document performance characteristics in DESIGN.md — Environment O(depth) lookup, IndexMap ~20% vs HashMap, thunk triple-boxing cost, Substitution::apply() tree walk cost [Minor, performance-expert]
- [ ] Add per-variable depth limit to Substitution::apply() — long chains >256 types silently return start of chain; either raise error or document truncation (`src/types.rs:141-144`) [Critical, type-theorist]
- [ ] Document Environment DAG invariant — add doc comment and debug-mode cycle detector (`src/value.rs:333-392`) [Major, eval-engine]
- [ ] Cache four-pass dict inference key resolution — `infer_expr` resolves keys twice across passes (`src/typecheck.rs:272-295`) [Minor, type-theorist]
- [ ] Add clarifying comment to `bind_args_thunks` double conflict check (`src/eval.rs:573-587`) [Nit, eval-engine]
- [ ] Consider `matches!` instead of `!=` in `key_in_range` (`src/eval.rs:22-50`) [Nit, eval-engine]
- [ ] Extract `MAX_APPLY_DEPTH` constant to shared location — duplicated in `src/types.rs:127` and `src/eval.rs:42` [Nit, performance-expert]
- [ ] Avoid AST clone in eval_call argument thunk creation — change `CallExpr` args to `Rc<Spanned<Expr>>` so eval_call does `Rc::clone` instead of deep-cloning AST subtrees per argument. Internal refactor to ast.rs/parser.rs, backward-compatible at public API. ~20-40% call overhead reduction. (`src/eval.rs:416-435`) [Major, performance-expert, design-review promoted]
- [ ] Avoid intermediate Vec in value_to_display_string — collects all formatted entries into Vec<String> then joins; write directly to String with_capacity instead (`src/lib.rs:194-204`) [Minor, performance-expert]
- [ ] Avoid intermediate Vec in builtin_split — `input.split(sep).collect::<Vec<&str>>()` then maps to thunks; use iterator directly (`src/builtins.rs:525-535`) [Nit, performance-expert]
- [ ] Cache materialized dict in `builtin_cycle_step` — currently re-materializes immutable dict on every iteration; store IndexMap directly (`src/builtins.rs:1375-1443`) [Major, performance-expert]
- [ ] Optimize `builtin_take` Dict path — materializes entire dict then clones first n entries; use `map.iter().take(n)` directly (`src/builtins.rs:1648-1657`) [Major, performance-expert]
- [ ] Consider SmallVec for sequence constructor tail args — `Vec::new()` + push allocates heap on every step of infinite sequences; SmallVec<[Rc<Thunk>; 2]> would stack-allocate common cases (`src/builtins.rs`) [Minor, performance-expert]
- [ ] SmallVec for eval_call positional args — most calls have ≤4 args; SmallVec<[Rc<Thunk>; 4]> avoids heap allocation for common case (`src/eval.rs:417-426`) [Minor, performance-expert]
- [ ] SmallVec for error stack frames — SmallVec<[StackFrame; 8]> for shallow stacks (`src/error.rs`) [Minor, performance-expert]
- [ ] Thunk origin String→Option<Rc<str>> — most thunks have empty origin; eliminates per-thunk allocation, share origin strings via Rc (`src/value.rs:204,216`) [Nit, performance-expert]
- [ ] Add capacity hint to variadic dict allocation — exact size known at allocation time (`src/eval.rs:610`) [Minor, performance-expert]
- [ ] Use static empty IndexMap for PendingCall named args — eliminates allocation on every PendingCall with no named args (`src/eval.rs:976`) [Nit, performance-expert]
- [ ] Use static empty dict thunk for default `$$` — eliminates allocation on every file eval without stdin (`src/eval.rs:287-291`) [Nit, performance-expert]

## iterative-eval: Iterative Evaluator

Replace the recursive `eval()` / `materialize()` call stack with an explicit continuation stack (stack machine). Nix, Nickel, and Jsonnet all use iterative evaluation with explicit frame types. LLT's recursive approach risks stack overflow on deeply-nested lazy chains and prevents tail-call optimization.

- [x] Design `Frame` enum for explicit continuation stack — see DESIGN.md §Iterative Evaluator — Defunctionalized CPS (CEK Machine). Uses `Action` enum (Eval/Materialize/Continue) + `Cont` enum (~18-20 defunctionalized continuation variants, boxed large fields for ≤96B frames) in an iterative two-register loop. Agent-reviewed: eval-engine, laziness-auditor, performance-expert.
- [ ] Research safe Rust arena patterns for thunks/environments — typed-arena, bumpalo, index-based arenas (Vec<Thunk> + ThunkId handles), and how to handle letrec self-reference without dangling pointers. Study how Rust projects (salsa, rustc's ty::TyCtxt, cranelift) solve arena + interior references. Assess whether GhostCell or similar can replace RefCell for environment mutation.
- [ ] Design arena lifetime policy for REPL/LSP — REPL accumulates session env across inputs (thunks from previous arenas must survive); LSP persists DocumentState across edits. Options: session-scoped arena with compaction, copy-out on persist (deep-materialize escaping values), or hybrid (arena for eval-local, Rc for persistent). See DESIGN.md §Allocation Strategy.
- [ ] Environment reuse in bind_args_thunks — safe with flat environments (each call writes to own activation frame). Deferred from perf-foundations where it was unsafe with shared `Rc<RefCell<Environment>>`. (`src/eval.rs:527-529`)
- [ ] Convert `materialize()` from recursive to iterative with `Vec<Frame>` work stack
- [ ] Convert `eval()` hot paths (dict construction, access chains) to iterative
- [ ] Implement tail-call optimization (TCO) for `call` expressions — detect tail position, reuse frame
- [ ] TCO for recursive stdlib functions (`fold`, `map`, `filter`, `sort-merge`) to avoid stack overflow on large inputs
- [ ] Benchmark: compare recursive vs iterative on deep chains and large collections
- [ ] Remove 64MB worker thread stack workaround once iterative eval eliminates deep recursion

## row-unification: Full Row-Variable Unification (Remy-Style)

Replace the current closed-strict/open-lenient record unification with full Remy-style row-variable unification. Row variables become first-class participants in type inference, enabling the type checker to infer record extension and restriction through polymorphic function boundaries.

**Depends on:** seq-core for sequence type support in row polymorphism.

- [ ] Design Remy-style row unification model (row variable binding, remainder semantics, occurs check)
- [ ] Add VarLevel (integer rank) tracking to type variables for sound let-polymorphism generalization (Elm's rank-based approach — determines which type variables can be generalized at `let` boundaries)
- [ ] Extend `Substitution::apply` to splice bound row variable fields into records (e.g., `[a: Int | ...r]` with `r → [b: String]` produces `[a: Int, b: String]`)
- [ ] Unify row rests: `RowVar` vs `RowVar` binds one to the other, `RowVar` vs `Closed` binds the var to the leftover fields as a closed record
- [ ] Handle "remainder" binding: `unify([a: Int | ...r], [a: Int, b: String | Closed])` binds `r → [b: String | Closed]`
- [ ] Extend `Type` representation if needed to support partial-row bindings (row var bound to fields + another row var)
- [ ] Update `instantiate` to freshen row variables alongside type variables
- [ ] Test inference through polymorphic functions that extend/restrict records (e.g., `[fn add-id [r@[...rest]] [id: 1  ...rest]]`)
- [ ] Verify consistency between `unify` and `is_subtype` for all RowRest combinations
- [ ] Add row-specific occurs check for `RowVar("r")` with `Record(..., RowVar("r"))` (infinite row type prevention)
- [ ] Add row variable substitution cycle handling — `Substitution::apply` must handle cycles when row variables bind to records containing the same row variable (`src/types.rs`) [Major, type-theorist]
- [ ] Add VarLevel scope tracking for row variables (Elm rank-based generalization — essential for row-unification soundness, type-theorist review)
- [ ] Subtyping proof search for TypeAssert defaults — validate default value type matches asserted type (type-theorist review)
- [ ] Fix row variable substitution creating duplicate fields — `merged.extend(extra_fields)` doesn't check for key collisions (`src/types.rs:166-184`) [Critical, type-theorist]
- [ ] Fix RowVar treated identically to Open in `is_subtype` — add TODO comment explaining row-unification placeholder (`src/types.rs:59-62`) [Major, type-theorist]

## sandbox: Sandboxing & Security

Design and implement four unprivileged sandboxing layers. See DESIGN.md §Sandboxing & Security for full design.

- [x] Design sandboxing model — see DESIGN.md §Sandboxing & Security
- [x] Decide policy for absolute paths — allowed if within any --allow-path
- [x] Decide policy for symlinks — canonicalize, then check against allowlist
- [ ] Implement filesystem allowlist in `EvalConfig` (depends on include-refactor)
- [ ] Add path-ancestor allowlist check in `builtin_include` (after canonicalize, before cache)
- [ ] Add `--allow-path` global CLI flag (default: `.`)
- [ ] Implement Landlock filesystem ACLs (Linux 5.13+, graceful degradation)
- [ ] Implement seccomp-bpf network sandbox (block socket/connect/bind/listen/accept)
- [ ] Implement seccomp-bpf process sandbox (block fork/execve/execveat, allow clone)
- [ ] Implement rlimit resource caps (RLIMIT_AS, RLIMIT_CPU eval-only, RLIMIT_NOFILE, RLIMIT_FSIZE)
- [ ] Add `--allow-network`, `--max-memory`, `--max-cpu`, `--max-fds` global CLI flags
- [ ] Test: relative paths within allowlist succeed
- [ ] Test: `../` traversal beyond allowlist fails
- [ ] Test: absolute paths outside allowlist fail
- [ ] Test: symlinks pointing outside allowlist fail
- [ ] Test: graceful degradation when Landlock/seccomp unavailable

## type-extensions: Type System Extensions

Future type system work identified by type-theorist review (2026-04-19).

- [ ] Design type system extension roadmap (Seq types, gradual typing, type classes, error recovery)
- [ ] Add Type::Seq inference to typecheck.rs — sequence builtins ($seq, $range, $repeat, $cycle, $iterate, $unfold, $take) currently infer as Any; annotate return types in `check_call` for LSP hover and type safety (`src/typecheck.rs`) [Major, type-theorist]
- [ ] Fix polymorphic call unification for named args — requires extending `Type::Function` to carry param names; after positional unification, named args are not unified (`src/typecheck.rs:389-447`) [Major, type-theorist, deferred from types-major-fixes]
- [ ] Gradual typing with Any→concrete boundary tracking and blame (TypeScript/Typed Racket model)
- [ ] Polymorphic recursion detection — forbid or support with depth-based instantiation tracking
- [ ] Type error recovery with `Type::Error` sentinel that doesn't unify (prevents cascading errors, improves LSP)
- [ ] Type class constraints for arithmetic/comparison (needed if user-defined types get custom operators)
- [ ] Type environment alias registry shadowing policy — either forbid shadowing or document that aliases follow lexical scope (`src/types.rs:433-435`) [Major, type-theorist]
- [ ] `type-of` returns "Dict" for all dicts, no list discrimination — document in Future Features (`src/builtins.rs`, `DESIGN.md:1710`) [Minor, stdlib-author]
- [ ] Fix type display for empty open record — `[...]` is ambiguous, consider `[... (open)]` notation (`src/types.rs:359`) [Minor, type-theorist]
- [ ] Make `TypeEnv::lookup` `pub(crate)` — currently private but useful for testing (`src/types.rs:415-427`) [Minor, type-theorist]
- [ ] Document `Substitution::get` being `cfg(test)` only — either make always-public or explain opaqueness (`src/types.rs:198-202`) [Minor, type-theorist]
- [ ] Fix instantiation counter overflow — `u32` theoretically overflows; use `u64` or document assumption (`src/types.rs:318-330`) [Minor, type-theorist]
- [ ] Document `Type::Number` having no literal variant — asymmetry with Int/String is due to dict key constraint (`src/types.rs:21-37`) [Minor, type-theorist]
- [ ] Fix `Type::Function` Display for nested types — add parentheses for nested function annotations (`src/types.rs:369-378`) [Minor, type-theorist]
- [ ] Validate variadic param type annotations in type checker — either forbid or assign typed value (`src/typecheck.rs:456-478`) [Minor, type-theorist]
- [ ] Clarify `resolve_annotated` interpreting all Fn annotations as function types (`src/typecheck.rs:522-533`) [Minor, type-theorist]
- [ ] Populate type map on errors — record `Type::Any` for failed subexpressions to improve LSP hover (`src/typecheck.rs:200-206`) [Minor, type-theorist]
- [ ] Consider `HashSet` instead of `BTreeSet` in `collect_type_vars` — order doesn't matter (`src/types.rs:85-106`) [Nit, type-theorist]
- [ ] Remove unused `Substitution` from `instantiate` return type — or document why returned (`src/types.rs:318-330`) [Nit, type-theorist]
- [ ] Document `Type::is_subtype` not short-circuiting on `Any` in nested positions (`src/types.rs:42-83`) [Nit, type-theorist]
- [ ] Fix type display using two spaces between fields — consider single space (`src/types.rs:345-367`) [Nit, type-theorist]
- [ ] Add comment explaining `IntLiteral(n) ~ Float` literal-specific promotion (`src/types.rs:263`) [Nit, type-theorist]
- [ ] Fix `TypeEnv::with_parent` taking `Rc` instead of `&Rc` — minor API ergonomics (`src/types.rs:399-405`) [Nit, type-theorist]
- [ ] Add `Eq` derive to `TypeError` (`src/types.rs:444-448`) [Nit, type-theorist]
- [ ] Document `TypeMap` using `(offset, offset)` as key instead of `Span` — offsets are sufficient (`src/typecheck.rs:16`) [Nit, type-theorist]
- [ ] Consider `Result<Type, TypeError>` for `infer_expr` match arms — most wrap single error in vec (`src/typecheck.rs:142-209`) [Nit, type-theorist]
- [ ] Document `check_call` not verifying named args exist in params — intentional: named args are eval-time (`src/typecheck.rs:389-447`) [Nit, type-theorist]
- [ ] Consider `HashMap` instead of `IndexMap` for type alias registry — order doesn't matter (`src/types.rs:386`) [Nit, type-theorist]
- [ ] Clarify `Fn@T` with zero params — document whether it means thunk or nullary function (`src/typecheck.rs:536-541`) [Nit, type-theorist]
- [ ] Fix monomorphic function calls skipping argument type checking — `!func_ty.has_type_vars()` early return bypasses argument-parameter type unification; monomorphic calls should still verify argument types match parameters (`src/typecheck.rs:421-422`) [Major, computer-scientist]
- [ ] Fix open-record unification silently dropping non-shared fields — only shared fields unified, unique fields ignored without constraint; Remy-style would bind fresh row variables to capture remainders (`src/types.rs:334-338`) [Major, computer-scientist]
- [ ] Fix letrec forward-reference typing to Any — single-pass dict inference binds forward refs to `Type::Any` instead of fixpoint iteration (Mycroft 1984); masks type errors in mutually recursive dict entries (`src/typecheck.rs:225`) [Minor, computer-scientist]
- [ ] Document literal promotion symmetry in unification — `IntLiteral↔Int` unification is bidirectional; in a subtyping-aware system `IntLiteral <: Int` but not vice versa; reduces diagnostic value (`src/types.rs:263-264`) [Minor, computer-scientist]

## Stdlib Expansion

Missing functions identified by cross-language analysis (Jsonnet, jq, Nix, Dhall). All implementable in LLT unless noted.

### stdlib-missing-core: Core Missing Functions

- [ ] `with-entries` — `entries | map(f) | from-entries` pipeline (jq pattern; depends on `from-entries` from stdlib-pre-seq)
- [ ] `partition` — single-pass split into matching/non-matching dicts (Nix + Dhall)
- [ ] `flat-map` / `concat-map` — `flatten (map f xs)`, monadic bind for collections (Jsonnet + jq)
- [ ] `find-first` / `find-first-or` — first element matching predicate, with default (Nix)
- [ ] `group-by` — group elements by key function, returning dict of lists (Nix)
- [ ] `deep-merge` — recursive merge for configuration overlays (Jsonnet, RFC 7396)
- [ ] `walk` — recursive bottom-up transform of all sub-values (jq)

### stdlib-convenience: Convenience Functions

- [ ] `sum`, `min`, `max`, `count` — aggregate functions (one-liners over fold)
- [ ] `contains?` / `elem?` — membership test
- [ ] `uniq` / `unique` — deduplicate collection
- [ ] `foldr` — right fold (LLT only has left fold currently)
- [ ] `zip-with` — generalized zip with combining function; define `zip` as special case (Nix)
- [ ] `map-indexed` / `map-keys` — indexed mapping and key transformation (Jsonnet)
- [ ] `sort-on` — sort by key-extraction function instead of comparator (Jsonnet + Nix)
- [ ] `flip`, `abs`, `sign`, `clamp` — small composable primitives (Nix + Jsonnet; `const` moved to stdlib-pre-seq)

### stdlib-type-predicates: Type Predicates & Guards

- [ ] `is-int?`, `is-str?`, `is-float?`, `is-bool?`, `is-dict?`, `is-fn?` — type predicate wrappers over `$type-of` (Jsonnet pattern)
- [ ] Runtime assertion guards at stdlib function entry with descriptive errors (Jsonnet pattern)

### stdlib-numeric: Numeric Utilities

- [ ] `min`, `max`, `sum`, `product` — aggregate functions (stdlib-author review)
- [ ] `abs`, `sign`, `clamp` — numeric primitives (stdlib-author review)

### stdlib-string-ops: String Operations (requires new Rust builtin)

- [ ] Add `substr` / `slice-str` Rust builtin for substring extraction (unblocks below)
- [ ] `starts-with?`, `ends-with?` — string prefix/suffix tests
- [ ] `chars` — string to character sequence
- [ ] `join` — sequence/dict of strings to single string with separator

## Error Context Enrichment

Enhance error reporting with richer context types inspired by Elm, Nickel, and rustc patterns.

### error-restructuring: Error Model Restructuring

Core error model improvements. Foundation for all later error work.

- [ ] Design structured error model (enum variants, error codes, style guidelines)
- [ ] Migrate freeform string error constructors to structured enum variants (`key_not_found`, `type_mismatch`, `arity_mismatch`)
- [ ] Add structured error codes (E001, E002, ...) for programmatic error filtering and documentation linking
- [ ] Document dual-span error model in DESIGN.md (currently undocumented design decision)
- [ ] Add builtin function name to error stack frames — builtin errors currently lack the function name in stack traces (`src/builtins.rs`, `src/error.rs`) [Major, span-integrity-checker]
- [ ] Deduplicate redundant span output when definition-site == materialization-site — show single span instead of identical pair (`src/error.rs`) [Major, span-integrity-checker]
- [ ] Add dual-span pattern to access chain errors — `DotAccess`, `BracketAccess` errors currently only report definition-site (`src/eval.rs`) [Major, span-integrity-checker]
- [ ] Fix builtin errors using call_span for definition-site — should use operand's span as definition-site, call_span as materialization-site (`src/builtins.rs:82-91`) [Major, span-integrity-checker]
- [ ] Establish error message style guidelines (rustc's rules: no trailing punctuation, no questions, may contain names but not expressions)

### error-context: Error Context & Suggestions

Richer error context for debugging.

- [ ] Add available keys to `key_not_found` errors for "did you mean?" suggestions (use `strsim` crate for edit-distance matching)
- [ ] Filter stdlib/prelude.llt frames from user-facing stack traces (Nickel `group_by_calls` pattern)
- [ ] Build `$include` chain threading — nested include errors should show the full include path ("included from A at line X")
- [ ] Add secondary span support for "evaluated to this" labels on lazy evaluation errors (Nickel dual-position pattern)
- [ ] Reconstruct multi-hop cycle paths for circular dependency errors (show the full cycle chain, not just the blackholed thunk)

### error-ux: Error UX Features

User-facing error presentation improvements.

- [ ] Source snippets in error output — include source context with carets like rustc (span-integrity-checker review)
- [ ] Span-aware error recovery in REPL — show source line with caret pointing to error span (span-integrity-checker review)
- [ ] `tinct explain <error-code>` command for extended help on error categories (span-integrity-checker review, Elm-inspired)
- [ ] Add LSP `related_information` for materialization-site spans and stack frames (currently discarded)

### error-message-polish: Error Message Polish (Minor)

Minor wording and span improvements.

- [ ] Improve document pipeline non-Dict error message for new users (`src/eval.rs:225-246`) [Minor, eval-engine]
- [ ] Fix `Span::origin()` used for non-origin errors — create separate span constructors for runtime limits and default inputs (`src/eval.rs:923, 292`) [Minor, span-integrity-checker]
- [ ] Add call-site span to depth limit errors — currently lacks stack frame attachment (`src/eval.rs:812-820`) [Minor, span-integrity-checker]
- [ ] Enhance "materialized at" error message to distinguish access vs call sites (`src/error.rs:85-86`) [Minor, span-integrity-checker]
- [ ] Change unification error wording from "type mismatch" to "cannot unify X with Y" (`src/types.rs:314`) [Minor, type-theorist]
- [ ] Improve Fn type expression error message for keyed params — currently generic (`src/typecheck.rs:764-772`) [Minor, type-theorist]

### error-infra-nits: Error Nits

Nit-level error infrastructure cleanup.

- [ ] Fix materialize depth check message duplicating constant (`src/eval.rs:812-820`) [Nit, eval-engine]
- [ ] Simplify `EvalError::new` parameter from `impl Into<String>` to `String` (`src/error.rs:56-79`) [Nit, span-integrity-checker]
- [ ] Standardize error category names (`src/error.rs:56+`) [Nit, span-integrity-checker]
- [ ] Review PendingBuiltin error path span handling — may overwrite operand span (`src/eval.rs:886`) [Nit, span-integrity-checker]

## stdlib-docs: Stdlib Documentation

Add type signatures and inline examples to all stdlib functions, serving as both documentation and executable tests.

- [ ] Add type annotations to all `stdlib/prelude.llt` function definitions
- [ ] Add inline assertion examples to each function (Dhall pattern: `assert` examples serve as tests AND docs)
- [ ] Generate stdlib reference documentation from annotated source
- [ ] Stdlib wholeness test: single test validating entire stdlib loads and contains all expected bindings (Nickel pattern)
- [ ] Add docstrings to `$quot` and `$mod` explaining Clojure truncate-toward-zero semantics (`stdlib/prelude.llt:71-73`) [Major, stdlib-author]
- [ ] Document `map-entries` return value structure — clarify whether function receives entries and returns new values or new entries (`stdlib/prelude.llt:314`, `DESIGN.md:1513`) [Major, stdlib-author]
- [ ] Document `values` and `entries` insertion order guarantee (`stdlib/prelude.llt:180-201`) [Minor, stdlib-author]
- [ ] Mark `make-entry` as internal — add docstring or rename to `-impl` (`stdlib/prelude.llt:32`) [Minor, stdlib-author]
- [ ] Add docstring to `fold` justifying alias duplication with `reduce` (`stdlib/prelude.llt:353`) [Nit, stdlib-author]
- [ ] Document `cond` returning `[]` when no branch matches (`stdlib/prelude.llt:120-123`) [Nit, stdlib-author]
- [ ] Add 16 undocumented stdlib functions to DESIGN.md stdlib section: `const`, `>`, `<=`, `>=`, `quot`, `mod`, `ceil`, `trunc`, `join`, `words`, `nth`, `conj`, `reindex`, `from-entries`, `any?`, `all?` [Major, stdlib-author]
- [ ] Add doc comment to `Value::Seq` match arm in `value_to_json` explaining why Seq→JSON is an error and requires `$collect` first (`src/lib.rs:161-166`) [Minor, integration-verifier]
- [ ] Update DESIGN.md concat classification to note dual-dispatch: Seq path is lazy O(1), Dict path is eager O(m) (`DESIGN.md:1257`) [Minor, grammar-architect]
- [ ] Add comment to Seq cycle detection in `deep_materialize` explaining raw pointer identity pattern (`src/eval.rs:1093-1100`) [Nit, integration-verifier]
- [ ] Update DESIGN.md ThunkState sketch to include `Failed(Box<EvalError>)` and `PendingCall` variants (`DESIGN.md:1988-1994`) [Nit, eval-engine]
- [x] Add corpus tests for `any?` and `all?` (any_true, any_false, any_empty, all_true, all_false, all_empty) (`tests/corpus/eval/stdlib/`) [Major, stdlib-author] — tests already exist: any.llt-eval, any_empty.llt-eval, any_shortcircuit.llt-eval, all.llt-eval, all_empty.llt-eval, all_shortcircuit.llt-eval

## Test Infrastructure

Improvements to test infrastructure identified by cross-language analysis and test-crafter review (2026-04-19).

### test-critical: Critical Test Coverage

- [ ] PendingBuiltin state transition unit tests — verify Unevaluated→PendingBuiltin→Materialized lifecycle, error recovery, cycle detection in isolation (`src/value.rs`, `src/eval.rs`) [Critical, test-crafter]
- [ ] Add error corpus tests with span assertions — current tests check message content only, not definition_span, materialization_span, or stack frame accuracy (`tests/corpus/eval/errors/`) [Critical, test-crafter + span-integrity-checker]
- [ ] Add selective materialization unit tests — use mock/panic functions to prove unused branches stay unevaluated (`src/eval.rs`) [Critical, test-crafter]
- [ ] Expand `tests/corpus/eval/laziness/` with more negative tests proving unused expressions are NOT evaluated (current: 2 tests, target: 10+)
- [ ] Add builtins.rs unit tests for additional edge cases — NaN, overflow, Unicode, cycle detection (337 tests exist, expand for special values) (`src/builtins.rs`) [Major, test-crafter]
- [ ] Add typecheck corpus tests (currently zero; Nickel has 90+ granular typecheck test files)
- [ ] Add `deep_materialize` corpus tests through the public API
- [ ] Materialization behavior corpus tests proving stdlib laziness categories (test-crafter review)
- [ ] Add `test_type_of_seq()` unit test verifying `builtin_type_of` returns `"Seq"` for `Value::Seq` — all other Value variants have type-of tests but Seq is missing (`src/builtins.rs`) [Major, integration-verifier]
- [ ] Add corpus test `type_of_seq.txt` — `[call $type-of [call $seq 1 []]]` returns `"Seq"` (`tests/corpus/eval/builtins/`) [Minor, integration-verifier]
- [ ] Add sequence constructor error path corpus tests — `range_start_overflow.txt`, `iterate_non_function.txt`, `unfold_invalid_return.txt`, `cycle_empty.txt` (`tests/corpus/eval/errors/`) [Critical, test-crafter]
- [ ] Add laziness proof tests for map/filter — `map_preserves_thunks.txt`, `filter_selective_materialization.txt` proving unused values stay unevaluated (`tests/corpus/eval/laziness/`) [Critical, test-crafter]
- [ ] Add Failed state same-span deduplication test — access Failed thunk twice with same span, verify no duplicate stack frames (`src/eval.rs`) [Minor, test-crafter]
- [ ] Add Failed state None→Some→Some edge case test — first access with None, then Some(span1), then Some(span2); verifies is_none() path (`src/eval.rs`) [Minor, test-crafter]
- [ ] Add doc comment to Failed state handler explaining dual-span model conditional update strategy (`src/eval.rs:873-894`) [Nit, span-integrity-checker + eval-engine]
- [ ] Add error corpus tests for drop/reduce/join type/arity mismatches — `drop_wrong_type.txt`, `reduce_wrong_type.txt`, `join_wrong_type.txt` (`tests/corpus/eval/errors/`) [Major, test-crafter]
- [ ] Add unit tests for builtin_drop, builtin_reduce, builtin_join (PendingCall chain construction, thunk state, span propagation) (`src/builtins.rs`) [Major, test-crafter]
- [ ] Add include caching corpus tests — same file included twice returns identical result, nested includes share cache, verify cache interaction with cycle detection (`tests/corpus/eval/builtins/`) [Major, test-crafter]
- [x] Add concat edge case corpus tests — empty dicts (`concat [] []`, `concat [a] []`, `concat [] [a]`), testing the `empty?` branch (`tests/corpus/eval/stdlib/`) [Minor, test-crafter] — tests exist: concat.llt-eval, concat_dict.llt-eval, concat_seq.llt-eval, concat_seq_dict.llt-eval
- [ ] Add concat error corpus tests — invalid input types, type mismatches (`tests/corpus/eval/errors/`) [Minor, span-integrity-checker]

### test-additional: Additional Test Coverage

- [ ] Add error corpus tests for arithmetic overflow ($+/$-/$* with i64 bounds), NaN/Infinity rejection ($floor/$round), string parse failure ($to-int/$to-float), TypeAssert failure, range mixed keys [Critical, test-crafter]
- [ ] Add depth limit corpus tests (256 levels succeeds, 257 errors)
- [ ] Add keyword-in-context corpus tests (`[call: 42]`, `[fn: hello]` testing colon-lookahead)
- [ ] Add static constraint negative tests (variadic-not-last, rest-entry position, annotation context)
- [ ] Add stack frame correctness unit tests — verify chain with correct labels and spans (`src/eval.rs:825+`) [Minor, span-integrity-checker]
- [ ] Add type system literal widening tests — widening chain, nested computed keys, polymorphic call with literals (`src/typecheck.rs:83`) [Minor, test-crafter]
- [ ] Add SPEC.md grammar coverage tests — parser_mechanisms tests for 100% grammar rule coverage (`SPEC.md`, `tests/corpus/valid/`) [Minor, test-crafter]
- [ ] Add `$_` implicit lambda edge case tests — nested `$_`, shadowing when `_` already bound, desugaring in dict entries vs call args (`src/eval.rs:669-686`) [Minor, test-crafter]
- [ ] Add row polymorphism tests for Closed-specific behavior — closed record with extra fields (`src/types.rs:679-837`) [Nit, type-theorist]
- [ ] Add === delimiter edge case tests — `delimiter_in_string.txt`, `delimiter_partial.txt`, `delimiter_triple_docs.txt` (`tests/corpus/valid/edge_cases/`) [Major, test-crafter]
- [ ] Add CRLF line ending corpus test — create `.txt` with actual `\r\n` bytes (`tests/corpus/valid/edge_cases/crlf_line_endings.txt`) [Minor, test-crafter]
- [ ] Add Unicode identifier corpus test — `[$café: espresso]` and other Unicode var names (`tests/corpus/valid/literals/unicode_identifiers.txt`) [Minor, test-crafter]
- [ ] Add annotated bare word corpus tests — `[x@Number: 42]`, `[fn@Int [] 42]` (`tests/corpus/valid/annotations/`) [Minor, test-crafter]
- [ ] Add variadic + named args interaction test — positional + variadic + named args together (`tests/corpus/eval/fn_variadic_plus_named.txt`) [Minor, test-crafter]
- [ ] Rename `threading.txt` test file to `pipeline.txt` to match function name (`tests/corpus/eval/stdlib/threading.txt`) [Nit, stdlib-author]
- [ ] Add TypeAssert `default:` fallback corpus test — `[@Number default: 42 "not a number"]` returns 42 (`tests/corpus/eval/builtins/`) [Minor, test-crafter]
- [ ] Add type error corpus tests directory — `type_mismatch.txt`, `unification_failure.txt`, `record_field_missing.txt` (`tests/corpus/eval/type_errors/`) [Major, test-crafter]

### test-framework: Test Framework Enhancements

- [ ] Extend error test framework: support `=== ERROR: substring` for message validation (test-crafter review)
- [ ] Generate per-file test functions for clearer failure reports (Nickel `test_resources!` pattern)
- [ ] Add snapshot testing for error messages using `insta` crate — after error format stabilizes (test-crafter review)
- [ ] Add `just coverage` command using cargo-llvm-cov for coverage measurement (test-crafter review)
- [ ] Add `tests/corpus/eval/regressions/` directory for regression tests
- [ ] Add cross-feature interaction tests (`tests/corpus/eval/cross_feature/`)
- [ ] Consider `assert_ast_eq!` macro for critical parser tests instead of Display comparison (`src/parser.rs:131`) [Minor, test-crafter]
- [ ] Rename builtin tests to `test_*` convention or document current convention (`src/builtins.rs:2298-4700`) [Nit, test-crafter]
- [ ] Standardize error corpus test format and document in README (`tests/corpus/`) [Nit, test-crafter]

### test-advanced: Advanced Testing (Fuzzing, Property-Based, Benchmarks)

- [ ] Add fuzzing targets (`fuzz/fuzz_targets/parse.rs`, `fuzz/fuzz_targets/eval_source.rs`)
- [ ] Add property-based testing (proptest) for parser round-trip and evaluator commutativity
- [ ] Add benchmarking via criterion crate — parser, evaluator, type checker baselines (performance-expert review)
- [ ] Add stack-size canary test (~200 nested brackets)
- [ ] Add pretty-print round-trip idempotence test (parse → Display → re-parse → Display → compare)
- [ ] Add function variance transitivity test or property test — transitivity assumed but not proven for subtyping (`src/types.rs:74-80`) [Major, type-theorist]

### test-tooling: Tooling Integration Tests & Documentation

- [ ] Integration tests for REPL/LSP — multi-line input, hover on nested expressions, multiple errors (test-crafter review)
- [ ] Add LSP corpus tests (`tests/lsp_corpus/`) with `.llt` + `.expected.json` per position
- [ ] IncludeContext API documentation — add docstrings to `eval_source()`, `eval_file()`, `eval_file_with_input()` warning that `$include` requires `set_include_context()` setup (`src/lib.rs`) [Major, integration-verifier]
- [ ] Document circular builtins⇄eval dependency — add safety comment at `src/builtins.rs:28` explaining the value-level vs import-level dependency [Minor, integration-verifier]
- [ ] Cross-layer contracts documentation — add section to DESIGN.md documenting BuiltinFn signature contract, serializer requirements, thread-local state discipline [Minor, integration-verifier]
- [ ] Document `value_to_json` vs `value_to_display_string` NaN/Infinity difference — add test for display_string with NaN/Inf (`src/lib.rs:112-125, 176-211`) [Minor, integration-verifier]
- [ ] Add lib.rs IncludeContext doc comment mentioning cache behavior — memoizes evaluated include results, Jsonnet-style (`src/lib.rs:44-46`) [Minor, integration-verifier]
- [ ] Add DESIGN.md testing requirements section — testing philosophy and per-decision test requirements [Minor, test-crafter]

## Documentation Divergences (DESIGN.md / SPEC.md / Code)

Found by systematic comparison of DESIGN.md, SPEC.md, and source code (2026-04-18).

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

### SPEC.md vs Code (from REVIEW.md)

- [ ] **SPEC.md section 7 semicolon rule divergence** — SPEC.md:802-804 defines `semicolon = _{ ";" }` as standalone rule, but grammar.pest:119 uses `";"?` inline; either add rule to grammar.pest or inline in SPEC.md [Minor, grammar-architect]
- [ ] **SPEC bare_word_char prose terminator list missing `$`** — SPEC.md formal grammar (`SPEC.md:161`) is correct and matches `grammar.pest:219-225`, but the prose terminator list (`SPEC.md:168-170`) omits `$`; add `$` to the bulleted list [Nit, grammar-architect]
- [ ] **SPEC.md §3.4 access chain grammar missing dot exclusion clarity** — add inline comment showing `.` exclusion in `var_ident_char` (`SPEC.md:85-92`) [Major, grammar-architect]
- [x] **SPEC.md §5.3 duplicate key detection claim vs implementation** — VERIFIED: runtime duplicate detection exists at `eval.rs:338`. SPEC.md:629 is accurate. [Resolved, grammar-architect]
- [ ] **SPEC.md annotation_value comment doesn't reference parent rule** — reference `param_annotation`/`fn_annotation` (`SPEC.md:792`) [Nit, grammar-architect]
- [ ] **SPEC.md Token Precedence missing annotated_bare** — add `annotated_bare` at position 5.5 (`SPEC.md:177-186`) [Nit, grammar-architect]
- [ ] **SPEC.md Bracket Nesting Depth Limit doesn't link to TODO.md** — add document reference (`SPEC.md:645-647`) [Nit, grammar-architect]
- [ ] **SPEC.md §8.11 has prose explanation but others don't** — be consistent across examples (`SPEC.md:1137-1139`) [Nit, grammar-architect]
- [ ] **SPEC.md Appendix numbered but only one exists** — remove "A" from "Appendix A" (`SPEC.md:1253`) [Nit, grammar-architect]
- [ ] **SPEC.md §1 Notation table missing compound-atomic** — add `${ ... }` entry (`SPEC.md:11-38`) [Nit, grammar-architect]
- [ ] **SPEC.md describes parser only, not eval semantics** — add note linking to DESIGN.md for eval semantics (`SPEC.md:1-12`) [Nit, integration-verifier]

### DESIGN.md vs Code (from REVIEW.md)

- [ ] **Dot-in-bare-word conflict across SPEC/DESIGN/tree-sitter** — pest allows `.` in bare words but tree-sitter excludes it; document divergence and rationale (`SPEC.md:161`, `DESIGN.md:856`, `tree-sitter-llt/grammar.js:9`) [Critical, grammar-architect]
- [ ] **Unrecorded tree-sitter decision: dot excluded from bare_word_char** — add confirmed decision to DESIGN.md documenting intentional divergence (`tree-sitter-llt/grammar.js:8-9`) [Critical, grammar-architect]
- [ ] **Document separator missing from DESIGN.md Tokenization Rules** — add `---` subsection (`DESIGN.md:722-813`) [Major, grammar-architect]
- [ ] **DESIGN.md Tokenization Rules tables have inconsistent headers** — unify column headers (`DESIGN.md:765-812`) [Nit, grammar-architect]
- [ ] **DESIGN.md pipeline model uses code fence for non-code** — use indentation instead (`DESIGN.md:20-29`) [Nit, grammar-architect]
- [ ] **DESIGN.md "Bare Word Character Set" header doesn't follow pattern** — retitle for consistency (`DESIGN.md:852-886`) [Nit, grammar-architect]
- [ ] **Stdlib count mismatch: 28 builtins vs 62 total not clarified** — update headers to show breakdown (`DESIGN.md:1393`, `CLAUDE.md:30`) [Nit, integration-verifier]

- [ ] **DESIGN.md laziness tables use future tense for completed builtin-thunk-return work** — change "Will return..." to past tense (`DESIGN.md:1245, 1268, 1761-1762`) [Minor, integration-verifier]
- [ ] **DESIGN.md BuiltinFn signature section omits BuiltinArgs struct** — update to mention builtin-thunk-return parameter bundling (`DESIGN.md:1732-1744`) [Minor, integration-verifier]
- [ ] **DESIGN.md include caching description sparse** — expand line 1835 to document cache key (canonical PathBuf), cache scope (thread-local), error non-caching rationale, cache lifetime (`DESIGN.md:1835`) [Minor, eval-engine]
- [ ] **CLAUDE.md IncludeContext description missing cache field** — update builtins.rs row to mention include result cache for memoization (`CLAUDE.md:30`) [Minor, integration-verifier]
- [ ] **DESIGN.md stale builtin count "44 total"** — update to 49 total after Sequences additions or remove inline count per no-stale-counts feedback (`DESIGN.md:1349`) [Minor, stdlib-author]
- [ ] **Include cache code comments** — add skip-guard rationale at cache hit, clarify "Check cache" comment placement, add doc comment to cache field (`src/builtins.rs:1036-1039,52`) [Nit, eval-engine]
- [ ] **IncludeContext::new() constructor** — add constructor to reduce breaking changes when fields are added; low priority pre-1.0 (`src/builtins.rs:54`) [Nit, integration-verifier]

### DESIGN.md documentation gaps (eval-engine review)

- [ ] **Letrec key parent scope justification** — document in DESIGN.md why dict keys in letrec evaluation use the parent scope rather than the letrec env (`src/eval.rs`, `DESIGN.md`) [Minor, eval-engine]
- [ ] **Cycle detection recovery strategy** — document in DESIGN.md what happens after InProgress cycle detection fires: thunk state management, error propagation, and whether thunk is left in InProgress or restored (`src/value.rs`, `DESIGN.md`) [Minor, eval-engine]

### CLAUDE.md (from REVIEW.md)

- [x] **CLAUDE.md references "Phase 6" for hand-written parser, should be Phase 7** — STALE: CLAUDE.md simplified, no longer contains phase references. [Resolved, grammar-architect]
