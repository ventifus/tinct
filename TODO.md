# Implementation Roadmap

Extracted from DESIGN.md. Tracks what's built, what's next, and what's deferred.

## Phase 0: Parser — Complete

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

## Phase 1: Evaluator — End-to-End Pipeline

The smallest slice that produces a working `parse -> eval -> output` pipeline. Lazy from the start — every value is a thunk.

**Deliverable:** `llt eval input.llt` outputs JSON. End-to-end pipeline.

### 1a: Value, Thunk, Environment

Foundation types. No evaluation logic yet.

- [x] `Value` enum: Int, Float, String, Bool, Dict, Function, Builtin
- [x] `Key` enum: Int(i64), String(String)
- [x] `Thunk` struct with `RefCell<ThunkState>` (Unevaluated / InProgress / Materialized)
- [x] Source location on thunks for error reporting (definition-site)
- [x] `Environment` with `parent` chain (lexical scoping)
- [x] `EvalError` type with source span (definition-site + materialization-site)
- [x] `Dict` uses insertion-ordered map (`IndexMap` or similar) with `Key` keys and `Rc<Thunk>` values

### 1b: Core Eval

Evaluate literals, variable references, and dict construction. After this step, `[x: 1 y: hello]` produces a dict.

- [x] `eval(ast, env) -> Result<Rc<Thunk>, Box<EvalError>>` — wraps AST nodes in thunks
- [x] `materialize(thunk) -> Result<Value, Box<EvalError>>` — forces a thunk, memoizes result
- [x] Literal evaluation: Int, Float, Bool, Str -> immediate `Materialized` thunks
- [x] VarRef lookup: walk the environment parent chain
- [x] Dict evaluation: create new `Environment` from entries, all values are thunks sharing it (letrec)
- [x] Auto-indexing: unkeyed entries get integer keys 0, 1, 2, ...
- [x] Keyed entries: evaluate key expression, insert with explicit key
- [x] Cycle detection: `InProgress` state triggers circular dependency error on re-entry

### 1c: Access Chains — Complete

Depends on 1b. After this step, `$data.name` and `$data[0]` work.

- [x] DotAccess: materialize expr, look up string key in dict
- [x] BracketAccess: materialize expr, evaluate key, look up in dict
- [x] RangeAccess: materialize expr, filter dict entries by key range
- [x] TypeAssert: evaluate as identity (type checker enforces in Phase 2a)
- [x] Annotated: evaluate as the bare string (type checker interprets in Phase 2a)

### 1d: Document Evaluation — Complete

Depends on 1b-1c. After this step, multi-expression scope chains and `$$` pipeline work.

- [x] Multi-expression documents: each expression's result dict becomes parent scope for the next
- [x] Multi-document files: `---` resets scope, previous document's output becomes `$$`
- [x] `$$` starts as `[]` (empty dict) for the first document
- [x] `$$` passes lazily between documents (no materialization at `---` boundary)

### 1e: Functions — Complete

Depends on 1b. After this step, `[fn [x] $x]` and `[call $f $x]` work.

- [x] `fn` evaluation: capture params + body + current env as `Value::Function`
- [x] `call` evaluation: materialize function, bind args to params in new env, wrap body as thunk
- [x] `$_` implicit lambda: evaluator wraps `[...]` containing VarRef("_") in `[fn [_] [...]]`
- [x] Named argument binding: match named args to params with `default:` annotations
- [x] Arity checking: wrong argument count is an error
- [x] Variadic params: collect remaining positional args into a dict with integer keys

## Phase 2: Type System

Separate AST pass that runs between parsing and evaluation (see DESIGN.md pipeline). Moved up from original Phase 3 to ensure all subsequent phases get proper types from the start.

### 2a: Core Types & Inference

Depends on 1e (full evaluation model must be stable). After this step, the type checker infers types for all data forms and validates annotations.

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

### 2b: Polymorphism & Function Types

Depends on 2a. After this step, polymorphic functions and open records work.

