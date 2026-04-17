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

- [x] Type representation: Type enum (Int, Float, String, Bool, Number, Record, Function, TypeVar, Any)
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

Depends on 1e + 2b. The 27 true primitives that MUST be Rust: operations LLT cannot express. Everything else is derived in LLT in `stdlib/prelude.llt`.

- [x] Builtin registration: populate root environment with `Value::Builtin` entries
- [x] Arithmetic: `+`, `-`, `*`, `/` with auto-promotion (Int+Int=Int, mixed=Float)
- [x] Comparison: `<`, `=`
- [x] Control: `if` (selective materialization: only chosen branch evaluated)
- [x] Dict primitives: `keys`, `length`, `merge` (right-biased)
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

- [ ] Evaluate a file, return its dict
- [ ] Namespaced usage: `utils: [call $include "utils.llt"]`
- [ ] Merged usage: include result becomes parent scope
- [ ] Cycle detection: error on circular includes
- [ ] Path resolution relative to including file

### 3d: Error Reporting Polish

Ongoing throughout earlier phases, but final polish here.

- [ ] Call stack reconstruction: chain of materialization sites
- [ ] Clear messages: "key not found", "type mismatch", "arity mismatch", "circular dependency"
- [ ] Source spans on all errors (definition-site + materialization-site)
- [ ] TypeAssert `default:` fallback support
- [ ] Thread call-site spans through BuiltinFn signature (resolve Span::origin sentinel in builtin errors)

## Pre-Phase 4: Stdlib Boundary Analysis — Complete

- [x] Identify the minimal set of builtins that MUST be implemented in Rust (27 total: arithmetic, comparison, if, keys/length/merge/append, string ops, numeric conversion, eval/error/try/apply, type-of, from-json)
- [x] Identify which stdlib functions CAN be implemented as LLT code (all control flow, collection ops, composition, list ops, sorting, sequences, assertions — implemented in `stdlib/prelude.llt`)
- [x] Document the boundary in DESIGN.md with rationale for each Rust-native builtin (see "Rust-Native vs LLT-Implemented Boundary" section)
- [x] Design the stdlib loading mechanism (`include_str!` prelude, Rust builtins → LLT stdlib → user code environment chain)
- [x] Update Phase 4 task list to reflect the split: Rust-native builtins vs LLT stdlib (Phase 3a = Rust builtins, Phase 4 = LLT already in prelude.llt)

## Phase 4: Stdlib Validation & Expansion

The LLT stdlib is implemented in `stdlib/prelude.llt` (already working; 61 corpus test files cover stdlib functions). This phase validates and expands it. Rust-native builtins (strings, numeric conversion) were registered in Phase 3a. LLT-implemented functions (`and`, `or`, `map`, `filter`, etc.) are already in the prelude.

### Validate prelude functions

- [x] Run prelude end-to-end with evaluator and fix any runtime bugs
- [ ] Test each LLT stdlib function against expected behavior
- [ ] Performance check: identify any functions that need Rust reimplementation for practical use

### Remaining items not yet in prelude

- [ ] `lazy-seq` — lazy infinite sequence constructor (may need Rust support)

### Deferred to stdlib

These are already in `stdlib/prelude.llt` and loaded via `create_stdlib_env()`:
- Logic: `and`, `or`
- Control flow: `cond`, `when`, `unless`
- Dict utilities: `get`, `get-or`, `get-in`, `has?`, `values`, `entries`, `empty?`, `set`, `remove`, `update`
- List ops: `first`, `rest`, `nth`, `last`, `cons`, `conj`, `concat`, `reverse`, `reindex`, `sort`, `sort-by`
- Collection ops: `map`, `map-entries`, `filter`, `reduce`, `fold`, `slice`, `take`, `drop`, `zip`, `flatten`, `find-deep`
- Composition: `compose`, `->` (threading)
- Error handling: `try-or`
- Sequences: `range`, `repeat`, `cycle`
- Assertions: `assert`
- Identity: `identity`

## Phase 5: Tooling

