# Templating

## Overview

tinct generates text output — config files, documents, serialized formats — through two coordinated mechanisms: data-first formatters (Part 1) and literate evaluation (Part 2). Template-polarity embedding (Part 3) is analyzed below; tinct does not adopt it because string interpolation and data-first formatters are sufficient.

1. **Text output from structured data.** `tinct eval config.llt fmt/yaml.llt` produces YAML. The formatter is a tinct program, not a CLI flag.

2. **Composable pipelines.** Multi-file pipeline at the CLI level: data flows through transformation stages, each independently testable.

3. **User-extensible formatters.** Anyone can write a formatter — `fmt/nginx.llt`, `fmt/mylog.llt` — no Rust code, no recompilation.

4. **Executable documentation.** Markdown files with embedded tinct code blocks that can be extracted and evaluated.

5. **Dogfooding.** Implementing YAML/TOML serialization in tinct tests the language's expressiveness and surfaces gaps.

## Design

The design has three coordinated parts: data-first formatters (Part 1), literate evaluation (Part 2), and an analysis of template-polarity embedding (Part 3). tinct adopts Parts 1 and 2. Part 3 is analyzed thoroughly below and rejected — string interpolation and data-first formatters are sufficient.

### Design Space: Three Polarities

Templating sits on a spectrum from "code generates text" to "text contains code." Three design points occupy this spectrum, each with different trade-offs for tinct:

| Dimension | Data-First | Template | Literate |
|-----------|-----------|----------|----------|
| Host | Programming language | Target format | Prose document |
| Code role | Primary — computes structure | Secondary — fills holes | Primary — explained by prose |
| Output model | Data → serializer | Text with interpolated values | Chunks → tangle/weave |
| Type safety | Full (structured data) | None (string concatenation) | Full within code blocks |
| Best for | Complex transformations | Sparse dynamic values | Documentation with live code |
| tinct fit | Natural | Awkward (syntax friction) | Natural (pipeline model) |
| Precedent | Jsonnet, Dhall, CUE | Jinja2, Mustache, ERB | noweb, Jupyter, R Markdown |

tinct extends along **two** of these axes: data-first serialization (Part 1) and literate evaluation (Part 2). Template-polarity embedding (Part 3) is analyzed and rejected — see §Part 3.

#### Same Task, Three Styles

Generating a YAML config file from data illustrates the trade-offs.

