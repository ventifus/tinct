# tinct

A **general-purpose programming language that puts structured data first** — making it natural to define, compose, query, and transform data without reaching for a separate tool.

Also: a testbed for fully automated *agentic virtuous-loop* software development.

**Vision:** One language where structured data is the native citizen. No impedance mismatch between your data model and your transformation logic — no shell pipelines to glue things together, no separate query language, no JSON-in-strings. Lazy evaluation keeps large structures efficient, Hindley-Milner types catch shape errors before they reach production, and generator-native pipelines (think jq, but typed and composable) make data flow a first-class concern.

## Syntax at a Glance

```tinct
[
    # Data -- just key-value pairs
    base: [timeout: 30  retries: 3]

    # Composition -- merge, override, no repetition
    dev:  [merge base [env: "dev"]]
    prod: [merge base [env: "prod"  timeout: 60]]

    # Functions -- first-class, lazy
    double: [fn@Number [x@Number] [* x 2]]

    # Pipelines -- chain transformations
    active-names: [sort
        [map [fn [u] u.name]
            [filter [fn [u] u.active] users]]]
]
```

## Key Features

### Single bracket syntax

`[]` for everything: dicts, function calls, type annotations, and document separators. One rule, no special forms to memorize.

```tinct
[name: "alice"  active: true]         # dict
[map [fn [u] u.name] users]           # function call
[x@Int: 42]                           # annotated entry
```

### Dict-first

Dicts are the fundamental data structure. Lists are dicts with consecutive integer keys, so all operations work uniformly on both.

```tinct
[a: 1  b: 2]   # dict — string keys
[10  20  30]   # list — integer keys 0, 1, 2
```

### Bare references

Quoted strings are literals; bare identifiers are variable references. The `$` prefix disambiguates data from calls.

```tinct
[env: "production"  tier: "api"]   # quoted strings are literals
[base: [env: environment]]         # environment is a variable
```

### Implied call

Function application uses `[f args...]` where `f` is a bare identifier, making calls concise. The `$` prefix forces data interpretation.

```tinct
[+ x 1]
[map [fn [u] u.name] users]
```

### Lazy evaluation

Everything is a thunk — computed only when materialized. Unused branches cost nothing; large structures can be partially accessed without evaluating the whole.

```tinct
[
    all-records: [load "large-dataset.json"]
    first-name:  all-records.0.name   # materializes only what's needed
]
```

### Type inference

Hindley-Milner inference with row polymorphism. Annotate where you want precision; the rest is inferred. Type errors are reported before evaluation runs.

```tinct
[
    double: [fn@Number [x@Number] [* x 2]]
    result: [double 21]   # inferred: Int
]
```

### Union types and algebraic data types

`x@[Int Null]` annotates a nullable value. Multi-entry `[type ...]` declarations define structural ADTs; `[match]` destructures them with exhaustiveness checking.

```tinct
Result: [type [ok: a] [err: Str]]

parse: [fn@Result [input@Str]
    [match [try [json-parse input]]
        [ok: v]    [ok: v]
        [err: msg] [err: [str "parse failed: " msg]]]]
```

### Pattern matching

`[match x ...]` dispatches on type, literal value, or structure. Dict and seq patterns bind fields. Guards and or-patterns extend arms.

```tinct
[match event
    [click: [x: cx  y: cy]]  [handle-click cx cy]
    [key:   [code: k]]        [handle-key k]
    _                         [ignore]]
```

### Type classes

Constrained polymorphism. Full Haskell-style class and instance declarations with multi-parameter type classes, functional dependencies, and runtime dispatch. Equality, comparison, and arithmetic overload through the class system.

```tinct
sorted: [sort items key: [fn [x] x.priority]]   # Comparable constraint enforced

[class [Add a b c] method: +]
[instance [Add Int Float Float] method: [fn [x y] [+ [float x] y]]]
[+ 1 2.0]   # infers Float via functional dependency
```

### Higher-kinded types and monadic composition

`Kind::Operator` (`* → *`) enables generic programming over type constructors. Functor, Applicative, Monad, Foldable, Traversable hierarchy with `Maybe` ADT. `[do]` macro desugars to `monad.bind` chains; the monad can be explicit or inferred from the return annotation.

```tinct
result: [do MonadResult
    [x <- [fetch-user id]]
    [y <- [fetch-posts x.id]]
    [pure [merge x [posts: y]]]]
```

### Named arguments

Call sites pass named arguments after positional ones. Functions declare named parameters with optional defaults.

```tinct
[fetch url timeout: 60  retries: 3]
```

### No null