- [ ] REPL
- [ ] LSP server (tower-lsp)
- [ ] tree-sitter grammar for syntax highlighting
- [ ] Formatter/pretty-printer


## Phase 6: Hand-Written Parser (E2)

Replace pest's recursive descent with a hand-written lexer + iterative parser using an explicit stack. The pest parser stays as a reference implementation for comparison until the new parser graduates.

**Goal:** Identical AST output from both parsers, selectable at parse time. Once the new parser passes the full test suite and matches pest output on all corpus files, it becomes the default and pest is removed.

### Lexer (`src/lexer.rs`)

Tokenizer producing a flat token stream. Whitespace-sensitivity for access chains handled here.

- [ ] Token enum: OpenBracket, CloseBracket, Colon, Semicolon, Dot, Range, At, Ellipsis, DocSeparator, Int(i64), Float(f64), BareWord(String), QuotedString(String), VarRef(String), BoolLit(bool)
- [ ] Single-pass tokenization with source spans on every token
- [ ] Whitespace-sensitive access detection: Dot/OpenBracket immediately after VarRef or CloseBracket (no whitespace) emits access-context tokens
- [ ] Comment skipping (`#` to EOL)
- [ ] String escapes (`\"`, `\\`, `\n`, `\t`, `\r`)
- [ ] Bare word denylist matching grammar.pest rules

### Iterative parser (`src/parser2.rs`)

Explicit `Vec<StackFrame>` for bracket nesting. Atoms and access chains parsed without recursion.

- [ ] StackFrame enum: Dict, Call, Fn, TypeAlias, TypeAssert (one variant per bracket form)
- [ ] On `[`: push frame, determine form from first token (keyword detection)
- [ ] On `]`: pop frame, construct AST node
- [ ] Between brackets: parse atoms, access chains, annotations (all non-recursive)
- [ ] MAX_DEPTH check on `stack.len()` (policy, not safety)
- [ ] Static constraints: positional-before-named, duplicate keys, variadic rules
- [ ] Error messages with precise context ("expected value after `:`", "unclosed bracket at line 5")

### Integration

- [ ] `parse()` API accepts parser selection (enum or feature flag)
- [ ] Both parsers produce identical `Spanned<File>` output
- [ ] Comparison test: parse every corpus file with both parsers, assert AST equality
- [ ] Benchmark: compare parse time on large inputs

### Graduation criteria

- [ ] Full test suite passes (all unit + corpus tests)
- [ ] AST output matches pest parser on every corpus file
- [ ] Error messages are equal or better quality
- [ ] No stack overflow on any nesting depth up to MAX_DEPTH

### Cleanup (post-graduation)

- [ ] Remove `pest` and `pest_derive` dependencies from Cargo.toml
- [ ] Remove `src/grammar.pest`
- [ ] Remove pest-specific code from `src/parser.rs`
- [ ] Rename `src/parser2.rs` to `src/parser.rs`
- [ ] Update CLAUDE.md, README.md, SPEC.md references

## Phase 7: Full Row-Variable Unification (Remy-Style)

Replace the current closed-strict/open-lenient record unification with full Remy-style row-variable unification. Row variables become first-class participants in type inference, enabling the type checker to infer record extension and restriction through polymorphic function boundaries.

- [ ] Extend `Substitution::apply` to splice bound row variable fields into records (e.g., `[a: Int | ...r]` with `r → [b: String]` produces `[a: Int, b: String]`)
- [ ] Unify row rests: `RowVar` vs `RowVar` binds one to the other, `RowVar` vs `Closed` binds the var to the leftover fields as a closed record
- [ ] Handle "remainder" binding: `unify([a: Int | ...r], [a: Int, b: String | Closed])` binds `r → [b: String | Closed]`
- [ ] Extend `Type` representation if needed to support partial-row bindings (row var bound to fields + another row var)
- [ ] Update `instantiate` to freshen row variables alongside type variables
- [ ] Test inference through polymorphic functions that extend/restrict records (e.g., `[fn add-id [r@[...rest]] [id: 1  ...rest]]`)
- [ ] Verify consistency between `unify` and `is_subtype` for all RowRest combinations