**Data-first (tinct's recommended approach):**

```tinct
# config.llt — pure data, separate formatter
[
  server: [
    port: base.port
    host: base.host
    workers: [* base.cores 2]
  ]
  logging: [
    level: [if base.debug "debug" "info"]
  ]
]

---

[emit [to-yaml %]]
```

**Template-polarity (Jinja-style, hypothetical):**

```yaml
# config.yaml.tinct — target format with embedded expressions
server:
  port: {{ base.port }}
  host: {{ base.host }}
  workers: {{ [* base.cores 2] }}
logging:
  level: {{ [if base.debug "debug" "info"] }}
```

**Literate (Markdown with tinct blocks):**

````markdown
# Server Configuration

Port and host from base config. Workers scaled to 2x cores.

```tinct
[server: [
  port: base.port
  host: base.host
  workers: [* base.cores 2]
]]
```

## Logging

Debug mode enables verbose logging.

```tinct
[logging: [level: [if base.debug "debug" "info"]]]
---
[emit [to-yaml %]]
```
````

The template-polarity version is shorter for this example, but note the friction: `[* base.cores 2]` inside `{{ }}` inside YAML is three levels of syntax interacting. The data-first version keeps each concern in its own layer.

---

## Part 1: Formatters as Pipeline Programs

Formatters are ordinary tinct programs. A formatter receives structured data via `%`, produces a string, and calls `emit` to send it to stdout. The CLI accepts multiple files and pipelines them — each file's output becomes `%` for the next.

```bash
# Pipeline: data program -> formatter program
tinct eval config.llt stdlib/out/yaml.llt

# Inline
tinct eval -e '[emit [to-yaml [port: 8080  host: "localhost"]]]'
```

### `emit` Builtin

A Rust builtin that writes a value directly to stdout, bypassing JSON serialization.

**Syntax:**

```tinct
[emit value]         # write string to stdout
[emit value1]        # multiple calls append
[emit value2]
```

**Semantics:**

- `emit` on `String` writes UTF-8 text to stdout
- `emit` on `Bytes` (future) writes raw binary to stdout
- Returns `Null`
- Multiple `emit` calls append to stdout sequentially
- If `emit` is never called during evaluation, the final pipeline value is JSON-serialized to stdout as before (backwards compatible)

**Interaction with lazy evaluation.** `emit` is a side-effecting operation — it writes to stdout. In tinct's call-by-need model (Launchbury, 1993), side effects are observable only when a thunk is forced. `emit` must be called at the top level of a pipeline stage (not inside a lazy binding) to ensure deterministic output ordering. If `emit` appears in a lazy binding, the output timing depends on when that binding is forced — which may be never. The evaluator forces `emit` calls eagerly within the document's top-level expression, treating them as strict positions.

**Internal representation:**

```rust
// New builtin
fn builtin_emit(args: &[Value], ctx: &mut EvalContext) -> Result<Value> {
    let val = force(&args[0], ctx)?;
    match val {
        Value::String(s) => ctx.emit_sink.write_all(s.as_bytes())?,
        _ => return Err(EvalError::type_mismatch("String", &val)),
    }
    ctx.emitted = true;  // suppress default JSON output
    Ok(Value::Null)
}
```

### Multi-File Pipeline

`tinct eval` accepts a list of `.llt` files. Each file is a pipeline stage: file_1 evaluates, its output becomes `%` for file_2, and so on.

```bash
# Single file (existing behavior, unchanged)
tinct eval config.llt

# Two-stage pipeline
tinct eval config.llt stdlib/out/yaml.llt

# Three-stage pipeline
tinct eval raw.llt transform.llt stdlib/out/toml.llt
```

This is equivalent to concatenating files with `---` separators but allows separate files to be composed at the CLI level. No new `--format` flags — output format is determined by which formatter program is in the pipeline.

**Interaction with `include` caching.** Multi-file pipeline stages share the include cache (doc/09-documents.md §Document Pipeline). If `config.llt` includes `stdlib/lib.llt`, and `fmt/yaml.llt` also includes `stdlib/lib.llt`, the second include hits the cache. This is correct and intentional — include caching is deterministic for fixed filesystem state.

### Standard Formatters

Ship in `stdlib/out/` as tinct programs:

- `yaml.llt` — YAML 1.2 serializer
- `toml.llt` — TOML serializer
- `json-pretty.llt` — indented JSON (alternative to default compact)
- `env.llt` — `KEY=VALUE` for `.env` files
- `ini.llt` — INI format
- `csv.llt` — CSV from list-of-dicts

Each formatter is both a standalone pipeline stage and a function importable via `include`:

```tinct
# stdlib/out/yaml.llt — YAML formatter (simplified)

to-yaml-value: [fn [val indent]
  [cond
    [null? val] "null"
    [bool? val] [str val]
    [int? val]  [str val]
    [float? val] [str val]
    [str? val]  [yaml-quote-string val]
    [dict? val] [yaml-dict val indent]
    "null"]]

to-yaml-dict: [fn [d indent]
  [join "\n" [map [fn [entry]
    [str
      [repeat indent " "]
      entry.key ": "
      [to-yaml-value entry.value [+ indent 2]]]]
    [entries d]]]]

to-yaml: [fn [val] [to-yaml-value val 0]]

---

[emit [to-yaml %]]
```

Formatters compose with tinct's existing mechanisms:

```tinct
# Format a subset
[emit [to-yaml [select % "server" "logging"]]]

# Custom wrapper
[emit [str "---\n" [to-yaml %] "\n---\n"]]
```

### Why Formatters in tinct

1. **Dogfooding.** Implementing YAML/TOML serialization in tinct tests the language's expressiveness. If tinct can't express a YAML serializer cleanly, that's a signal about language gaps worth fixing.

2. **User-extensible.** Anyone can write a formatter — `fmt/nginx.llt`, `fmt/mylog.llt` — no Rust code, no recompilation.

3. **Pipeline-native.** Formatters are pipeline stages, not CLI flags. Data flows in via `%`, text flows out via `emit`.

4. **`emit` unifies text and binary.** Output encoding is a tinct-level concern, not a CLI flag. No `--format text/yaml/toml` flags needed.

5. **Performance escape hatch.** If a formatter becomes a bottleneck, promote it to a Rust builtin without changing the API. The Jsonnet model: `std.manifestYamlDoc()` started as stdlib, later optimized in C++.

---

## Part 2: Literate tinct (Code Blocks in Markdown)

Embed tinct code blocks in Markdown. A `tinct literate` command extracts and evaluates the tinct blocks, optionally rendering results back into the document.

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
pool-size: [* %.workers 2]
```
````

### Extraction Modes

1. **`tangle`** — extract tinct code blocks, evaluate as a pipeline (blocks are sequential documents, like `---` separation), output the final value as JSON. The prose is discarded.

   ```bash
   tinct literate tangle config.md
   # outputs: {"port": 8080, "workers": 4, ...}
   ```

2. **`weave`** — evaluate tinct blocks, render results inline in the Markdown, produce a document with computed values filled in. Requires a convention for marking where results appear in prose.

   ```bash
   tinct literate weave report.md > report-rendered.md
   ```

3. **`eval`** — extract and evaluate, ignore prose. Equivalent to concatenating all code blocks with `---` and running `tinct eval`.

   ```bash
   tinct literate eval config.md
   ```

### Semantics

**Block extraction.** The `tangle` and `eval` modes extract code blocks tagged with `tinct` (or `llt`) from the Markdown source. Each block becomes a pipeline document, separated by implicit `---` boundaries. `%` threads between blocks in document order.

**Weave rendering.** The `weave` mode requires a convention for marking result positions. Two options:

```markdown
The port is: <!-- tinct: %.port -->
The port is: `{= %.port}`
```

The weave processor evaluates the expression and replaces the marker with the rendered result.

**Scope.** All code blocks within a single Markdown file share a pipeline scope — earlier blocks' bindings are visible to later blocks (via `%` threading). This matches tinct's `---` pipeline semantics.

### Pipeline Mapping

tinct's `---` pipeline model maps directly to multiple code blocks. Each code block is a pipeline stage — its output becomes `%` for the next block. Prose between blocks serves as documentation for the transformation steps.

This mirrors Knuth's (1984) literate programming insight: code follows explanation order, not execution order. With tinct, the explanation order IS the execution order (pipeline stages run sequentially), so `tangle` and `eval` produce the same result.

### Formatter Integration

Literate mode composes with Part 1 formatters. The last code block can call `emit`:

````markdown
# Generate YAML Config

```tinct
[port: 8080  hostname: "api.example.com"]
```

## Output

```tinct
[emit [to-yaml %]]
```
````

```bash
tinct literate eval config.md
# emits YAML to stdout
```

### Why Literate tinct

1. **Documentation and code co-located.** READMEs, runbooks, and reports with executable tinct examples.

2. **Markdown is universal.** Rendered by GitHub, editors, IDEs. Code blocks get syntax highlighting.

3. **Natural for reports.** Prose wraps computed data — statistics, tables, version numbers — in a readable document.

4. **Executable examples.** Documentation that can be run and verified, not just read.

---

## Part 3: Template-Polarity Embedding (Analysis)

Template-polarity embedding — placing tinct expressions inside a host document in the target format — is the Jinja2/Mustache/ERB model. The host document is mostly static text; escape sequences mark where dynamic values are computed. This section analyzes what this approach looks like for tinct and why it is not adopted.

### What Template Embedding Would Look Like

A template file is written in the target format with tinct expressions delimited by markers. Two delimiter styles are natural:

**Expression delimiters** — interpolate a value:

```yaml
# {{ expr }} style (Jinja-like)
port: {{ config.port }}
workers: {{ [* config.cores 2] }}
```

**Block delimiters** — control flow:

```yaml
# {% block %} style (Jinja-like)
{% [if config.debug] %}
logging:
  level: debug
  verbose: true
{% [else] %}
logging:
  level: info
{% [end] %}
```

**Processing model:**

```bash
tinct template config.yaml.tinct --data config.llt
```

The template processor:

1. Parses the host document as raw text
2. Extracts tinct expressions from delimiters
3. Evaluates expressions against data from `--data` or `%`
4. Converts results to strings and interpolates into the text
5. Emits the resulting text to stdout

### Syntax Friction

tinct's syntax creates friction inside template delimiters that languages like Python (Jinja) or Ruby (ERB) do not face:

| Operation | Jinja2 (Python) | ERB (Ruby) | tinct |
|-----------|-----------------|------------|-------|
| Multiply | `{{ cores * 2 }}` | `<%= cores * 2 %>` | `{{ [* cores 2] }}` |
| Conditional | `{{ "debug" if debug else "info" }}` | `<%= debug ? "debug" : "info" %>` | `{{ [if debug "debug" "info"] }}` |
| String concat | `{{ name + ".log" }}` | `<%= name + ".log" %>` | `{{ [str name ".log"] }}` |
| Field access | `{{ config.port }}` | `<%= config.port %>` | `{{ config.port }}` |

Field access is clean (`config.port`), but computation is verbose. The `[fn args]` syntax — explicit and unambiguous for a standalone language — becomes noisy when embedded in another format. Every template expression beyond simple variable interpolation pays a syntax tax.

String interpolation (`i"..."` from `doc/whatif/string-interpolation.md`) addresses this at the *expression* level. But template-polarity embedding operates at the *document* level — the entire file is the template, not just individual strings.

### When Template Embedding Is Better

Template-polarity embedding outperforms data-first formatters when:

1. **The output is 95%+ static.** A 200-line nginx.conf with 5 variable substitutions is better expressed as the nginx.conf itself with 5 `{{ }}` markers than as a tinct program that constructs 200 lines of text.

2. **Domain experts own the file.** An ops engineer who knows nginx.conf syntax but not tinct can read and edit a template directly. A data-first formatter requires understanding tinct.

3. **Format fidelity matters.** Templates preserve exact whitespace, comments, and formatting of the target format. Data-first formatters reconstruct formatting from structured data, which may not match expectations.

4. **The target format is unstructured.** Arbitrary text (log messages, email bodies, CLI output) has no natural data-first representation. Templates handle free-form text naturally.

### When Data-First Is Better

Data-first formatters outperform template embedding when:

1. **The output is highly computed.** When most values are derived from transformations, the template becomes more `{{ }}` than text. At that point, the template format adds noise rather than clarity.

2. **Multiple output formats.** The same data serialized as YAML, TOML, and JSON requires three templates but one data program with three formatters.

3. **Type safety matters.** Templates concatenate strings — a type error in a template expression produces malformed output silently. Data-first formatters operate on typed structured data; errors are caught before serialization.

4. **The output structure is complex.** Deeply nested YAML with conditional sections, merged defaults, and computed keys is natural in tinct's data model but requires complex template logic (loops, conditionals, nesting) that recreates half a programming language inside the template — the "inner platform" anti-pattern that Greenspun's tenth rule warns about.

### Template Embedding vs. String Interpolation

String interpolation (`i"..."`) is micro-level template embedding — tinct expressions inside a string literal. Document-level template embedding (Jinja-style) is the same mechanism scaled up to entire files.

| Level | Mechanism | Scope | Type safety |
|-------|-----------|-------|-------------|
| Micro | `i"port: $config.port"` | One string | Desugars to `str` (typed) |
| Macro | `port: {{ config.port }}` | Entire file | String concat (untyped) |

If `i"..."` plus data-first formatters cover the use cases, document-level template embedding adds complexity without proportional value. The trigger for template embedding is when users need to maintain files in foreign formats (nginx.conf, Dockerfile, Makefile) where the format itself is the primary artifact and tinct provides only a few dynamic values.

---

## Template Embedding vs. Literate Programming

Template embedding and literate programming both embed code inside a host document, but they serve opposite purposes.

### Structural Parallel

Both approaches have the same physical structure: a text document with code snippets marked by delimiters. The difference is what the host document IS and what the code snippets DO.

| Dimension | Template Embedding | Literate Programming |
|-----------|-------------------|---------------------|
| Host document | Target output format | Prose documentation |
| Host purpose | IS the output (with holes filled) | Documents the program |
| Code purpose | Computes dynamic values | IS the program |
| Processing | Evaluate → interpolate into text | Tangle (extract code) or weave (render docs) |
| Output | Rendered target document | Executable code or rich documentation |
| Primary audience | Machine/consumer of the target format | Human reader |
| Code density | Sparse (mostly static text) | Dense (mostly code) |
| Code ordering | Dictated by target format structure | Dictated by explanation |
| Pipeline model | No natural expression | Maps directly to `---`/`%` |

### In tinct

tinct's literate mode (Part 2) maps naturally to the pipeline model. Each Markdown code block is a pipeline stage. `%` threads between blocks. Prose documents the transformation steps. The explanation order IS the pipeline order, which satisfies Knuth's (1984) insight that code should follow the order of human understanding.

Template embedding inverts tinct's model. Instead of tinct computing data and serializing it (data-first), or tinct code explained by prose (literate), tinct expressions are scattered inside a foreign format. The pipeline model — tinct's central organizing principle — has no natural expression in a template file.

### When They Overlap

The overlap occurs in **weave mode**: literate tinct's weave command evaluates code blocks and renders results into the Markdown document. This is structurally similar to template embedding — computed values appear in a host document. The difference is that weave produces documentation (Markdown with results), not deployment artifacts (YAML config files).

A literate tinct document that produces YAML via `emit` in its last code block combines both paradigms: prose documents the configuration decisions (literate), and the pipeline produces formatted output (data-first). Template embedding is not needed — the same result is achieved by composition of the other two approaches.

### Knuth's Distinction

Knuth (1984) distinguished between **tangle** (extract code for the compiler) and **weave** (produce typeset documentation). Both operate on the same source document. Template embedding has no weave analogue — a Jinja template produces only its rendered output, not documentation about itself.

This asymmetry reveals a deeper difference: literate programming treats code as a first-class artifact worth explaining. Template embedding treats code as incidental — a means to fill in values. For tinct, where data transformations ARE the interesting part (not the output format), literate mode is the more natural host for embedded code.

### Practical Comparison

The same task — a documented, parameterized config — in both styles:

**Template embedding:**

```yaml
# config.yaml.tinct
# Server configuration for {{ env }} environment
server:
  port: {{ base.port }}
  workers: {{ [* base.cores 2] }}
```

Produces YAML. The documentation ("Server configuration for...") is a comment in the target format — it may or may not survive processing, depending on whether the template processor preserves non-delimited text verbatim.

**Literate tinct:**

````markdown
# Server Configuration

This configures the server for the target environment. Worker
count is scaled to twice the available CPU cores per the capacity
planning guidelines in RFC-0042.

```tinct
[
  server: [
    port: base.port
    workers: [* base.cores 2]
  ]
]
---
[emit [to-yaml %]]
```
````

Produces YAML (via `emit`) AND is readable documentation. The prose explains *why* workers are 2x cores — context that a YAML comment cannot capture. The literate document serves dual duty: executable config generator and design rationale.

## Implementation

### CLI

`tinct eval` accepts multiple `.llt` files as pipeline stages. New `tinct literate` subcommand with `tangle`, `weave`, and `eval` modes for Markdown files. When `emit` is called, default JSON output is suppressed. Multi-file pipeline extends the existing argument parser. The `emit`-suppresses-JSON behavior requires a flag in the evaluation context.

### Evaluator (`src/eval.rs`)

`emit` is a Rust builtin with access to a write sink on `EvalContext`. `%` threads across file boundaries (previously only within `---` boundaries in a single file). `EvalContext` gains a write sink (e.g., `Box<dyn Write>`) and an `emitted: bool` flag. `emit` introduces a side-effecting builtin into an otherwise pure evaluator.

### Parser (`src/parser.rs`)

A Markdown extraction pass identifies ` ```tinct ` code blocks and extracts their content as sequential pipeline documents. The Markdown extraction is a preprocessing step before the existing parser — it produces concatenated tinct source with `---` separators, which the existing parser already handles.

### Type Checker (`src/typecheck.rs`)

Cross-document type checking extends to multi-file pipelines. Document N's inferred output type is compatible with document N+1's expected `%` type (or `%@Type` annotation from `doc/whatif/structural-contracts.md`). Within a single file, cross-document checking already exists for `---` boundaries. Extending to multi-file requires threading type information across file boundaries.

### Standard Library

`stdlib/out/` contains standard formatters: `yaml.llt`, `toml.llt`, `json-pretty.llt`, `env.llt`, `ini.llt`, `csv.llt`. New files, no changes to existing code.

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
  sandboxed execution. — Defines the modern template-polarity model:
  host document in target format, `{{ }}` expression delimiters,
  `{% %}` block delimiters. tinct's Part 3 analysis uses Jinja2 as
  the primary comparison point.
- Wanstrath, C. (2009). Mustache. Logic-less templates. Enforced
  separation of data preparation from presentation. — The extreme
  data-driven end of template embedding: templates cannot contain
  logic, only variable interpolation. Validates tinct's data-first
  philosophy by reaching the same conclusion (separate data from
  presentation) from the template side.
- Go standard library. "text/template" and "html/template." — Pipeline
  model inside templates (`{{ .Field | function }}`). Notable for
  bringing pipeline semantics into template syntax — the inverse of
  tinct's approach (tinct brings template output into pipeline
  semantics).
