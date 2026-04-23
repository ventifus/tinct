# What If: tinct as a Templating Language

What would it take to use tinct for generating text output — config
files, documents, serialized formats — beyond its current JSON output?

## Current State

tinct produces structured data. The CLI outputs JSON (`--format json`,
default) or LLT display format (`--format llt`). There is no mechanism
for:

- Producing text output (YAML, TOML, plain text) from structured data
- Embedding tinct code blocks in prose documents (Markdown, READMEs)
- Rendering templates where tinct computes dynamic values

The pipeline model (`---` separators, `$$` threading) processes tinct
files end-to-end. The output is always a single materialized value
serialized as JSON.

### Related Capabilities

- **`$str`** concatenates values into strings — text generation within
  tinct
- **`$include`** loads other `.llt` files — composition of tinct code
- **`$from-json`** parses JSON strings — input parsing, not output
- **String interpolation** (`doc/whatif/string-interpolation.md`) —
  proposed `i"Hello $name"` syntax for ergonomic string building

## Two Polarities

The research surfaces a fundamental axis: **which language is the host?**

### Template Polarity: Code in Prose

The host document is the target format (HTML, YAML, conf). Code
snippets are embedded via delimiters (`{{ }}`, `<%= %>`). The template
engine splices evaluated code into the text.

**Examples:** Jinja2, Mustache/Handlebars, ERB, Go `text/template`.

**Characteristic:** Output format is visible in the template. Authors
work in the target format's idiom. But: stringly typed, no format
awareness, injection-prone.

### Data-First Polarity: Structured Data → Serializer

The host is the programming language. Output is structured data
serialized at the boundary.

**Examples:** Jsonnet, Nix, Dhall, CUE — and **tinct today**.

**Characteristic:** Type-safe, format-aware serialization. The
serializer handles quoting and escaping. But: text-heavy output
requires verbose string building.

### Literate Polarity: Prose in Code

The host is human-readable prose. Complete code is embedded in named
chunks. `tangle` extracts code; `weave` produces documentation.

**Examples:** Knuth (1984), noweb, Jupyter notebooks.

**Characteristic:** Code follows explanation order, not execution
order. tinct's `---` pipeline already creates a quasi-literate
structure — computation stages are visually separated, each
independently understandable.

### Comparison

| Dimension | Template | Data-First | Literate |
|-----------|----------|-----------|----------|
| Host | Target format | Programming language | Prose |
| Output model | String concat | Data → serializer | Chunks → tangle/weave |
| Type safety | None | Structured | Varies |
| Best for | Sparse computed values | Complex transformations | Documentation |

tinct extends along **two** of these axes: data-first serialization
(Part 1) and literate evaluation (Part 2). Template-polarity embedding
is deferred — if tinct formatters plus `$str` (and eventually `i"..."`)
cover the need, Jinja-style templates may never be needed.

---

## Part 1: Formatters as Pipeline Programs

Formatters are ordinary tinct programs. A formatter receives structured
data via `$$`, produces a string, and calls `$emit` to send it to
stdout. The CLI accepts multiple files and pipelines them — each file's
output becomes `$$` for the next.

```bash
# Pipeline: data program → formatter program
tinct eval config.llt stdlib/fmt/yaml.llt

# Inline
tinct eval -e '[call $emit [call $to-yaml [port: 8080  host: "localhost"]]]'
```

### `$emit` Builtin

A Rust builtin that writes a value directly to stdout, bypassing JSON
serialization. Output encoding is determined by value type:

- `$emit` on `String` → writes UTF-8 text
- `$emit` on `Bytes` (future) → writes raw binary
- Returns `Null`
- Multiple `$emit` calls append to stdout sequentially

If `$emit` is never called during evaluation, the final pipeline value
is JSON-serialized to stdout as today (backwards compatible).

### Multi-File Pipeline

`tinct eval` accepts a list of `.llt` files. Each file is a pipeline
stage: file₁ evaluates, its output becomes `$$` for file₂, and so on.

```bash
# Single file (existing behavior, unchanged)
tinct eval config.llt

# Two-stage pipeline
tinct eval config.llt stdlib/fmt/yaml.llt

# Three-stage pipeline
tinct eval raw.llt transform.llt stdlib/fmt/toml.llt
```

This is equivalent to concatenating files with `---` separators but
allows separate files to be composed at the CLI level. No new
`--format` flags — output format is determined by which formatter
program is in the pipeline.

