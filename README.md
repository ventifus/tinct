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
    double: [fn@Number [let x@Number] [* x 2]]

    # Pipelines -- chain transformations
    active-names: [sort
        [map [fn [let u] u.name]
            [filter [fn [let u] u.active] users]]]
]
```

## Key Features

### Single bracket syntax

`[]` for everything: dicts, function calls, type annotations, and document separators. One rule, no special forms to memorize.

```tinct
[name: "alice"  active: true]         # dict
[map [fn [let u] u.name] users]       # function call
[x@Integer: 42]                           # annotated entry
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
[map [fn [let u] u.name] users]
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
    double: [fn@Number [let x@Number] [* x 2]]
    result: [double 21]   # inferred: Int
]
```

### Union types and algebraic data types

`x@[or Int Null]` annotates a nullable value. Multi-entry `[type ...]` declarations define structural ADTs; `[match]` destructures them with exhaustiveness checking.

```tinct
Result: [type [ok: a] [err: String]]

parse: [fn@Result [let input@String]
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
sorted: [sort items key: [fn [let x] x.priority]]   # Comparable constraint enforced

[class [Add a b c] method: +]
[instance [Add Int Float Float] method: [fn [let x y] [+ [float x] y]]]
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
[active: [filter [fn [let u] u.active] %.users]]
---
[sort %.active]
```

### Native streaming format

`-o stream` serializes each value as a stdlib-closed normal form (SCN) tinct expression — a single-line string that any downstream tinct program can parse and evaluate. `-i stream` reads a lazy `[Seq Expression]` from stdin, one SCN record per line, with true O(1)-memory streaming. Two tinct programs connected by a stream pipe are as composable as a single program.

```sh
tinct run -i stream -o stream filter.llt < spans.llt-stream \
  | tinct run -i stream analyze.llt
```

`emit v` sends a value to the `%emit` output channel. The output formatter drains `%emit` concurrently and writes each received value to stdout.

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

### Source formatter

`tinct fmt` — idempotent formatter that canonicalizes whitespace and layout. `--check` mode for CI, `--in-place` for editor integration.

`tinct fmt --oneline` / `--nospaces` / `--minimize` — tinct-hosted compact formatter modes via `stdlib/formatter/compact.llt`. Produces minified output for diffing, embedding, and tooling pipelines.

### Runtime reflection

`[describe f]` returns a dict with the function's signature, doc string, annotation, and source AST. `[ast-of f]` returns the full body as a tinct dict using the canonical `[type: "..." span: [...] ...]` schema. Enables docgen and metaprogramming without a separate tooling layer.

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
        [map [fn [let u] u.name]
            [filter [fn [let u] [and u.active [= u.role "admin"]]] users]]]
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
just ci             # Run full CI pipeline
just --list         # See all commands
```

### Using Cargo (Native)

If you have Rust installed:

```bash
cargo build --release
cargo test
cargo run -- run samples/basic.llt
cargo run -- run -o llt samples/basic.llt        # Tinct display format
echo '{"x": 1}' | cargo run -- run -i json -    # Read Tinct from stdin with JSON input
cargo run -- fmt samples/basic.llt               # Format source file (stdout)
cargo run -- fmt --in-place samples/basic.llt    # Format source file in place
cargo run -- fmt --check samples/basic.llt       # Check formatting without writing
cargo run --features lsp -- lsp                  # Start LSP server (stdio)
```

## Testing

`just test` runs unit tests and corpus tests. `just test-corpus` runs only the file-based corpus suite.

Corpus tests live in `tests/corpus/` — each `.llt-eval` file contains a tinct program and optional expected output after a `===` delimiter:

```text
[key: "value"]
===
Dict({"key": String("value")})
```

Add a test by creating a `.llt-eval` file in the appropriate subdirectory and running `just test-corpus`.

## Requirements

### Containerized Workflow

- **just** -- Command runner ([install](https://github.com/casey/just))
- **podman** or **docker** -- Container runtime

### Native Workflow

- **Rust** 1.85+ -- ([install](https://rustup.rs))

## Documentation

- **[doc/](doc/index.md)** -- Language specification (17 chapters): syntax, data model, functions, type system, evaluation, stdlib, tooling, examples, internals

## Naming Conventions

### Builtin and stdlib function names

Two-word names use a hyphen. The convention depends on the relationship
between the two words:

**Domain-first** — when operating on an existing value of a known type,
the domain/type/protocol comes first, the verb second:

```text
str-find      str-length    str-chars     str-repeat
bytes-find    bytes-of      bytes-equal?
timestamp-add timestamp-diff timestamp-year
http-get      tls-layer     quic-session
dir-cap       net-cap       tag-of        type-of
```

**Verb-first** — when constructing or converting *to* a domain type
(the input is not yet that type), the verb comes first:

```text
parse-timestamp   format-timestamp
load-tz           from-json
write-atomic
```

The dividing line: if the primary input *is already* the domain type,
domain-first. If the function *produces* that type from something else,
verb-first.

Single-word builtins are always verbs: `map`, `filter`, `open`,
`connect`, `emit`, `reverse`, `sort`.

### File and module names

stdlib files use `kebab-case.llt`. Source files use `snake_case.rs`.

## Development Workflow

Features move through these stages in order:

1. **Design** — explore the problem, evaluate alternatives
2. **Whatif doc** (`doc/whatif/`) — write a proposal covering current state, what the feature provides, design, phased adoption, and references
3. **Accept & update spec** — mark the whatif `State: Accepted`, verify prerequisites are met or scheduled, run a design review for proposals touching formal semantics, incorporate the design into the relevant `doc/` chapters (present tense; the spec describes tinct as it will be), and create tracker sprints for each adoption phase
4. **Sprint** — one or more tracker sprints tracking implementation tasks
5. **Complete** — sprint checked off; spec was already accurate

The `doc/` directory is the language spec. It describes the intended language, not the current implementation state. Implementation follows the spec.