Missing keys are always errors. Use `get-or` for optional access, keeping the absence of a value explicit.

```tinct
data.missing                           # error: key not found
[get-or data "missing" "fallback"]     # explicit optional access
```

### `%` pipeline

Multi-document files pass the output of one document to the next as `%`. Transform data across stages without intermediate variables or a shell pipeline.

```tinct
[users: [...]]
---
[active: [filter [fn [u] u.active] %.users]]
---
[sort %.active]
```

### Standard library

`stdlib/prelude.llt` is written in Tinct itself, covering collection operations, string manipulation, math, and control flow. It is loaded automatically into every evaluation.

Supplemental modules are available but must be loaded explicitly with `[include libdir "name.llt"]`:

| Module | Contents |
|--------|----------|
| `strings.llt` | String utilities: `pad-left`, `pad-right`, `str-find`, `str-reverse` |
| `math.llt` | Math constants (`pi`, `e`, `phi`) and functions (`hypot`, `deg->rad`, `rad->deg`, `log-base`) |
| `encoding.llt` | Base64 and hex encode/decode; XOR masking |
| `numeric.llt` | Numeric type aliases: `UInt8`/`UInt16`/`UInt32`, `Int8`/`Int16`/`Int32` |
| `path.llt` | Path manipulation: `basename`, `dirname`, `extension`, `path-join`, `path-parts` |
| `io.llt` | File I/O helpers: `read-file`, `read-lines`, `println`, `write-file`, `write-file-atomic` |
| `datetime.llt` | `Timestamp`, `Duration`, `ClockCap`, timezone support |
| `regex.llt` | Literal matching regex engine; `re-compile`/`re-match`/`re-find`/`re-replace`/`re-split` |
| `net.llt` | HTTP helpers: `http-get`, `fetch` (HTTPS via reqwest ALPN), `parse-http-response`, URL utilities |
| `toml-lite.llt` | TOML subset parser in pure Tinct |
| `protocols/dns.llt` | DNS query wire format (RFC 1035): `encode-dns-name`, `build-dns-query`, QTYPE constants |
| `protocols/websocket.llt` | WebSocket frame encoding/decoding + HTTP upgrade handshake |
| `protocols/socks5.llt` | SOCKS5 proxy wire helpers |
| `protocols/grpc.llt` | gRPC frame encoding/decoding |
| `macros.llt` | `tmpl` and `do` macro transformers (auto-loaded by the expander) |

### Interactive REPL

`tinct repl` — incremental evaluation with persistent scope chains, bracket matching, line editing, and history.

### Source formatter

`tinct fmt` — idempotent formatter that canonicalizes whitespace and layout. `--check` mode for CI, `--in-place` for editor integration.

`tinct fmt --oneline` / `--nospaces` / `--minimize` — tinct-hosted compact formatter modes via `stdlib/formatter/compact.llt`. Produces minified output for diffing, embedding, and tooling pipelines.

### Runtime reflection

`[describe f]` returns a dict with the function's signature, doc string, annotation, and source AST. `[ast-of f]` returns the full body as a tinct dict using the canonical `[type: "..." span: [...] ...]` schema. Enables docgen, REPL `:describe`, and metaprogramming without a separate tooling layer.

```tinct
doc: [describe my-fn].doc   # "Computes the running total"
sig: [sig-from-ast [ast-of my-fn]]
```

### Type stage

`--- stage: type` document sections run at type-check time, not at runtime. They define type-level functions, aliases, and resolvers using a subset of tinct (`%rust "type-core"`). The type dict schema (`[kind: "named" name: "Int"]`, `[kind: "seq" elem: T]`, etc.) is the interchange format between the inference engine and type-stage code.

### AST dict schema

`ast_to_dict` — canonical serialization of any tinct program as a tinct dict. Shared infrastructure for the formatter, quasiquoting, and macros. Every `Expr` variant maps to a `[type: "..." span: [...] ...]` dict node, enabling tinct programs to inspect and transform tinct programs.

### LSP server

`tinct lsp` — language server with live diagnostics and hover types over stdio, integrating with any LSP-capable editor.

## Examples

### Data Representation