- Shopify. "Liquid template language." — Sandboxed template language
  for non-programmers. Safety through restriction rather than types.
  Demonstrates template embedding designed for domain experts who
  are not programmers — the primary use case where template-polarity
  outperforms tinct's data-first model.

**Literate programming:**
- Knuth, D.E. (1984). "Literate programming." *The Computer Journal*,
  27(2), 97-111. — Code in explanation order, extracted by `tangle`.
  tinct's pipeline stages map to Knuth's code chunks. The
  tangle/weave distinction — same source producing both executable
  code and documentation — has no analogue in template embedding,
  revealing a structural asymmetry between the two approaches.
- Ramsey, N. (1994). "Literate programming simplified." *IEEE Software*,
  11(5), 97-105. — noweb: language-independent literate programming.
  tinct's literate mode follows this philosophy — Markdown is the
  host, tinct code blocks are the chunks. noweb's language
  independence parallels tinct's format independence (any output
  format via formatters).
- Jupyter/IPython notebooks — modern literate programming with
  interleaved code cells and prose. The weave-mode analogue for
  tinct: evaluate code blocks, render results inline.
- Haskell. "Literate Haskell" (.lhs files). — Compiler-native
  literate mode: Haskell source embedded in LaTeX or Markdown with
  `>` line prefix. Demonstrates that literate mode can be a
  first-class feature of the language toolchain, not an external tool.