- [x] Function type inference from params + body + annotations
- [x] `Fn@Return [Params]` type interpretation
- [x] Type variable introduction (lowercase names: `a`, `b`, `k`, `v`)
- [x] Type variable unification (Hindley-Milner style)
- [x] Row polymorphism: open records with `...`, named row variables (`...rest`)
- [x] Polymorphic function type checking (e.g., `map: Fn@[b] [Fn@b [a]  [a]]`)
- [x] `Any` as escape hatch with `[@Type $expr]` as the way back to concrete types

## Phase 3: Runtime Pipeline

Build the end-to-end runtime. Builtins get proper type signatures from Phase 2.

### 3a: Rust-Native Builtins

Depends on 1e + 2b. The 28 true primitives that MUST be Rust: operations LLT cannot express. Everything else is derived in LLT in `stdlib/prelude.llt`.

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

### 3a-llt: LLT Stdlib Loading

Depends on 3a. Load `stdlib/prelude.llt` to provide the rest of the stdlib.

- [x] `include_str!("../stdlib/prelude.llt")` to bundle at compile time
- [x] Parse and evaluate prelude with Rust builtins as parent environment
- [x] User code inherits from stdlib environment
- [x] Verify all prelude functions work end-to-end

### 3b: CLI + JSON Output

Depends on 3a. After this step, `llt eval input.llt` produces JSON.

- [x] JSON serialization of Value (`value_to_json` in `lib.rs`, `serde_json`)
- [x] `llt eval input.llt` — evaluate file, serialize final value as JSON to stdout (clap CLI)
- [x] Stdin input: parse stdin as JSON, inject as `$$` for the first document (`eval_file_with_input`)
- [x] `--format` flag: output as JSON (default) or LLT (YAML deferred — would require `serde_yaml` dependency)
- [x] `--eval` flag: deep-force all thunks before serializing (surface errors before partial output)

### 3c: `$include`

Depends on 1b. Complex enough to be its own step: file I/O, cycle detection, scope merging.

- [x] Evaluate a file, return its dict
- [x] Namespaced usage: `utils: [call $include "utils.llt"]`
- [x] Merged usage: include result becomes parent scope
- [x] Cycle detection: error on circular includes
- [x] Path resolution relative to including file

### 3d: Error Reporting Polish

Ongoing throughout earlier phases, but final polish here.

- [x] Call stack reconstruction: chain of materialization sites
- [x] Clear messages: "key not found", "type mismatch", "arity mismatch", "circular dependency"
- [x] Source spans on all errors (definition-site + materialization-site)
- [x] TypeAssert `default:` fallback support
- [x] Thread call-site spans through BuiltinFn signature (resolve Span::origin sentinel in builtin errors)

## Pre-Phase 4: Stdlib Boundary Analysis — Complete

- [x] Identify the minimal set of builtins that MUST be implemented in Rust (28 total: arithmetic, comparison, if, keys/length/merge/append, string ops, numeric conversion, eval/error/try/apply, type-of, from-json, include)
- [x] Identify which stdlib functions CAN be implemented as LLT code (all control flow, collection ops, composition, list ops, sorting, sequences, assertions — implemented in `stdlib/prelude.llt`)
- [x] Document the boundary in DESIGN.md with rationale for each Rust-native builtin (see "Rust-Native vs LLT-Implemented Boundary" section)
- [x] Design the stdlib loading mechanism (`include_str!` prelude, Rust builtins → LLT stdlib → user code environment chain)
- [x] Update Phase 4 task list to reflect the split: Rust-native builtins vs LLT stdlib (Phase 3a = Rust builtins, Phase 4 = LLT already in prelude.llt)

## Phase 4: Stdlib Validation & Expansion

The LLT stdlib is implemented in `stdlib/prelude.llt` (already working; 79 corpus test files cover stdlib functions). This phase validates and expands it. Rust-native builtins (strings, numeric conversion) were registered in Phase 3a. LLT-implemented functions (`and`, `or`, `map`, `filter`, etc.) are already in the prelude.

### Validate prelude functions

