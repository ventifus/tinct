# Implementation Roadmap

See DONE.md for the full history of completed sprints.

For future work beyond the active sprints below, see:
- `doc/whatif/index.md §Adopt Now` — features ready to implement (String Interpolation Phase 1, Let Binding, Structural Contracts Phase 1, ADTs Phase 1, Source Snippets, Circular Dep Error Paths, Eval Semantics Verification Phase 1)
- `doc/whatif/index.md §Wait for Trigger` — features with complete designs pending a concrete trigger

## I/O and Capabilities

See doc/whatif/io.md.

- [x] Accept io — see doc/whatif/io.md (State: Accepted — 2026-05-04)

### io-phase1: File Caps, emit, stdin, env

See doc/whatif/io.md §Phase 1.

- [ ] Add `Value::DirCap` wrapping `cap_std::fs::Dir` (`src/value.rs`)
- [ ] Add `Value::Handle` wrapping `Rc<RefCell<Box<dyn io::Read + io::Write>>>` (`src/value.rs`)
- [ ] Add `Value::RevocableDirCap` with `inner: DirCap` and `revoked: Rc<Cell<bool>>` (`src/value.rs`)
- [ ] Add `emitted: bool` and `env_allowlist: Option<HashSet<String>>` to `EvalContext` (`src/eval.rs`)
- [ ] Implement `dir-cap`, `open`, `narrow`, `revocable`, `slurp`, `write`, `lines`, `emit`, `env` builtins (`src/builtins.rs`)
- [ ] Modify `include` to take `DirCap` first arg; cache by `(st_dev, st_ino)` (`src/builtins.rs`, `src/eval.rs`)
- [ ] Inject `pwd`, `libdir`, `stdin` into root env at startup (`src/eval.rs`, `src/main.rs`)
- [ ] Add `--cap-fs`, `--no-pwd`, `--no-libdir`, `--no-stdin`, `--no-env`, `--allow-env`, `--libdir-path` CLI flags (`src/main.rs`)
- [ ] Create `stdlib/io.llt` with `read-file`, `write-file`, `append-file`, `read-lines`, `println` (`stdlib/io.llt`)
- [ ] Suppress default JSON output when `emitted == true` (`src/main.rs`)
- [ ] Update sandbox documentation for cap model flags (`doc/12-tooling.md`)
- [ ] Corpus tests for file I/O, emit, stdin, env, revocable caps (`tests/corpus/io/`)

### io-phase2: Network Caps, stdlib/net.llt

**Depends on:** `io-phase1`

See doc/whatif/io.md §Phase 2.

- [ ] Add `Value::NetCap` wrapping `Vec<NetCapEntry>` (`src/value.rs`)
- [ ] Define `NetCapEntry` enum with hostname, host:port, IPv4/IPv6 CIDR variants (`src/value.rs`)
- [ ] Implement `net-cap`, `connect`, `tls` builtins (`src/builtins.rs`)
- [ ] Add `rustls = "0.23"` dependency to `Cargo.toml` (`Cargo.toml`)
- [ ] Implement hostname and CIDR matching logic for NetCap allowlist (`src/builtins.rs`)
- [ ] Add `--cap-net` CLI flag with accumulation for same name (`src/main.rs`)
- [ ] Create `stdlib/net.llt` with `fetch`, `fetch-opts`, `http-parse-response`, `parse-url`, `http-format-request` (`stdlib/net.llt`)
- [ ] TLS: rustls integration with system CA store, hostname verification (`src/builtins.rs`)
- [ ] Corpus tests for TCP connect, TLS connections, NetCap allowlist matching (`tests/corpus/io/`)
- [ ] Error tests for connection denials, revoked caps (`tests/corpus/io/`)

### io-phase3: Atomic Writes, Streaming Fetch, Sandbox Hardening

**Depends on:** `io-phase2`

See doc/whatif/io.md §Phase 3.

- [ ] Implement atomic file writes via temp file + rename (`src/builtins.rs`)
- [ ] Add `write-atomic` stdlib function using temp + rename pattern (`stdlib/io.llt`)
- [ ] Enable streaming fetch response body via `lines` over socket handle (`stdlib/net.llt`)
- [ ] Harden `--no-pwd --no-stdin --no-env` enforcement (`src/eval.rs`, `src/main.rs`)
- [ ] Add error messages for missing caps (e.g., `open pwd ...` when `--no-pwd`) (`src/builtins.rs`)
- [ ] Corpus tests for fully sandboxed invocations and handle lifecycle (`tests/corpus/io/`)

