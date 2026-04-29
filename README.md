# tinct

A **unified data representation and transformation language** that combines JSON-like simplicity with lazy functional programming power.

**Vision:** One language for both defining data structures (like JSON/YAML) and transforming them (like JSONnet/jq), with lazy evaluation for efficiency and infinite structures.

**Status:** Phases 0-4, 6a, and 6b complete -- hand-written iterative parser, fully spanned AST, lazy evaluator with letrec dict scoping, scope chains, `$$` pipeline, function evaluation, Hindley-Milner type inference with row polymorphism, 46 Rust-native builtins, Tinct standard library (79 corpus tests covering all public functions), interactive REPL with line editing and history, LSP server with textDocument/didOpen, didChange, and publishDiagnostics, comprehensive test suite (1425+ tests).

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
    active-names: [call $sort
        [call $map [fn [u] $u.name]
            [call $filter [fn [u] $u.active] $users]]]
]
```

## Key Features

### Designed

- **Single bracket syntax** -- `[]` for everything: data, function calls, type annotations
- **Dict-first** -- dicts are the fundamental unit; lists are dicts with integer keys
- **`$` sigils** -- bare words are strings, `$word` is a variable reference
- **Explicit `call`** -- `[call $f $x]` for function application
- **Lazy evaluation** -- everything is a thunk, computed only when needed
- **Mandatory types** -- Haskell-style inference with row polymorphism, `@` annotations
- **Named arguments** -- `[call $fetch $url timeout: 60]` via `@` property dicts
- **No null** -- missing keys are errors, `$get-or` for safe access
- **Pipeline model** -- `data -> transform1 -> transform2 -> canonicalize (JSON/YAML)`

### Implemented

- **Parser** -- hand-written iterative descent parser with whitespace-sensitive access chains
- **AST** -- `File`, `Document`, `Expr`, `Entry`, `Param`, `Annotation`, and `Spanned<T>` node types
- **Evaluator foundation** -- `Value`, `Thunk` (lazy memoization), `Environment` (lexical scope chain) types (Phase 1a)
- **Core evaluation** -- literals, VarRef, dict construction with letrec semantics, cycle detection, depth limit (256) (Phase 1b)
- **Access chains** -- dot access, bracket access, range expressions, type assertions, annotated access (Phase 1c)
- **Document evaluation** -- multi-expression scope chains, multi-document `$$` pipeline with lazy passing (Phase 1d)
- **Function evaluation** -- `fn`/`call` with closures, `$_` implicit lambda desugaring, named args with defaults, variadics, arity checking (Phase 1e)
- **Type system** -- `Type` enum (Int, IntLiteral, Float, Str, StringLiteral, Bool, Number, Record, Function, TypeVar, Any), `TypeEnv` scope chain, `TypeError` reporting (Phase 2a)
- **Type checker** -- `typecheck_file()`, `infer_expr()`, four-pass dict inference, access chain checking, TypeAssert enforcement, type alias expansion (Phase 2a)
- **Polymorphism** -- Hindley-Milner unification, `Fn@Return [Params]` function type expressions, row polymorphism (open/closed/row-var records), type variable instantiation per call site (Phase 2b)
- **Rust-native builtins** -- 46 builtins (arithmetic, comparison, control, dict, string, numeric, parsing, eval control, type introspection, I/O, sequences, proxy) with `standard_builtins()` registry (Phase 3a + 3c)
- **Standard library** -- `stdlib/prelude.llt` with stdlib functions written in Tinct itself, loaded via `create_stdlib_env()` (Phase 3a-llt)
- **Error reporting** -- `EvalError` with definition-site span, materialization-site span, and `StackFrame` traces
- **Interactive REPL** -- `tinct repl` with line editing, history, bracket matching, scope chains, and error recovery (Phase 6a)
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

    # Inline data
    users: [
        [name: Alice  role: admin  active: true]
        [name: Bob    role: user   active: false]
        [name: Carol  role: admin  active: true]
    ]

    # Transform data -- lazy, only computed when accessed
    active-admins: [call $sort
        [call $map [fn [u] $u.name]
            [call $filter [fn [u] [call $and $u.active [call $= $u.role admin]]] $users]]]
]
```

## Quick Start

