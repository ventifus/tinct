# Lazy Lisp Transformer

A **unified data representation and transformation language** that combines JSON-like simplicity with lazy functional programming power.

**Vision:** One language for both defining data structures (like JSON/YAML) and transforming them (like JSONnet/jq), with lazy evaluation for efficiency and infinite structures.

**Status:** Phase 0 (parser) and Phase 1a-1c (evaluator foundation + core eval + access chains) complete -- pest PEG grammar, fully spanned AST, lazy evaluator with letrec dict scoping, comprehensive test suite (228 unit tests + file-based corpus). Phase 1d (document evaluation) is next.

## Syntax at a Glance

```lisp
[
    # Data -- just key-value pairs
    base: [timeout: 30  retries: 3]

    # Composition -- merge, override, no repetition
    dev:  [call $merge $base [env: dev]]
    prod: [call $merge $base [env: prod  timeout: 60]]

    # Functions -- first-class, lazy
    double: [fn@Number [x@Number] [call $* $x 2]]

    # Pipelines -- chain transformations
    active-names: [call $-> $users
        [partial $where active $= true]
        [partial $pluck name]
        $sort]
]
```

## Key Features

### Designed

- **Single bracket syntax** -- `[]` for everything: data, function calls, type annotations
- **Dict-first** -- dicts are the fundamental unit; lists are dicts with integer keys
- **`$` sigils** -- bare words are strings, `$word` is a variable reference
- **Explicit `call`/`partial`** -- `[call $f $x]` for application, `[partial $f $x]` for partial application
- **Lazy evaluation** -- everything is a thunk, computed only when needed
- **Mandatory types** -- Haskell-style inference with row polymorphism, `@` annotations
- **Named arguments** -- `[call $fetch $url timeout: 60]` via `@` property dicts
- **No null** -- missing keys are errors, `$get-or` for safe access
- **Pipeline model** -- `data -> transform1 -> transform2 -> canonicalize (JSON/YAML)`

### Implemented

- **Parser** -- pest PEG grammar with whitespace-sensitive access chains
- **AST** -- `File`, `Document`, `Expr`, `Entry`, `Param`, `Annotation`, and `Spanned<T>` node types
- **Evaluator foundation** -- `Value`, `Thunk` (lazy memoization), `Environment` (lexical scope chain) types (Phase 1a)
- **Core evaluation** -- literals, VarRef, dict construction with letrec semantics, cycle detection, depth limit (256) (Phase 1b)
- **Access chains** -- dot access, bracket access, range expressions, type assertions, annotated access (Phase 1c)
- **Error reporting** -- `EvalError` with definition-site span, materialization-site span, and `StackFrame` traces
- **Standard library** -- `stdlib/prelude.llt` with stdlib functions written in LLT itself
- **Corpus testing** -- file-based test suite in `tests/corpus/` with `===` delimiter for expected output

## Examples

### Data Representation

```lisp
[
    database: [
        host: "localhost"
        port: 5432
        timeout: 30
    ]

    api: [
        endpoint: "/v1"
        rate-limit: 1000
    ]
]
```

### With Transformations

```lisp
[
    # Base config -- shared settings
    base: [timeout: 30  retries: 3]

    # Compose environments via merge
    production: [call $merge $base [timeout: 60  env: production]]

    # Transform data -- lazy, only computed when accessed
    users: [call $from-json [call $read-file "users.json"]]
    admin-names: [call $-> $users
        [partial $where role $= admin]
        [partial $pluck name]
        $sort]
]
```

## Quick Start

### Using Just (Containerized -- Recommended)

No Rust installation required. All commands run in containers:

```bash
just build          # Build debug version
just test           # Run all tests (unit + corpus)
just test-corpus    # Run only corpus tests
just run            # Run parser on test_input.txt
just ci             # Run full CI pipeline
just --list         # See all commands
```

### Using Cargo (Native)

If you have Rust installed:

```bash
cargo build --release
cargo test
cargo run -- test_input.txt
```

## Project Structure

| File | Purpose |
|------|---------|
| `src/grammar.pest` | PEG grammar (lexical + syntactic rules) |
| `src/ast.rs` | AST types: `File`, `Document`, `Expr`, `Entry`, `Param`, `Annotation`, `Spanned<T>` |
| `src/parser.rs` | pest pairs to AST conversion + comprehensive unit tests |
| `src/eval.rs` | Evaluator: `eval()`, `materialize()`, dict construction with letrec semantics, depth limit (256) |
| `src/value.rs` | Runtime types: `Value`, `Thunk` (lazy memoization), `Environment` (lexical scope chain) |
| `src/error.rs` | `EvalError` with definition-site span, materialization-site span, `StackFrame` traces |
| `src/test_util.rs` | Shared test helpers: `test_span()`, `sp()` (test-only, `#[cfg(test)]`) |
| `src/lib.rs` | Public API: `parse(input) -> Result<Spanned<File>, ParseError>`, `parse_expression` convenience |
| `src/main.rs` | CLI: read file (max 10MB), parse, print AST |
| `stdlib/prelude.llt` | LLT standard library: stdlib functions written in LLT itself |
| `tests/corpus/` | File-based test suite (valid + invalid inputs) |
| `tests/corpus_tests.rs` | Corpus test runner with `===` delimiter support |
| `test_input.txt` | Example input demonstrating syntax |
| `Cargo.toml` | Dependencies: pest, indexmap |
| `justfile` | Containerized build commands |

## Testing

### Unit Tests

228 tests across multiple modules covering:
- **parser.rs** -- every AST node type, access chains, special forms, annotations, document structure, static constraints, and error cases
- **ast.rs** -- Display/Debug formatting for all AST types
- **eval.rs** -- core evaluation (literals, VarRef, dict letrec, cycle detection), access chain evaluation (dot, bracket, range, type assert, annotated), depth limiting, and materialization span propagation
- **value.rs** -- Value, Thunk, and Environment types (evaluator foundation)
- **error.rs** -- `EvalError` and `StackFrame` formatting with definition-site and materialization-site spans

### Corpus Tests (`tests/corpus/`)

File-based test suite with auto-discovery. Each `.txt` file is parsed; valid inputs must succeed, invalid inputs must fail. Tests can include expected AST output after a `===` delimiter:

```
[key: value]
===
Dict({"key": String("value")})
```

```
tests/corpus/
  valid/
    literals/       -- int, float, bool, string, bare word, var ref
    special_forms/  -- call, fn, type, partial
    access/         -- dot, bracket, chained, range, space-prevents-access
    annotations/    -- type assert (simple + dict)
    documents/      -- multi-expression, multi-document, --- separator
    complex/        -- full config, pipeline, conditionals, comments, semicolons
    simple/         -- basic key-value pairs, nesting
    edge_cases/     -- empty input, whitespace
  invalid/
    syntax_errors/  -- missing bracket, extra tokens, unexpected colon, missing value
```

Add a test by creating a `.txt` file in the appropriate directory, then run `just test-corpus`.

## Requirements

### Containerized Workflow
- **just** -- Command runner ([install](https://github.com/casey/just))
- **podman** or **docker** -- Container runtime

### Native Workflow
- **Rust** 1.83+ -- ([install](https://rustup.rs))

## Documentation

- **[DESIGN.md](DESIGN.md)** -- Language design: vision, 61 confirmed decisions, open questions, roadmap
- **[SPEC.md](SPEC.md)** -- Formal parser specification: lexical/syntactic grammar (PEG), AST node types, static constraints
- **[TODO.md](TODO.md)** -- Implementation roadmap with current status
- **[CLAUDE.md](CLAUDE.md)** -- Development guide and implementation details