- [x] Run prelude end-to-end with evaluator and fix any runtime bugs
- [x] Test each LLT stdlib function against expected behavior (79 corpus tests covering all public stdlib functions)
- [x] Performance check: identify any functions that need Rust reimplementation for practical use (see below)
- [x] Unify `value_to_display_string` and `value_to_json` via shared visitor pattern (analyzed: kept separate — divergent leaf rendering, error handling, and dict assembly means a visitor adds more code than it removes)
- [x] Clear thread-local `INCLUDE_CTX` after evaluation for library API safety (`clear_include_context()`)

Remaining stdlib functions stay in LLT prelude: logic (`and`, `or`), control flow (`cond`, `when`, `unless`), dict utilities (`get`, `get-or`, `get-in`, `has?`, `values`, `entries`, `empty?`, `set`, `remove`, `update`), list ops (`first`, `nth`, `last`, `reindex`), collection ops (`map-entries`, `fold`, `slice`, `find-deep`), composition (`compose`, `->`), error handling (`try-or`), assertions (`assert`), identity (`identity`).

## Phase 5: Sequences and Laziness Restoration

The original design calls for everything to be lazy. Several operations are currently eager due to the `BuiltinFn` return type (`Value` instead of `Rc<Thunk>`) and the absence of lazy function application. This phase adds `Value::Seq` for lazy computation and `PendingCall` thunk state to restore laziness across the language. See DESIGN.md "Sequences and Lazy Computation" for full design.

### 5a: PendingCall Thunk State

Add `PendingCall(func: Rc<Thunk>, args: Vec<Rc<Thunk>>)` to `ThunkState`. This enables lazy function application at runtime without AST nodes.