### Standard Formatters

Ship in `stdlib/fmt/` as tinct programs:

- `yaml.llt` — YAML 1.2 serializer
- `toml.llt` — TOML serializer
- `json-pretty.llt` — indented JSON (alternative to default compact)
- `env.llt` — `KEY=VALUE` for `.env` files
- `ini.llt` — INI format
- `csv.llt` — CSV from list-of-dicts

Each formatter is both a standalone pipeline stage and a function
importable via `$include`:

```lisp
# stdlib/fmt/yaml.llt — YAML formatter (simplified)

to-yaml-value: [fn [val indent]
  [call $cond
    [call $null? $val] "null"
    [call $bool? $val] [call $str $val]
    [call $int? $val]  [call $str $val]
    [call $float? $val] [call $str $val]
    [call $str? $val]  [call $yaml-quote-string $val]
    [call $dict? $val] [call $yaml-dict $val $indent]
    "null"]]

to-yaml-dict: [fn [d indent]
  [call $join "\n" [call $map [fn [entry]
    [call $str
      [call $repeat $indent " "]
      $entry.key ": "
      [call $to-yaml-value $entry.value [call $+ $indent 2]]]]
    [call $entries $d]]]]

to-yaml: [fn [val] [call $to-yaml-value $val 0]]

---

[call $emit [call $to-yaml $$]]
```

Formatters compose with tinct's existing mechanisms:

```lisp
# Format a subset
[call $emit [call $to-yaml [call $select $$ "server" "logging"]]]

# Custom wrapper
[call $emit [call $str "---\n" [call $to-yaml $$] "\n---\n"]]
```

### Prerequisites

- **Type predicates** (`$int?`, `$str?`, `$dict?`, etc.) — formatters
  must inspect value types to dispatch during serialization. See
  `doc/whatif/type-predicates.md`.
- **`$str`** — already implemented, sufficient for text building.
- **Structural contracts** (`doc/whatif/structural-contracts.md`) —
  formatters can declare expected input shape via `$$@Type`.

### Why Formatters in tinct

1. **Dogfooding.** Implementing YAML/TOML serialization in tinct tests
   the language's expressiveness. If tinct can't express a YAML
   serializer cleanly, that's a signal about language gaps worth
   fixing.

2. **User-extensible.** Anyone can write a formatter — `fmt/nginx.llt`,
   `fmt/mylog.llt` — no Rust code, no recompilation.

3. **Pipeline-native.** Formatters are pipeline stages, not CLI flags.
   Data flows in via `$$`, text flows out via `$emit`.

4. **`$emit` unifies text and binary.** Output encoding is a
   tinct-level concern, not a CLI flag. No `--format text/yaml/toml`
   flags needed.

5. **Performance escape hatch.** If a formatter becomes a bottleneck,
   promote it to a Rust builtin without changing the API. The Jsonnet
   model: `std.manifestYamlDoc()` started as stdlib, later optimized
   in C++.

---

## Part 2: Literate tinct (Code Blocks in Markdown)

Embed tinct code blocks in Markdown. A `tinct literate` command
extracts and evaluates the tinct blocks, optionally rendering results
back into the document.

````markdown
# Server Configuration

The server listens on the configured port.

```tinct
[
  port: 8080
  workers: 4
  hostname: api.example.com
]
```

## Derived Values

Worker pool size is twice the worker count:

```tinct
pool-size: [call $* $$.workers 2]
```
````

### Extraction Modes

1. **`tangle`** — extract tinct code blocks, evaluate as a pipeline
   (blocks are sequential documents, like `---` separation), output
   the final value as JSON. The prose is discarded.

   ```bash
   tinct literate tangle config.md
   # outputs: {"port": 8080, "workers": 4, ...}
   ```

2. **`weave`** — evaluate tinct blocks, render results inline in the
   Markdown, produce a document with computed values filled in. Requires
   a convention for marking where results appear in prose.

   ```bash
   tinct literate weave report.md > report-rendered.md
   ```

3. **`eval`** — extract and evaluate, ignore prose. Equivalent to
   concatenating all code blocks with `---` and running `tinct eval`.

   ```bash
   tinct literate eval config.md
   ```

### Pipeline Mapping

