# What If: Template-Polarity Embedding for tinct

**State:** Proposal

What would it take to let tinct serve as a lightweight preprocessor for
foreign-format files — nginx.conf, Dockerfile, Makefile, systemd units —
where the file itself is the primary artifact and tinct provides a handful
of dynamic values?

## Current State

tinct's templating story covers three modes, all implemented:

- **Data-first formatters** (`emit` + `stdlib/out/yaml.llt` etc.): tinct
  computes structure, a formatter serializes it to the target format.
- **String interpolation** (`i"..."`): micro-level variable substitution
  inside tinct string literals.
- **Literate mode** (`tinct literate tangle|eval|weave`): tinct code blocks
  embedded in Markdown prose documents.

These cover the case where **tinct is the primary language** and the output
format is secondary. They do not cover the inverse: files where **the target
format is primary** and tinct provides a few dynamic substitutions.

```tinct
# Data-first: tinct builds the structure
[
  server: [port: config.port  host: config.host]
  workers: [* config.cores 2]
]
---
[emit [to-yaml %]]
```

A 200-line nginx.conf with five variable substitutions has the opposite
shape: mostly static text with a handful of computed values. Writing it
as a data-first tinct program requires reconstructing all 200 lines from
structured data — a significant overhead when the file format, ordering,
and comments must be preserved exactly.

### What's Missing

1. **No template-polarity mode.** There is no way to write a file
   primarily in a foreign format and embed tinct expressions in it.
2. **No `tinct template` subcommand.** No CLI entry point for processing
   a template file against a tinct data program.
3. **No expression delimiter syntax.** `{{ expr }}` and `{% block %}`
   are not currently recognized by any tinct tooling.

## Why Template-Polarity Matters for tinct

- **Foreign-format file ownership.** An ops engineer maintaining
  nginx.conf can keep it in nginx.conf syntax with five `{{ config.x }}`
  markers — no tinct knowledge required for the 195 static lines.
- **Format fidelity.** Templates preserve exact whitespace, comments, and
  field ordering of the target format. Data-first formatters reconstruct
  these from structure and may not match expectations.
- **Unstructured text targets.** Makefiles, Dockerfiles, shell scripts,
  and systemd units resist data modelling. Their structure is positional
  and format-specific. Templates handle these naturally.
- **Minimal-substitution cases.** When a file has ≤10 dynamic values and
  90%+ static content, template-polarity has much lower cognitive overhead
  than a full data-first tinct program.

## Design

Template-polarity embedding uses the **Jinja2 delimiter convention**:
`{{ expr }}` for value interpolation and `{% expr %}` for block control
flow. This convention is widely understood (Jinja2, Ansible, Django, Nunjucks,
Twig) and has no conflict with tinct syntax — tinct uses `[...]` for all
constructs and never uses `{` or `}`.

```nginx
# nginx.conf.tinct — template-polarity style
server {
    listen {{ config.port }};
    server_name {{ config.host }};
    worker_processes {{ config.workers }};

    {% [if config.ssl] %}
    ssl_certificate {{ config.cert_path }};
    ssl_certificate_key {{ config.key_path }};
    {% [end] %}

    location / {
        proxy_pass http://{{ config.upstream }};
    }
}
```

```bash
tinct template nginx.conf.tinct --data config.llt
# → renders nginx.conf to stdout with values substituted
```

### Expression Delimiters — `{{ expr }}`

The content inside `{{ }}` is a tinct expression evaluated in the context
of the data program's top-level dict. The result is converted to a string
via the `str` builtin and interpolated into the surrounding text.

```
{{ config.port }}              → "8080"
{{ [* config.cores 2] }}      → "8"
{{ [str config.host ":8080"] }} → "example.com:8080"
{{ [if config.debug "debug" "info"] }} → "debug"
```

No type checking on the interpolated value beyond `str` conversion — this
is the string-concat model. Type errors in the tinct expression (e.g.,
calling `+` on a string) produce an eval error before interpolation.