**Evaluation semantics:**
- Launchbury, J. (1993). "A natural semantics for lazy evaluation."
  *POPL*, pp. 144-154. — Call-by-need semantics. Relevant to `emit`'s
  interaction with lazy evaluation: side effects are only observable
  when thunks are forced.

**Domain-specific language embedding:**
- Hudak, P. (1996). "Building domain-specific embedded languages."
  *ACM Computing Surveys*, 28(4), 196-es. — The EDSL approach: embed
  the domain in the host language rather than the host language in
  the domain. tinct's data-first formatters follow this philosophy —
  YAML serialization is a tinct function, not tinct embedded in YAML.
- Czarnecki, K. & Eisenecker, U.W. (2000). *Generative Programming.*
  Addison-Wesley. Ch. 8-9. — Template metaprogramming and generative
  approaches. Distinguishes generation-time computation from
  output-time computation. tinct's pipeline model cleanly separates
  these concerns; template embedding conflates them.

**Anti-patterns:**
- HashiCorp. "Terraform and Jinja2." — Quoting fragility when
  generating HCL from templates.
- Ansible community. "YAML + Jinja2 gotchas." — String-vs-structure
  confusion in templated YAML. Both anti-patterns arise from
  template-polarity embedding of structured formats — the generated
  text must be valid YAML/HCL, but the template processor operates
  on strings, not structure. Data-first generation avoids this class
  of problem entirely.

**Typed template systems:**
- Chlipala, A. (2015). "Ur/Web: A Simple Model for Programming the
  Web." *POPL*, pp. 153-165. — Statically typed web templates with
  full type checking across template boundaries. Demonstrates that
  type-safe templates are possible but require deep language-level
  integration — not achievable by bolting templates onto an existing
  language.

**Cross-references:**
- `doc/whatif/string-interpolation.md` — `i"..."` prefix syntax for
  micro-level template embedding within tinct strings.
- `doc/whatif/type-predicates.md` — Runtime type tests needed by
  formatter programs for dispatch on value types.
- `doc/feature/macros.md` — Macro system that could generate template
  processing code at expansion time.