tinct's `---` pipeline model maps directly to multiple code blocks.
Each code block is a pipeline stage — its output becomes `$$` for the
next block. Prose between blocks serves as documentation for the
transformation steps.

This mirrors Knuth's literate programming insight: code follows
explanation order, not execution order. With tinct, the explanation
order IS the execution order (pipeline stages run sequentially), so
`tangle` and `eval` produce the same result.

### Formatter Integration

Literate mode composes with Part 1 formatters. The last code block
can call `$emit`:

````markdown
# Generate YAML Config

```tinct
[port: 8080  hostname: api.example.com]
```

## Output

```tinct
[call $emit [call $to-yaml $$]]
```
````

```bash
tinct literate eval config.md
# emits YAML to stdout
```

### Why Literate tinct

1. **Documentation and code co-located.** READMEs, runbooks, and
   reports with executable tinct examples.

2. **Markdown is universal.** Rendered by GitHub, editors, IDEs.
   Code blocks get syntax highlighting.

3. **Natural for reports.** Prose wraps computed data — statistics,
   tables, version numbers — in a readable document.

4. **Executable examples.** Documentation that can be run and
   verified, not just read.

---

## Phased Adoption

### Phase 1: `$emit`, Multi-File Pipeline, and Type Predicates

Three prerequisites that enable Part 1:

- **`$emit` builtin** — Rust builtin that writes to stdout. Takes a
  `String` → writes UTF-8. Returns `Null`. When `$emit` is called,
  the CLI suppresses default JSON output.

- **Multi-file pipeline** — `tinct eval` accepts multiple `.llt` files.
  Each file's output becomes `$$` for the next.

- **Type predicates** — `$int?`, `$float?`, `$str?`, `$bool?`,
  `$null?`, `$dict?`, `$fn?`. See `doc/whatif/type-predicates.md`.

### Phase 2: Standard Formatters

Ship `stdlib/fmt/` with tinct-implemented formatters. Writing these
exercises and validates tinct's string handling, recursion, type
predicates, and composition patterns.

### Phase 3: String Interpolation

Add `i"..."` string interpolation to make formatters more ergonomic:

```lisp
# Before
[call $str $indent $key ": " [call $quote-yaml $val] "\n"]

# After
i"$indent$key: ${[call $quote-yaml $val]}\n"
```

Not required for correctness — `$str` is sufficient.

### Phase 4: Literate Mode

Add `tinct literate` with `tangle`, `weave`, and `eval` subcommands.
Independent of Phases 1–3.

### Prerequisites

- **Phase 1:** No dependencies beyond current codebase.
- **Phase 2:** Phase 1 complete.
- **Phase 3:** Independent of Phase 2 (can run in parallel).
- **Phase 4:** Independent of all other phases.

### Trigger

Phase 1: any use case requires non-JSON text output from tinct, or
pattern matching work begins (type predicates are shared).

Phase 2: users need YAML/TOML output from tinct data.

Phase 4: documentation-driven development becomes a tinct workflow,
or users want executable examples in docs.

## References

**Data-first generation:**
- Jsonnet: `std.manifestYamlDoc()`, `std.manifestJson()` — structured
  data serialized at the boundary. stdlib functions later optimized
  in C++ for performance.
- Nix: `pkgs.writeText`, `lib.generators.toYAML` — file generation as
  build artifacts.
- Dhall: `dhall-to-yaml`, `dhall-to-json` — total language with
  guaranteed termination, serialized output.
- CUE: Lattice-based constraint unification with `tool/exec` text
  rendering escape hatch.

**Template engines:**
- Ronacher, A. (2008). Jinja2. Template inheritance, autoescaping,
  sandboxed execution.
- Wanstrath, C. (2009). Mustache. Logic-less templates. Enforced
  separation of data preparation from presentation.

**Literate programming:**
- Knuth, D.E. (1984). "Literate programming." *The Computer Journal*,
  27(2), 97–111. — Code in explanation order, extracted by `tangle`.
- Ramsey, N. (1994). "Literate programming simplified." *IEEE Software*,
  11(5), 97–105. — noweb: language-independent literate programming.
- Jupyter/IPython notebooks — modern literate programming with
  interleaved code cells and prose.

**Anti-patterns:**
- HashiCorp. "Terraform and Jinja2." — Quoting fragility when
  generating HCL from templates.
- Ansible community. "YAML + Jinja2 gotchas." — String-vs-structure
  confusion in templated YAML.