```tinct
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

```tinct
[
    # Base config -- shared settings
    base: [timeout: 30  retries: 3]

    # Compose environments via merge
    production: [merge base [timeout: 60  env: "production"]]

    # Inline data
    users: [
        [name: "Alice"  role: "admin"  active: true]
        [name: "Bob"    role: "user"   active: false]
        [name: "Carol"  role: "admin"  active: true]
    ]

    # Transform data -- lazy, only computed when accessed
    active-admins: [sort
        [map [fn [u] u.name]
            [filter [fn [u] [and u.active [= u.role "admin"]]] users]]]
]
```

## Quick Start

### Using Just (Containerized -- Recommended)

No Rust installation required. All commands run in containers:

```bash
just build          # Build debug version
just test           # Run all tests (unit + corpus)
just test-corpus    # Run only corpus tests
just run            # Eval samples/basic.llt (the demo program), output JSON
just repl           # Start interactive REPL
just ci             # Run full CI pipeline
just --list         # See all commands
```

### Using Cargo (Native)

If you have Rust installed:

```bash
cargo build --release
cargo test
cargo run -- eval samples/basic.llt
cargo run -- eval --format llt samples/basic.llt  # Tinct display format
cargo run -- eval --eval samples/basic.llt         # Deep-force all thunks
echo '{"x": 1}' | cargo run -- eval -           # Read Tinct from stdin
echo '{"x": 1}' | cargo run -- eval file.llt    # Inject JSON as %
cargo run -- fmt samples/basic.llt                 # Format source file (stdout)
cargo run -- fmt --in-place samples/basic.llt      # Format source file in place
cargo run -- fmt --check samples/basic.llt         # Check formatting without writing
cargo run --features repl -- repl               # Start interactive REPL
cargo run --features lsp -- lsp                 # Start LSP server (stdio)
```

## Project Structure

| File | Purpose |
|------|---------|
| `src/lexer.rs` | Hand-written tokenizer with whitespace-sensitive access detection |
| `src/parser.rs` | Hand-written iterative descent parser + comprehensive unit tests |
| `src/ast.rs` | AST types: `File`, `Document`, `Expr`, `Entry`, `Param`, `Annotation`, `Spanned<T>` |
| `src/ast_dict.rs` | `ast_to_dict` — canonical AST → tinct dict serialization; shared by formatter, quasiquoting, macros |
| `src/expand.rs` | Macro expander: runs `[defmacro]` transformers before typecheck/eval; pre-registers `tmpl` macro |
| `src/desugar.rs` | Desugarer: `$_` implicit lambda; source-to-source pass between parsing and type checking |
| `src/resolve.rs` | Variable resolution pass: de Bruijn slot assignment, free-variable detection |
| `src/imports.rs` | Shared import resolution: `build_prelude_env()`, `collect_include_paths()`, `build_type_env()` |
| `src/types.rs` | Type system: `Type` enum (including `Union`, `Intersection`, `Negation`, `Never`, `Top`, `Map`); `Row` (flat, no tail after BAS); `Substitution` (kinded unification); `TypeEnv`, `TypeError`, `InferState` (levels-based generalization) |
| `src/type_env.rs` | Builtin type registrations: seeds `TypeEnv` with types for all builtins; `%pwd`/`%libdir`/`%stdin` cap types |
| `src/type_unify.rs` | Unification engine: `unify()`, occurs check, row unification, level adjustment |
| `src/typecheck.rs` | Type checker: `typecheck_file()`, `infer_expr()`, five-pass dict inference, TypeAssert enforcement, type alias expansion, polymorphic `check_call`, row polymorphism |
| `src/typecheck_annot.rs` | Annotation type inference helpers |
| `src/typecheck_dict.rs` | Dict-specific type inference (five-pass algorithm) |
| `src/value.rs` | Runtime types: `Value`, `Thunk` (lazy memoization), `Environment` (lexical scope chain), `BuiltinFn` signature |
| `src/arena.rs` | `ThunkId(u32)` arena: `Vec<Thunk>` flat storage for the evaluator |
| `src/eval.rs` | Evaluator core: `eval()`, `Expr::Sequential` strict binding, dict construction with letrec semantics |
| `src/eval_call.rs` | Function call evaluation: `fn`/`call`, named args, variadics, arity checking |
| `src/eval_materialize.rs` | `materialize()`: call-site span attachment, stack frame propagation, WHNF forcing |
| `src/eval_access.rs` | Access chain evaluation: dot, bracket, range, TypeAssert |
| `src/eval_dict.rs` | Dict construction and letrec scoping |
| `src/eval_pipeline.rs` | Document pipeline evaluation: scope chains, `%` pipeline, document-level Sequential |
| `src/eval_deep.rs` | `deep_materialize()`: recursive full forcing of all thunks |
| `src/builtins.rs` | Builtin registry: `standard_builtins()`, `create_root_env()`, `create_stdlib_env()` (loads `stdlib/prelude.llt`) |
| `src/builtins_io.rs` | I/O builtins: `open`, `slurp`, `write`, `lines`; `connect` (transport-generic: Tcp/Udp/UnixStream/UnixDatagram/Icmp); `tls-layer`, `tls-peer-cert`, `spki-pin`; `quic-session`, `quic-open-stream`, `http2-session`, `http3-session`, `http-request`, `icmp-ping`; `src/async_rt.rs` tokio runtime |
| `src/builtins_math.rs` | Math builtins: arithmetic, `floor`, `ceil`, `round`, `pow`, `log`, `sqrt`, etc. |
| `src/builtins_string.rs` | String builtins: `str`, `str-find`, `str-split`, `str-replace`, `str-chars`, etc. |
| `src/builtins_meta.rs` | Meta builtins: `type-of`, `tag-of`, `eval`, `try`, `apply`, `force`, `validate` |
| `src/builtins_bytes.rs` | Bytes builtins: `bytes-of`, `bytes-find`, `bytes-equal?`, `bytes-concat` |
| `src/builtins_uri.rs` | URI builtins: `uri`, `url`, `urn`, `uri-params`, `uri-origin`, `uri->string` |
| `src/builtins_datetime.rs` | Date/time builtins: `now`, `timestamp-add`, `timestamp-diff`, `format-timestamp`, `parse-timestamp` |
| `src/builtins_dict.rs` | Dict builtins: `merge`, `get`, `get-or`, `keys`, `values`, `entries`, `map-keys` |
| `src/builtins_seq_prim.rs` | Sequence primitives: `length`, `first`, `last`, `nth`, `take`, `drop`, `reverse` |
| `src/builtins_seq_xform.rs` | Sequence transformers: `map`, `filter`, `flat-map`, `zip`, `each`, `each-key`, `each-kv` |
| `src/builtins_seq_gen.rs` | Sequence generators: `range`, `repeat`, `iterate`, `collect-kv` |
| `src/builtins_seq_reduce.rs` | Sequence reducers: `reduce`, `fold`, `sum`, `any`, `all`, `count` |
| `src/error.rs` | `EvalError` with definition-site span, materialization-site span, `StackFrame` traces; `TypeError` with `T001`–`T004` codes; `render_span_snippet` |
| `src/formatter.rs` | Source formatter: idempotent pretty-printing (`tinct fmt`), `--check` mode |
| `src/literate.rs` | Literate mode support |
| `src/coverage.rs` | Coverage instrumentation |
| `src/test_util.rs` | Shared test helpers: `test_span()`, `sp()` (test-only, `#[cfg(test)]`) |
| `src/lib.rs` | Public API: `parse()`, `eval_source()`, `eval_file()`, `materialize()`, `deep_materialize()`, `create_stdlib_env()`, `json_to_value()`, `value_to_json()`, `value_to_display_string()`; `EvalContext`, `EvalConfig`, `EvalState` |
| `src/repl.rs` | REPL session: scope chains, bracket matching, `:describe`/`:type`/`:help` meta-commands, error recovery |
| `src/lsp/` | LSP server: `tinct lsp` with `textDocument/didOpen`, `didChange`, `publishDiagnostics`, and hover |
| `src/main.rs` | CLI (`tinct` binary): `eval`, `fmt`, `repl`, `lsp`, `explain` subcommands; `--cap-fs`/`--cap-net`/`--cap-file` cap injection |
| `stdlib/prelude.llt` | Core stdlib (auto-loaded): collection ops, string utils, control flow, Result type, `str-find`, `str-repeat`, `make-entry` |
| `stdlib/strings.llt` | String utilities: `pad-left`, `pad-right`, `str-reverse` (explicit include required) |
| `stdlib/math.llt` | Math constants + functions: `pi`, `e`, `phi`, `hypot`, `deg->rad`, `log-base` (explicit include required) |
| `stdlib/encoding.llt` | Base64/hex encode/decode, XOR masking (explicit include required) |
| `stdlib/numeric.llt` | Integer type aliases: `UInt8`–`UInt32`, `Int8`–`Int32` (explicit include required) |
| `stdlib/path.llt` | Path manipulation: `basename`, `dirname`, `extension`, `path-join` (explicit include required) |
| `stdlib/io.llt` | File I/O helpers: `read-file`, `read-lines`, `println`, `write-file` (explicit include required) |
| `stdlib/datetime.llt` | Date/time support: `Timestamp`, `Duration`, `ClockCap` (explicit include required) |
| `stdlib/regex.llt` | Regex engine in pure Tinct: `re-match`, `re-find`, `re-replace` (explicit include required) |
| `stdlib/net.llt` | HTTP helpers: `http-get`, `fetch`, URL utilities (explicit include required) |
| `stdlib/toml-lite.llt` | TOML subset parser in pure Tinct (explicit include required) |
| `stdlib/macros.llt` | `tmpl` and `do` macro transformers (auto-loaded by expander) |
| `stdlib/protocols/` | Wire format libraries: `dns.llt`, `websocket.llt`, `socks5.llt`, `grpc.llt` (explicit include required) |
| `tests/corpus/` | File-based test suite (valid + invalid inputs) |
| `tests/corpus_tests.rs` | Corpus test runner with `===` delimiter support |
| `tests/cli_tests.rs` | CLI integration tests: file eval, format flags, stdin JSON, error handling |
| `samples/` | Sample tinct programs (`basic.llt` — the canonical demo) |
| `Cargo.toml` | Dependencies: indexmap, serde_json, clap, rustyline, rustls, cap-std, etc. |
| `justfile` | Containerized build commands |