### Using Just (Containerized -- Recommended)

No Rust installation required. All commands run in containers:

```bash
just build          # Build debug version
just test           # Run all tests (unit + corpus)
just test-corpus    # Run only corpus tests
just run            # Eval test_input.llt, output JSON
just repl           # Start interactive REPL
just ci             # Run full CI pipeline
just --list         # See all commands
```

### Using Cargo (Native)

If you have Rust installed:

```bash
cargo build --release
cargo test
cargo run -- eval test_input.llt
cargo run -- eval --format llt test_input.llt  # Tinct display format
cargo run -- eval --eval test_input.llt         # Deep-force all thunks
echo '{"x": 1}' | cargo run -- eval -           # Read Tinct from stdin
echo '{"x": 1}' | cargo run -- eval file.llt    # Inject JSON as $$
cargo run --features repl -- repl               # Start interactive REPL
```

## Project Structure

| File | Purpose |
|------|---------|
| `src/lexer.rs` | Hand-written tokenizer with whitespace-sensitive access detection |
| `src/parser.rs` | Hand-written iterative descent parser + comprehensive unit tests |
| `src/ast.rs` | AST types: `File`, `Document`, `Expr`, `Entry`, `Param`, `Annotation`, `Spanned<T>` |
| `src/eval.rs` | Evaluator: `eval()`, `materialize()` (call-site span attachment, stack frame propagation), dict construction with letrec semantics, document evaluation with scope chains and `$$` pipeline, function evaluation (`fn`/`call`), `$_` implicit lambda desugaring, named args, variadics, arity checking, TypeAssert `default:` fallback, depth limit (256) |
| `src/builtins.rs` | 46 Rust-native builtins (arithmetic, comparison, control, dict, string, numeric, parsing, eval control, type introspection, I/O, sequences, proxy), `IncludeContext` + thread-local for `$include`, `standard_builtins()` registry, `create_root_env()`, `create_stdlib_env()` (loads `stdlib/prelude.llt`) |
| `src/value.rs` | Runtime types: `Value`, `Thunk` (lazy memoization), `Environment` (lexical scope chain), `BuiltinFn` signature |
| `src/error.rs` | `EvalError` with definition-site span, materialization-site span, `StackFrame` traces |
| `src/types.rs` | Type system: `Type` enum (Int, Float, Str, Bool, Number, Record, Function, TypeVar, Any, IntLiteral, StringLiteral, Seq, Proxy), `Row` struct with `RowTail` (Empty, RowVar), `Substitution` (kinded unification with `type_map` and `row_map`), `TypeEnv` (Rc-based scope chain), `TypeError`, `InferState` (levels-based let-generalization) |
| `src/typecheck.rs` | Type checker: `typecheck_file()`, `infer_expr()`, four-pass dict inference, access chain checking, TypeAssert enforcement, type alias expansion, polymorphic `check_call`, `Fn@Return [Params]` resolution, row polymorphism |
| `src/test_util.rs` | Shared test helpers: `test_span()`, `sp()` (test-only, `#[cfg(test)]`) |
| `src/lib.rs` | Public API: `parse()`, `parse_expression()`, `eval_source()`, `eval_file()`, `eval_file_with_input()`, `materialize()`, `deep_materialize()`, `create_stdlib_env()`, `set_include_context()`, `clear_include_context()`, `IncludeContext`, `json_to_value()`, `value_to_json()`, `value_to_display_string()` |
| `src/repl.rs` | REPL session: scope chains, bracket matching, error recovery |
| `src/main.rs` | CLI (`tinct` binary): `tinct eval [OPTIONS] <FILE>` -- evaluate Tinct files, output JSON or Tinct format, stdin JSON injection, `--eval` deep-forcing, `$include` context setup |
| `stdlib/prelude.llt` | Tinct standard library: stdlib functions written in Tinct itself |
| `tests/corpus/` | File-based test suite (valid + invalid inputs) |
| `tests/corpus_tests.rs` | Corpus test runner with `===` delimiter support |
| `tests/cli_tests.rs` | CLI integration tests: file eval, format flags, stdin JSON, error handling |
| `test_input.llt` | Example input demonstrating syntax |
| `Cargo.toml` | Dependencies: indexmap, serde_json, clap, rustyline (optional) |
| `justfile` | Containerized build commands |

