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

- [ ] `eval(ast, env) -> Result<Rc<Thunk>, EvalError>` — wraps AST nodes in thunks
- [ ] `materialize(thunk) -> Result<Value, EvalError>` — forces a thunk, memoizes result
- [ ] Literal evaluation: Int, Float, Bool, Str -> immediate `Materialized` thunks
- [ ] VarRef lookup: walk the environment parent chain
- [ ] Dict evaluation: create new `Environment` from entries, all values are thunks sharing it (letrec)
- [ ] Auto-indexing: unkeyed entries get integer keys 0, 1, 2, ...
- [ ] Keyed entries: evaluate key expression, insert with explicit key
- [ ] Cycle detection: `InProgress` state triggers circular dependency error on re-entry

### 1c: Access Chains

Depends on 1b. After this step, `$data.name` and `$data[0]` work.

- [ ] DotAccess: materialize expr, look up string key in dict
- [ ] BracketAccess: materialize expr, evaluate key, look up in dict
- [ ] RangeAccess: materialize expr, filter dict entries by key range
- [ ] TypeAssert: evaluate as identity (defer enforcement to Phase 3)
- [ ] Annotated: evaluate as the bare string (defer to Phase 3)

### 1d: Document Evaluation

Depends on 1b-1c. After this step, multi-expression scope chains and `$$` pipeline work.

- [ ] Multi-expression documents: each expression's result dict becomes parent scope for the next
- [ ] Multi-document files: `---` resets scope, previous document's output becomes `$$`
- [ ] `$$` starts as `[]` (empty dict) for the first document
- [ ] `$$` passes lazily between documents (no materialization at `---` boundary)

### 1e: Functions

Depends on 1b. After this step, `[fn [x] $x]` and `[call $f $x]` work.

- [ ] `fn` evaluation: capture params + body + current env as `Value::Function`
- [ ] `call` evaluation: materialize function, bind args to params in new env, wrap body as thunk
- [ ] `$_` implicit lambda: evaluator wraps `[...]` containing VarRef("_") in `[fn [_] [...]]`
- [ ] Named argument binding: match named args to params with `default:` annotations
- [ ] Arity checking: wrong argument count is an error
- [ ] Variadic params: collect remaining positional args into a dict with integer keys

### 1f: Core Builtins

Depends on 1e. Populate root environment with builtins for basic computation.

- [ ] Builtin registration: populate root environment with `Value::Builtin` entries
- [ ] Arithmetic: `+`, `-`, `*`, `/`, `div`, `mod` with auto-promotion (Int+Int=Int, mixed=Float)
- [ ] Comparison: `=`, `<`, `>`, `<=`, `>=`
- [ ] Logic: `if` (short-circuit), `and`, `or`, `not`
- [ ] Dict: `get`, `get-or`, `has?`, `merge`, `keys`, `values`, `length`, `empty?`
- [ ] Strings: `str` (concat)
- [ ] `$eval` — recursively force all thunks
- [ ] `$apply` — call function with list entries spread as positional args
- [ ] `identity` — return argument unchanged
- [ ] `error` — construct error value

### 1g: CLI + JSON Output

Depends on 1f. After this step, `llt eval input.llt` produces JSON.

- [ ] JSON serialization of Value (new dependency: `serde_json`)
- [ ] `llt eval input.llt` — evaluate file, serialize final value as JSON to stdout
- [ ] `$from-json` builtin — parse JSON string into LLT dict
- [ ] Stdin input: parse stdin as JSON, inject as `$$` for the first document
- [ ] `--format` flag: output as JSON (default), YAML, or LLT
- [ ] `--eval` flag: deep-force all thunks before serializing (surface errors before partial output)

### 1h: `$include`

Depends on 1b. Complex enough to be its own step: file I/O, cycle detection, scope merging.

- [ ] Evaluate a file, return its dict
- [ ] Namespaced usage: `utils: [call $include "utils.llt"]`
- [ ] Merged usage: include result becomes parent scope
- [ ] Cycle detection: error on circular includes
- [ ] Path resolution relative to including file

### 1i: Error Reporting Polish

Ongoing throughout Phase 1, but final polish here.

- [ ] Call stack reconstruction: chain of materialization sites
- [ ] Clear messages: "key not found", "type mismatch", "arity mismatch", "circular dependency"
- [ ] Source spans on all errors (definition-site + materialization-site)

## Phase 2: Stdlib Expansion

Expand builtins to cover the full stdlib listed in DESIGN.md.

### List operations (integer keys, renumber)

- [ ] `first`, `rest`, `cons`, `conj`, `concat`, `reverse`
- [ ] `sort`, `sort-by` (materializing)
- [ ] `reindex`

### Universal collection operations

- [ ] `map`, `map-entries` (lazy-transforming)
- [ ] `filter` (materializing predicates, lazy on values)
- [ ] `reduce`, `fold` (materializing)
- [ ] `nth`, `last`, `slice`, `take`, `drop`
- [ ] `zip`, `flatten`, `find-deep`

### Dict operations

- [ ] `get-in`, `set`, `remove`, `update`, `entries`

### Threading and composition

- [ ] `->` (threading/pipeline)
- [ ] `compose`

### Control flow

- [ ] `cond` (multi-branch conditional)
- [ ] `when`, `unless` (single-arm conditional)

### Error handling

- [ ] `try`, `try-or`

### Numeric utilities

- [ ] `to-int`, `to-float`, `floor`, `ceil`, `round`

### String utilities

- [ ] `words`, `join`, `split`, `replace`, `upper`, `lower`, `trim`

### Sequences

- [ ] `range`, `repeat`, `cycle`, `lazy-seq`

### Introspection

- [ ] `type-of`, `assert`

## Phase 3: Type System

- [ ] Type inference engine
- [ ] Row polymorphism for dicts
- [ ] Type alias expansion
- [ ] Type error reporting
- [ ] Function type interpretation (`Fn@Return [Params]` in type checker)
- [ ] `Annotated` node interpretation in type context
- [ ] TypeAssert enforcement (identity in Phase 1)

## Phase 4: Tooling

- [ ] REPL
- [ ] LSP server (tower-lsp)
- [ ] tree-sitter grammar for syntax highlighting
- [ ] Formatter/pretty-printer