## Testing

### Unit Tests

Tests across multiple modules covering:
- **parser.rs** -- every AST node type, access chains, special forms, annotations, document structure, static constraints, and error cases
- **ast.rs** -- Display/Debug formatting for all AST types
- **eval.rs** -- core evaluation (literals, variable references, dict letrec, cycle detection), access chain evaluation (dot, bracket, range, type assert, annotated), document evaluation (scope chains, `%` pipeline, laziness, isolation), function evaluation (`fn`/`call`, named args, variadics, `_` implicit lambda desugaring, TypeAlias), depth limiting, and materialization span propagation
- **builtins.rs** -- all Rust-native builtins (arithmetic auto-promotion, division by zero, comparison cross-type, `if` selective materialization, dict operations, string operations, numeric floor/round with NaN/infinity guards, string parsing, eval/error/try/apply, type-of, from-json, include with cycle detection/path resolution/nested includes/stdlib access, sequences, proxy), stdlib env loading (root env + prelude)
- **value.rs** -- Value, Thunk, and Environment types (evaluator foundation)
- **error.rs** -- `EvalError` and `StackFrame` formatting with definition-site and materialization-site spans
- **types.rs** -- Type enum, TypeEnv scope chain, subtyping (Number, structural records, function variance, open/closed/row-var records), unification (Hindley-Milner, type variable instantiation, substitution application, literal promotions, occurs check)
- **typecheck.rs** -- type inference (literals, records, access chains, functions, scope chains, `%` pipeline), TypeAssert enforcement, type alias resolution, annotation interpretation, `Fn@Return [Params]` function type expressions, row polymorphism, polymorphic function call checking (instantiate + unify + apply)