- [ ] Add `PendingCall` variant to `ThunkState` in `value.rs`
- [ ] Add `Failed(Box<EvalError>)` variant to `ThunkState` for error memoization (Nix's `nFailed` pattern — cache failures instead of restoring `Unevaluated` and re-evaluating)
- [ ] Handle `PendingCall` in `materialize()`: extract func+args, call function, memoize result
- [ ] Handle `PendingCall` in `Thunk::take_*` methods for state management
- [ ] Handle `PendingCall` in cycle detection (set `InProgress`, restore on error)
- [ ] Add `Thunk::new_pending_call(func, args, span)` constructor
- [ ] Tests: PendingCall materializes correctly, memoizes, cycle-detects

### 5b: BuiltinFn Signature Change

Change `BuiltinFn` to return `Rc<Thunk>` instead of `Value`. This removes the forced materialization boundary for builtins.

- [ ] Change `BuiltinFn` type alias: `-> Result<Value, ...>` to `-> Result<Rc<Thunk>, ...>`
- [ ] Update `materialize()` PendingBuiltin handler to use returned thunk
- [ ] Update all 28 builtins to wrap return values in `Thunk::new_materialized()`
- [ ] Update `$if` to return the chosen branch thunk directly (no materialization)
- [ ] Update `$merge` to return lazy overlay (right dict shadows left, no cloning)
- [ ] Tests: verify all builtins still work, $if laziness preserved

### 5c: Value::Seq

Add `Value::Seq(head: Rc<Thunk>, tail: Rc<Thunk>)` for lazy sequences.

- [ ] Add `Seq` variant to `Value` enum
- [ ] Add `seq?` type check to `type-of` builtin
- [ ] Handle `Seq` in `value_to_json` (error: must $collect first)
- [ ] Handle `Seq` in `value_to_display_string` (show `Seq(head, ...)`)
- [ ] Handle `Seq` in `deep_materialize` (force head, recurse on tail up to depth limit)
- [ ] Add visited set to `deep_materialize` for cycle tracking across mutual dict references (Nix `forceValueDeep` pattern)
- [ ] Add `Seq` type to type system (`types.rs`)
- [ ] Sequence builtins (Rust-native): `seq`, `head`, `tail`, `collect`, `seq?`
- [ ] Tests: seq construction, head/tail, collect, type-of, JSON error, display

### 5d: Sequence Constructors

Rewrite `range`, `repeat`, `cycle` (currently in `stdlib/prelude.llt`) as Rust builtins returning `Seq` instead of eagerly-built dicts. Add new constructors `iterate` and `unfold`.

- [ ] Move `range`, `repeat`, `cycle` from LLT prelude to Rust builtins
- [ ] `range` returns Seq; 1-arg form `[call $range start]` is infinite
- [ ] `repeat` returns Seq; 1-arg form `[call $repeat val]` is infinite
- [ ] `cycle` returns Seq; 1-arg form `[call $cycle xs]` is infinite
- [ ] `iterate`: `[call $iterate $f $x]` -> x, f(x), f(f(x)), ...
- [ ] `unfold`: `[call $unfold $step $seed]` -> step returns `[value state]` or `[]`
- [ ] Tests: finite/infinite range, repeat, cycle, iterate, unfold

### 5e: Dual-Dispatch Map/Filter

Make `$map` and `$filter` work on both dicts and sequences, with Rust implementations for performance.

- [ ] `$map` on dict: return dict with PendingCall thunks (lazy, same keys)
- [ ] `$map` on seq: return lazy seq
- [ ] `$filter` on dict: return seq (must evaluate predicates)
- [ ] `$filter` on seq: return lazy seq
- [ ] `$take` on seq: return seq of first n elements
- [ ] `$drop` on seq: return seq skipping first n elements
- [ ] `$reduce` on seq: accumulate, materializing each step
- [ ] Move `map`, `filter`, `take`, `drop`, `reduce` from LLT prelude to Rust builtins
- [ ] Tests: map/filter on dicts (lazy verification), map/filter on seqs, mixed pipelines

### 5f: Include Caching

Cache `$include` results so re-including the same file returns the cached thunk instead of re-evaluating. Jsonnet caches import thunks; LLT currently re-evaluates every `$include` call.

- [ ] Add `HashMap<PathBuf, Rc<Thunk>>` to `IncludeContext` for result caching
- [ ] Return cached thunk on re-include of the same resolved path
- [ ] Tests: same file included twice returns identical thunk, cache respects path normalization

### 5g: Laziness Inventory

Every operation should be as lazy as possible. This is the tracking list.

**Currently eager, should become lazy:**

- [ ] `$map` on dict -- returns eager dict (fix: PendingCall thunks, Phase 5e)
- [ ] `$filter` on dict -- returns eager dict (fix: return Seq, Phase 5e)
- [ ] `$range` -- builds full dict eagerly, O(n^2) (fix: return Seq, Phase 5d)
- [ ] `$repeat` -- builds full dict eagerly (fix: return Seq, Phase 5d)
- [ ] `$cycle` -- builds full dict eagerly (fix: return Seq, Phase 5d)
- [ ] `$if` -- materializes chosen branch (fix: return branch thunk, Phase 5b)
- [ ] `$merge` -- clones both dicts (fix: lazy overlay, Phase 5b)
- [ ] `$update` -- eagerly applies function (fix: PendingCall on updated value, Phase 5e)
- [ ] `$concat` -- eagerly clones and merges (fix: Seq concat, or lazy dict overlay)
- [ ] `$cons` -- eagerly clones and shifts (fix: Seq cons, O(1))
- [ ] `$rest` -- eagerly clones dict minus first entry (fix: Seq tail, O(1))
- [ ] `$reverse` -- eagerly builds new dict (fix: Seq or lazy reindexing)
- [ ] `$zip` -- eagerly builds paired dict (fix: lazy Seq zip)
- [ ] `$flatten` -- eagerly traverses and rebuilds (fix: lazy Seq flatten)
- [ ] `$sort`, `$sort-by` -- eagerly materializes all values (inherently eager: must compare)
- [ ] `$apply` -- double-forces by materializing `invoke_function()`'s result thunk (fix: return thunk directly, Phase 5b)

**Currently eager, must stay eager (inherently materializing):**

- [ ] `$reduce`, `$fold` -- accumulator pattern requires sequential materialization
- [ ] `$sort`, `$sort-by` -- must compare values to determine order
- [ ] `$length` -- must know all entries (on seqs: must traverse entirely)
- [ ] `$=`, `$<` and comparisons -- must inspect values
- [ ] `$+`, `$-`, `$*`, `$/` and arithmetic -- must compute
- [ ] `$str`, `$split`, `$replace`, string ops -- must inspect string content
- [ ] `$to-int`, `$to-float`, `$floor`, `$round` -- must convert
- [ ] `$type-of` -- must inspect value variant
- [ ] `$from-json` -- must parse entire JSON string
- [ ] `$eval` -- deep-forces all thunks by definition
- [ ] `$error` -- constructs error value (structural, but the error itself is concrete)
- [ ] `$find-deep` -- must traverse structure

**Already lazy:**

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
- [x] `$try` -- lazy on success value (wraps in dict)

## Phase 6: Tooling

Execution order: REPL → LSP → tree-sitter. One commit per item.

### Phase 6a: REPL (`llt repl`)

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

### Phase 6b: LSP server (`llt lsp`)

Uses `lsp-server` (sync), not `tower-lsp` (async), because `Rc<RefCell<Environment>>` is not Send/Sync.

- [ ] Add `lsp-server` + `lsp-types` dependencies (optional, under `lsp` feature)
- [ ] Document store (`src/lsp/document.rs`): HashMap<Url, DocumentState>, re-parse/eval/typecheck on change
- [ ] Span conversion (`src/lsp/convert.rs`): LLT Span (offset, 1-indexed) ↔ LSP Position (0-indexed, UTF-16)
- [ ] Analysis + hover (`src/lsp/analysis.rs`)
  - Expose `infer_expr` or add `type_at_position()` helper in typecheck.rs
  - Hover on `$var` shows inferred type, hover on `[call ...]` shows signature
  - Parse errors → Error diagnostics, type errors → Warning diagnostics (advisory)
  - Go-to-definition: stretch goal (requires span tracking in Environment bindings)
- [ ] Main loop (`src/lsp/server.rs`): `Connection` + crossbeam message loop
  - Requests: `initialize`, `shutdown`, `textDocument/hover`
  - Notifications: `didOpen`, `didChange`, `didClose`
  - Publish diagnostics on every document change
- [ ] CLI wiring: `Lsp` variant in `Commands`, `llt lsp` on stdio
- [ ] Tests (span conversion, hover analysis, diagnostic generation, simulated LSP client)
- [ ] Just recipe: `just lsp`

### Phase 6c: tree-sitter grammar

- [x] Scaffold `tree-sitter-llt/` (package.json, grammar.js skeleton, tree-sitter.json)
- [x] Implement grammar rules (port from grammar.pest / SPEC.md)
  - `token.immediate()` for whitespace-sensitive `.` and `[` access
  - Special form keywords via tree-sitter `word` rule keyword extraction
  - Comments as `comment` node type in extras
  - `---` document separator via external scanner (C, negative lookahead)
- [x] Test corpus (49 tests: literals, dicts, special forms, access, documents, annotations, comments)
- [x] Highlight queries (`queries/highlights.scm`)
- [x] Just recipes: `just ts-generate`, `just ts-test`, `just ts-parse FILE`


## Phase 7: Hand-Written Parser (E2)

Replace pest's recursive descent with a hand-written lexer + iterative parser using an explicit stack. The pest parser stays as a reference implementation for comparison until the new parser graduates.

**Goal:** Identical AST output from both parsers, selectable at parse time. Once the new parser passes the full test suite and matches pest output on all corpus files, it becomes the default and pest is removed.

### Phase 7a: Lexer (`src/lexer.rs`)

Tokenizer producing a flat token stream. Whitespace-sensitivity for access chains handled here.

- [ ] Token enum: OpenBracket, CloseBracket, Colon, Semicolon, Dot, Range, At, Ellipsis, DocSeparator, Int(i64), Float(f64), BareWord(String), QuotedString(String), VarRef(String), BoolLit(bool)
- [ ] Single-pass tokenization with source spans on every token
- [ ] Whitespace-sensitive access detection: Dot/OpenBracket immediately after VarRef or CloseBracket (no whitespace) emits access-context tokens
- [ ] Comment skipping (`#` to EOL)
- [ ] String escapes (`\"`, `\\`, `\n`, `\t`, `\r`)
- [ ] Bare word denylist matching grammar.pest rules
- [ ] CRLF line ending support: track line boundaries correctly for `\r\n` (pest and LSP convert.rs both assume `\n` only)

### Phase 7b: Iterative parser (`src/parser2.rs`)

Explicit `Vec<StackFrame>` for bracket nesting. Atoms and access chains parsed without recursion.

- [ ] StackFrame enum: Dict, Call, Fn, TypeAlias, TypeAssert (one variant per bracket form)
- [ ] On `[`: push frame, determine form from first token (keyword detection)
- [ ] On `]`: pop frame, construct AST node
- [ ] Between brackets: parse atoms, access chains, annotations (all non-recursive)
- [ ] MAX_DEPTH check on `stack.len()` (policy, not safety)
- [ ] Static constraints: positional-before-named, duplicate keys, variadic rules
- [ ] Error messages with precise context ("expected value after `:`", "unclosed bracket at line 5")

### Phase 7c: Formatter/Pretty-Printer (`llt fmt`)

Uses the hand-written lexer's token stream (comment-preserving, unlike pest).

- [ ] Formatting engine (`src/formatter.rs`)
  - Indent nested `[]` by 2 spaces per depth
  - One entry per line (unless bracket expr fits on one line, ~80 char threshold)
  - Comments stay attached to their line
  - Semicolons replaced with newlines (or preserved in single-line mode)
  - Consistent spacing: one space after `:`
  - `---` separators get blank lines above and below
  - Collapse multiple blank lines to one
- [ ] CLI subcommand: `Fmt` variant with `--check`, `--in-place`, `--stdin`
- [ ] Tests (idempotency, comment preservation, indentation, single-line vs multi-line, edge cases)
- [ ] Just recipes: `just fmt-llt FILE`, `just fmt-llt-check FILE`

### Phase 7d: Integration

- [ ] `parse()` API accepts parser selection (enum or feature flag)
- [ ] Both parsers produce identical `Spanned<File>` output
- [ ] Comparison test: parse every corpus file with both parsers, assert AST equality
- [ ] Benchmark: compare parse time on large inputs

### Phase 7e: Graduation criteria

- [ ] Full test suite passes (all unit + corpus tests)
- [ ] AST output matches pest parser on every corpus file
- [ ] Error messages are equal or better quality
- [ ] No stack overflow on any nesting depth up to MAX_DEPTH

### Phase 7f: Cleanup (post-graduation)

- [ ] Remove `pest` and `pest_derive` dependencies from Cargo.toml
- [ ] Remove `src/grammar.pest`
- [ ] Remove pest-specific code from `src/parser.rs`
- [ ] Rename `src/parser2.rs` to `src/parser.rs`
- [ ] Update CLAUDE.md, README.md, SPEC.md references

## Performance: Stdlib Rust Reimplementations

Nearly all accumulator-based stdlib functions are O(n^2) due to `merge`/`append` materializing and cloning the growing accumulator IndexMap on every iteration. Sort is O(n^2 log n) because `sort-merge` uses `cons` (O(n)) per element.

**Note:** Phase 5 (Sequences and Laziness Restoration) addresses much of this by moving `map`, `filter`, `range`, `take`, `drop`, `reduce` to Rust builtins with lazy dispatch. The items below track remaining performance work not covered by Phase 5.

Remaining Rust reimplementations after Phase 5 (all currently in `stdlib/prelude.llt`):
- `rest`, `cons`, `conj`, `concat`, `reverse` -- list primitives, used by sort (O(n) each due to cloning)
- `sort`, `sort-by` / `sort-merge` -- single Rust builtin using Vec::sort_by would be O(n log n)
- `zip`, `flatten`, `find-deep` -- recursive traversal or lazy seq versions for perf

## Phase 7½: Iterative Evaluator

Replace the recursive `eval()` / `materialize()` call stack with an explicit continuation stack (stack machine). Nix, Nickel, and Jsonnet all use iterative evaluation with explicit frame types. LLT's recursive approach risks stack overflow on deeply-nested lazy chains and prevents tail-call optimization.

- [ ] Design `Frame` enum for explicit continuation stack (similar to Jsonnet's 22 `FrameKind` variants)
- [ ] Convert `materialize()` from recursive to iterative with `Vec<Frame>` work stack
- [ ] Convert `eval()` hot paths (dict construction, access chains) to iterative
- [ ] Implement tail-call optimization (TCO) for `call` expressions — detect tail position, reuse frame
- [ ] TCO for recursive stdlib functions (`fold`, `map`, `filter`, `sort-merge`) to avoid stack overflow on large inputs
- [ ] Benchmark: compare recursive vs iterative on deep chains and large collections
- [ ] Remove 64MB worker thread stack workaround once iterative eval eliminates deep recursion

## Phase 8: Full Row-Variable Unification (Remy-Style)

Replace the current closed-strict/open-lenient record unification with full Remy-style row-variable unification. Row variables become first-class participants in type inference, enabling the type checker to infer record extension and restriction through polymorphic function boundaries.

- [ ] Add VarLevel (integer rank) tracking to type variables for sound let-polymorphism generalization (Elm's rank-based approach — determines which type variables can be generalized at `let` boundaries)
- [ ] Extend `Substitution::apply` to splice bound row variable fields into records (e.g., `[a: Int | ...r]` with `r → [b: String]` produces `[a: Int, b: String]`)
- [ ] Unify row rests: `RowVar` vs `RowVar` binds one to the other, `RowVar` vs `Closed` binds the var to the leftover fields as a closed record
- [ ] Handle "remainder" binding: `unify([a: Int | ...r], [a: Int, b: String | Closed])` binds `r → [b: String | Closed]`
- [ ] Extend `Type` representation if needed to support partial-row bindings (row var bound to fields + another row var)
- [ ] Update `instantiate` to freshen row variables alongside type variables
- [ ] Test inference through polymorphic functions that extend/restrict records (e.g., `[fn add-id [r@[...rest]] [id: 1  ...rest]]`)
- [ ] Verify consistency between `unify` and `is_subtype` for all RowRest combinations

## Phase 9: Sandboxing & Security

Design and implement filesystem sandboxing for `$include` and any future I/O operations. Currently `$include` can read any file the process can access (mitigated only by file size limit and cycle detection).

- [ ] Design sandboxing model: restrict includes to a subtree of the initial file's directory
- [ ] Decide policy for absolute paths (block entirely vs. resolve relative to sandbox root)
- [ ] Decide policy for symlinks (resolve and check target is within sandbox, or block)
- [ ] Implement sandbox root calculation in `IncludeContext`
- [ ] Add `canonical.starts_with(&root_dir)` check in `builtin_include`
- [ ] Add CLI flag to configure sandbox root (e.g., `--include-root`)
- [ ] Test: relative paths within sandbox succeed
- [ ] Test: `../` traversal beyond sandbox root fails
- [ ] Test: absolute paths outside sandbox fail
- [ ] Test: symlinks pointing outside sandbox fail

## Phase 10: Stdlib Expansion

Missing functions identified by cross-language analysis (Jsonnet, jq, Nix, Dhall). All implementable in LLT unless noted.

### 10a: Core Missing Functions

- [ ] `from-entries` — inverse of `$entries`; reconstruct dict from `[key value]` pairs (jq pattern)
- [ ] `with-entries` — `entries | map(f) | from-entries` pipeline (jq pattern)
- [ ] `partition` — single-pass split into matching/non-matching dicts (Nix + Dhall)
- [ ] `flat-map` / `concat-map` — `flatten (map f xs)`, monadic bind for collections (Jsonnet + jq)
- [ ] `find-first` / `find-first-or` — first element matching predicate, with default (Nix)
- [ ] `any?` / `all?` — short-circuit predicate tests over collections
- [ ] `group-by` — group elements by key function, returning dict of lists (Nix)
- [ ] `deep-merge` — recursive merge for configuration overlays (Jsonnet, RFC 7396)
- [ ] `walk` — recursive bottom-up transform of all sub-values (jq)
- [ ] `until` — iterate function until predicate holds; functional loop (jq)

### 10b: Convenience Functions

- [ ] `sum`, `min`, `max`, `count` — aggregate functions (one-liners over fold)
- [ ] `contains?` / `elem?` — membership test
- [ ] `uniq` / `unique` — deduplicate collection
- [ ] `foldr` — right fold (LLT only has left fold currently)
- [ ] `zip-with` — generalized zip with combining function; define `zip` as special case (Nix)
- [ ] `map-indexed` / `map-keys` — indexed mapping and key transformation (Jsonnet)
- [ ] `sort-on` — sort by key-extraction function instead of comparator (Jsonnet + Nix)
- [ ] `const`, `flip`, `abs`, `sign`, `clamp` — small composable primitives (Nix + Jsonnet)

### 10c: Type Predicates & Guards

- [ ] `is-int?`, `is-str?`, `is-float?`, `is-bool?`, `is-dict?`, `is-fn?` — type predicate wrappers over `$type-of` (Jsonnet pattern)
- [ ] Runtime assertion guards at stdlib function entry with descriptive errors (Jsonnet pattern)

### 10d: String Operations (requires new Rust builtin)

- [ ] Add `substr` / `slice-str` Rust builtin for substring extraction (unblocks below)
- [ ] `starts-with?`, `ends-with?` — string prefix/suffix tests
- [ ] `chars` — string to character sequence
- [ ] `join` — sequence/dict of strings to single string with separator

## Phase 11: Error Context Enrichment

Enhance error reporting with richer context types inspired by Elm, Nickel, and rustc patterns.

- [ ] Add available keys to `key_not_found` errors for "did you mean?" suggestions (use `strsim` crate for edit-distance matching)
- [ ] Filter stdlib/prelude.llt frames from user-facing stack traces (Nickel `group_by_calls` pattern)
- [ ] Build `$include` chain threading — nested include errors should show the full include path ("included from A at line X")
- [ ] Establish error message style guidelines (rustc's rules: no trailing punctuation, no questions, may contain names but not expressions)
- [ ] Migrate freeform string error constructors to structured enum variants (`key_not_found`, `type_mismatch`, `arity_mismatch`)
- [ ] Add secondary span support for "evaluated to this" labels on lazy evaluation errors (Nickel dual-position pattern)
- [ ] Reconstruct multi-hop cycle paths for circular dependency errors (show the full cycle chain, not just the blackholed thunk)

## Phase 12: Stdlib Documentation

Add type signatures and inline examples to all stdlib functions, serving as both documentation and executable tests.

- [ ] Add type annotations to all `stdlib/prelude.llt` function definitions
- [ ] Add inline assertion examples to each function (Dhall pattern: `assert` examples serve as tests AND docs)
- [ ] Generate stdlib reference documentation from annotated source
- [ ] Stdlib wholeness test: single test validating entire stdlib loads and contains all expected bindings (Nickel pattern)

## Phase 13: Test Infrastructure Improvements

Improvements to test infrastructure identified by cross-language analysis.

- [ ] Add `tests/corpus/eval/laziness/` directory with negative tests proving unused expressions are NOT evaluated
- [ ] Add `tests/corpus/eval/regressions/` directory for regression tests
- [ ] Add typecheck corpus tests (currently zero; Nickel has 90+ granular typecheck test files)
- [ ] Add `deep_materialize` corpus tests through the public API
- [ ] Add keyword-in-context corpus tests (`[call: 42]`, `[fn: hello]` testing colon-lookahead)
- [ ] Add fuzzing targets (`fuzz/fuzz_targets/parse.rs`, `fuzz/fuzz_targets/eval_source.rs`)
- [ ] Add pretty-print round-trip idempotence test (parse → Display → re-parse → Display → compare)
- [ ] Add stack-size canary test (~200 nested brackets)

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