### Block Delimiters — `{% expr %}`

Block delimiters execute a tinct expression for its control-flow effect.
Two forms are supported:

```
{% [if condition] %}   ... text ...   {% [end] %}
{% [if condition] %}   ... text ...   {% [else] %}   ... text ...   {% [end] %}
```

The block body is static text (including nested `{{ }}` interpolations).
Block evaluation is eager — all `{% %}` blocks are resolved before
`{{ }}` interpolations are performed in a block's body.

### Processing Model

```bash
tinct template <template-file> [--data <llt-file>]
```

1. Read the template file as raw text
2. Parse: find `{{ }}` and `{% %}` delimiter pairs; extract tinct
   expressions as strings
3. If `--data` provided, evaluate the data program to get a dict; otherwise
   use an empty dict
4. Evaluate `{% %}` blocks to resolve conditionals — emit or suppress body
   text accordingly
5. Evaluate each `{{ }}` expression in the resulting text against the data
   dict; convert result to string
6. Write rendered text to stdout

The template processor is a new `src/template.rs` module. It uses the
existing tinct parser and evaluator for expression evaluation — no new
language semantics. The template scanning (finding `{{` and `}}` in
arbitrary host text) is a simple stateful scan, not a full lexer.

### Delimiter Safety

`{` and `}` do not appear anywhere in tinct syntax — all tinct constructs
use `[` and `]`. A valid tinct expression will never contain `{{` or `}}`,
making delimiter detection unambiguous even when tinct expressions are
embedded inside `{{ }}` markers. Literal `{{` or `}}` in the host text
can be escaped as `{{{` and `}}}` (one extra brace, Jinja2 convention).

### Interaction with `i"..."`

`i"..."` is micro-level template embedding (within a tinct expression).
Template-polarity is macro-level (the entire file is the template). They
are complementary: tinct expressions inside `{{ }}` may themselves use
`i"..."` for string construction.

```nginx
location {{ i"/api/$config.version" }} {
    proxy_pass {{ i"http://$config.upstream:$config.port" }};
}
```

## What Would Change

### CLI (`src/main.rs`)

**Current:** `tinct eval`, `tinct literate` subcommands.

**Proposed:** Add `tinct template <file> [--data <llt-file>]` subcommand.
Parse args, load data program if specified, call `template::render()`.

**Impact:** Minor — new subcommand, no changes to existing commands.

### Template Processor (`src/template.rs`, new file)

**Current:** Does not exist.

**Proposed:** `pub fn render(template: &str, data: &Value) -> Result<String, TemplateError>`.
Scans for `{{`/`}}` and `{%`/`%}` pairs. Parses tinct expressions from
delimiter contents using the existing parser. Evaluates against the data
dict using the existing evaluator. Returns rendered string.

**Impact:** New file, ~200-300 lines. No changes to existing evaluator or
parser — uses them as a library.

### Tooling (`doc/12-tooling.md`)

**Current:** Documents `eval`, `fmt`, `literate` subcommands.

**Proposed:** Add `tinct template` subcommand documentation.

**Impact:** Minor — documentation only.

## Prerequisites

- Phases 1-3 of `doc/whatif/completed/templating.md` complete (all done as
  of 2026-05-04).
- Existing tinct evaluator stable (no dependencies on future type system work).

## References

- Ronacher, A. (2008). *Jinja2 template engine.* — Defines the
  `{{ }}`/`{% %}` delimiter convention adopted here. Template
  inheritance, autoescaping, sandboxed execution.
- Ansible community. "YAML + Jinja2 gotchas." — Documents anti-patterns
  from template-polarity embedding of structured formats; validates the
  "≤10 substitutions" trigger heuristic.
- tinct `doc/whatif/completed/templating.md` §Part 3 — Original analysis
  of template-polarity embedding; this proposal is the follow-up
  evaluation after Phases 1-3 adoption.