## Testing

### Unit Tests

1425+ tests across multiple modules covering:
- **parser.rs** -- every AST node type, access chains, special forms, annotations, document structure, static constraints, and error cases
- **ast.rs** -- Display/Debug formatting for all AST types
- **eval.rs** -- core evaluation (literals, VarRef, dict letrec, cycle detection), access chain evaluation (dot, bracket, range, type assert, annotated), document evaluation (scope chains, `$$` pipeline, laziness, isolation), function evaluation (`fn`/`call`, named args, variadics, `$_` implicit lambda desugaring, TypeAlias), depth limiting, and materialization span propagation
- **builtins.rs** -- all 46 Rust-native builtins (arithmetic auto-promotion, division by zero, comparison cross-type, `if` selective materialization, dict operations, string operations, numeric floor/round with NaN/infinity guards, string parsing, eval/error/try/apply, type-of, from-json, include with cycle detection/path resolution/nested includes/stdlib access, sequences, proxy), stdlib env loading (root env + prelude)
- **value.rs** -- Value, Thunk, and Environment types (evaluator foundation)
- **error.rs** -- `EvalError` and `StackFrame` formatting with definition-site and materialization-site spans
- **types.rs** -- Type enum, TypeEnv scope chain, subtyping (Number, structural records, function variance, open/closed/row-var records), unification (Hindley-Milner, type variable instantiation, substitution application, literal promotions, occurs check)
- **typecheck.rs** -- type inference (literals, records, access chains, functions, scope chains, `$$` pipeline), TypeAssert enforcement, type alias resolution, annotation interpretation, `Fn@Return [Params]` function type expressions, row polymorphism, polymorphic function call checking (instantiate + unify + apply)

### Corpus Tests (`tests/corpus/`)

File-based test suite with auto-discovery. Each `.llt-eval` file is parsed; valid inputs must succeed, invalid inputs must fail. Tests can include expected output after a `===` delimiter:

```
[key: value]
===
Dict({"key": String("value")})
```

```
tests/corpus/
  valid/
    literals/       -- int, float, bool, string, bare word, var ref
    special_forms/  -- call, fn, type
    access/         -- dot, bracket, chained, range, space-prevents-access
    annotations/    -- type assert (simple + dict)
    documents/      -- multi-expression, multi-document, --- separator
    complex/        -- full config, pipeline, conditionals, comments, semicolons
    simple/         -- basic key-value pairs, nesting
    edge_cases/     -- empty input, whitespace
  invalid/
    syntax_errors/  -- missing bracket, extra tokens, unexpected colon, missing value
  eval/             -- evaluator tests (simple dict, scope chain, $$ pipeline, functions)
    builtins/       -- builtin function evaluation
    errors/         -- expected eval failures (cycle detection, arity, undefined var)
    stdlib/         -- stdlib function evaluation
```

Add a test by creating a `.llt-eval` file in the appropriate directory, then run `just test-corpus`.

## Requirements

### Containerized Workflow
- **just** -- Command runner ([install](https://github.com/casey/just))
- **podman** or **docker** -- Container runtime

### Native Workflow
- **Rust** 1.85+ -- ([install](https://rustup.rs))

## Documentation

- **[doc/](doc/index.md)** -- Language specification (17 chapters): syntax, data model, functions, type system, evaluation, stdlib, tooling, examples, internals
- **[TODO.md](TODO.md)** -- Implementation roadmap with current sprint status

## Development Workflow

Features move through these stages in order:

1. **Design** — explore the problem, evaluate alternatives
2. **Whatif doc** (`doc/whatif/`) — write a proposal covering current state, what the feature provides, design, phased adoption, and references
3. **Accept & update spec** — mark the whatif `State: Accepted`, verify prerequisites are met or scheduled, run a design review for proposals touching formal semantics, incorporate the design into the relevant `doc/` chapters (present tense; the spec describes tinct as it will be), and create TODO.md sprints for each adoption phase
4. **Sprint** — one or more TODO.md sprints tracking implementation tasks
5. **Complete** — sprint checked off; spec was already accurate

The `doc/` directory is the language spec. It describes the intended language, not the current implementation state. Implementation follows the spec.