### io-phase4: Cap Types in Type Checker

**Depends on:** `io-phase3`

See doc/whatif/io.md §Phase 4.

- [ ] Add `Type::DirCap`, `Type::NetCap`, `Type::Handle` variants (`src/types.rs`)
- [ ] Infer cap types in `infer_expr` for builtin calls (`src/typecheck.rs`)
- [ ] Update builtin signatures with cap types (`src/builtins.rs`)
- [ ] Corpus tests for cap type inference and errors (`tests/corpus/typecheck/`)

## Templating: Text Output and Formatters

See doc/whatif/templating.md.

- [x] Accept templating — see doc/whatif/templating.md (State: Accepted — 2026-05-04)

### templating-phase1: emit and Multi-File Pipeline

**Depends on:** `io-phase1`

See doc/whatif/templating.md §Phase 1.

- [ ] Accept multiple `.llt` files in `tinct eval` CLI argument parser (`src/main.rs`)
- [ ] Thread `%` across file boundaries — each file's output becomes `%` for next (`src/eval.rs`)
- [ ] Document `emit` semantics and lazy evaluation interaction (`doc/11a-builtins.md`)
- [ ] Document multi-file pipeline CLI behavior (`doc/09-documents.md`)
- [ ] Corpus tests for `emit` builtin and multi-file pipeline (`tests/corpus/`)

### templating-phase2: Standard Formatters

**Depends on:** `templating-phase1`

See doc/whatif/templating.md §Phase 2.

- [ ] Create `stdlib/fmt/` directory with base formatter pattern
- [ ] Implement `stdlib/fmt/yaml.llt` — YAML 1.2 serializer using type predicates + recursion
- [ ] Implement `stdlib/fmt/toml.llt` — TOML serializer
- [ ] Implement `stdlib/fmt/json-pretty.llt` — indented JSON alternative to compact default
- [ ] Implement `stdlib/fmt/env.llt` — `KEY=VALUE` for `.env` files
- [ ] Implement `stdlib/fmt/csv.llt` — CSV from list-of-dicts
- [ ] Document standard formatters in `doc/11-stdlib.md`
- [ ] Integration tests: data program | formatter produces expected output (`tests/corpus/`)

### templating-phase3: String Interpolation

See doc/whatif/templating.md §Phase 3 and doc/whatif/string-interpolation.md.

- [ ] Add `i"..."` token to lexer — detect `i` prefix before `"` (`src/lexer.rs`)
- [ ] Parse `i"..."` as `InterpolatedString` AST node (`src/parser.rs`, `src/ast.rs`)
- [ ] Desugar `InterpolatedString` to `[str ...]` call in desugar pass (`src/desugar.rs`)
- [ ] Handle `$ident` simple interpolation and `${expr}` expression interpolation in parser
- [ ] Update formatter to preserve `i"..."` strings (idempotency) (`src/formatter.rs`)
- [ ] Corpus tests for string interpolation (`tests/corpus/`)
- [ ] Document string interpolation syntax (`doc/02-syntax.md`)

### templating-phase4: Literate Mode

See doc/whatif/templating.md §Phase 4.

- [ ] Add `tinct literate` subcommand to CLI (`src/main.rs`)
- [ ] Implement `tinct literate tangle` — extract ` ```tinct ` code blocks as `---`-separated pipeline (`src/literate.rs`)
- [ ] Implement `tinct literate eval` — tangle + evaluate + print result (`src/literate.rs`)
- [ ] Implement `tinct literate weave` — evaluate blocks, render results inline via markers (`src/literate.rs`)
- [ ] Thread `%` between code blocks in document order
- [ ] Corpus tests for tangle and eval modes (`tests/corpus/`)
- [ ] Document literate mode semantics (`doc/09-documents.md`)

## Template-Polarity Research

- [ ] Research template-polarity embedding — evaluate after Phases 1-3 adoption whether `emit` + `i"..."` + formatters cover use cases or whether `tinct template` with `{{ expr }}` delimiters is needed. See doc/whatif/templating.md §Part 3.