### Corpus Tests (`tests/corpus/`)

File-based test suite with auto-discovery. Each `.llt-eval` file is parsed; valid inputs must succeed, invalid inputs must fail. Tests can include expected output after a `===` delimiter:

```
[key: "value"]
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
  eval/             -- evaluator tests (simple dict, scope chain, % pipeline, functions)
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

## Naming Conventions

### Builtin and stdlib function names

Two-word names use a hyphen. The convention depends on the relationship
between the two words:

**Domain-first** — when operating on an existing value of a known type,
the domain/type/protocol comes first, the verb second:

```
str-find      str-length    str-chars     str-repeat
bytes-find    bytes-of      bytes-equal?
timestamp-add timestamp-diff timestamp-year
http-get      tls-layer     quic-session
dir-cap       net-cap       tag-of        type-of
```

**Verb-first** — when constructing or converting *to* a domain type
(the input is not yet that type), the verb comes first:

```
parse-timestamp   format-timestamp
load-tz           from-json
write-atomic
```

The dividing line: if the primary input *is already* the domain type,
domain-first. If the function *produces* that type from something else,
verb-first.

Single-word builtins are always verbs: `map`, `filter`, `open`, `slurp`,
`connect`, `emit`, `reverse`, `sort`.

### File and module names

stdlib files use `kebab-case.llt`. Source files use `snake_case.rs`.

## Development Workflow

Features move through these stages in order:

1. **Design** — explore the problem, evaluate alternatives
2. **Whatif doc** (`doc/whatif/`) — write a proposal covering current state, what the feature provides, design, phased adoption, and references
3. **Accept & update spec** — mark the whatif `State: Accepted`, verify prerequisites are met or scheduled, run a design review for proposals touching formal semantics, incorporate the design into the relevant `doc/` chapters (present tense; the spec describes tinct as it will be), and create TODO.md sprints for each adoption phase
4. **Sprint** — one or more TODO.md sprints tracking implementation tasks
5. **Complete** — sprint checked off; spec was already accurate

The `doc/` directory is the language spec. It describes the intended language, not the current implementation state. Implementation follows the spec.
