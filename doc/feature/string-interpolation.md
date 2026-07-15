# String Interpolation

## Overview

String interpolation reduces verbosity for multi-part string construction.
tinct uses `str` for string concatenation:

```tinct
greeting: [str "Hello " name ", you are " age " years old"]
```

Interpolated strings express the same thing more directly:

```tinct
greeting: i"Hello $name, you are $age years old"
```

Key benefits:

1. **Reduced verbosity.** `i"Hello $name"` vs `[str "Hello " name]` — fewer
   tokens, more readable.

2. **Formatter ergonomics.** Formatter code that builds strings becomes
   significantly more readable:

   ```tinct
   # Before
   [str indent key ": " val "\n"]

   # After (pre-compute call results, then interpolate)
   [yaml-val: [quote-yaml val]  result: i"$indent$key: $yaml-val\n"]
   ```

3. **LLM token efficiency.** `str` calls produce high token counts.
   Interpolation reduces token count for string-heavy code, improving LLM
   generation quality.

Implemented as `templating-phase3`: `i"..."` + formatter roundtrip (completed
2026-05-05). The `${expr}` expression interpolation form was removed in sprint
S-927 — only `$ident` variable references are supported.

## Supersession Notes

The `§Internal Representation` section below is the authoritative description of the current implementation.

## Design

A distinct `i"..."` prefix for interpolated strings keeps regular `"..."` strings
unchanged:

```tinct
# Regular string (no interpolation, current behavior)
"Hello name"           # literal string containing "name"

# Interpolated string
i"Hello $name"          # → "Hello Alice"
```

### Syntax

**Simple interpolation:** `$identifier` expands to a variable's value,
converted to a string via `str` semantics.

```tinct
i"Count: $n"                # simple variable
```

Only plain `$ident` is supported — dot access inside `$...` is not. Variable
names stop at `.` and other punctuation. Pre-compute dot-access values before
interpolating:

```tinct
# WRONG: only $config is interpolated; ".host" is appended literally
i"Host: $config.host"       # → "Host: {config-value}.host"

# CORRECT: pre-compute the value, then interpolate
[host: config.host]
i"Host: $host"              # → "Host: localhost"
```

**Escaping:** `$$` produces a literal `$` inside an interpolated string.

```tinct
i"Price: $$$amount"     # → "Price: $42"
```

### Semantics — Desugaring via `tmpl`

An interpolated string is transformed in the **desugar pass** (`src/desugar.rs`),
not at parse time. This is a pure syntactic transformation — no new evaluation
semantics:

```tinct
# Source
i"Hello $name, you are $age years old"

# Desugars to (in the desugar pass)
[tmpl "Hello $name, you are $age years old"]
```

The `tmpl` macro (defined in `stdlib/prelude.llt`) then runs at evaluation time —
lazily, when the interpolated string value is forced — and produces a
`[str-parts ...]` call that evaluates to the final interpolated string.

This desugaring preserves laziness: each interpolated segment is a normal
expression, evaluated on demand.

### Internal Representation

The lexer emits all strings (plain, interpolated, triple-quoted, triple-interpolated)
as a single unified `Token::StringLiteral { prefix: String, delimiter: String, content: String }`.
The content is stored raw — no escape processing, no `$ident` scanning, no indentation
stripping is done at lex time.

The desugar pass (`src/desugar.rs`) converts `StringLiteral` nodes:

- `prefix == ""`, single delimiter (`"`): plain string — kept as `SurfaceExpression::StringLiteral`.
- `prefix == "i"`, single delimiter: interpolated — `build_interpolated_string_node` scans content
  for `$$` (literal `$`) and `$ident` variable references, emits `[tmpl "template"]` call.
- `prefix == ""`, triple delimiter (`"""`): triple-quoted — wraps in `[unindent ...]` call.
- `prefix == "i"`, triple delimiter: both — wraps `[unindent [tmpl "template"]]`.

Lowering (`src/lower.rs`) handles escape processing in `process_escapes`: `\\`, `\"`, `\n`, `\t`,
`\r`, and unknown-escape pass-through. This applies only to single-quoted strings (`delimiter.len()
== 1`). Triple-quoted strings (`delimiter.len() >= 3`) pass content verbatim — `\n` inside
`"""..."""` is a literal backslash followed by `n`, not a newline.

Variable names in `$ident` patterns stop at whitespace, brackets, and common punctuation
(`,`, `.`, `!`, `?`), enabling natural text like `i"Hello $name, welcome!"` where the comma
is not part of the variable name.

Note: `tmpl` and `unindent` are required entries in any prelude that supports interpolated
and triple-quoted strings — they are part of the tinct Rust protocol (see D-3 for the formal
decision on whether to implement them as Rust builtins).

### Interaction with Lazy Evaluation

Because interpolated strings desugar to `[tmpl ...]` calls (which the `tmpl`
macro expands to string construction), they inherit tinct's lazy evaluation
semantics. Each interpolated segment becomes a thunk that is forced only when
the resulting string value is demanded. This is consistent with Launchbury
(1993) — the desugaring introduces no new evaluation forms.

### Interaction with Type Inference

`str` already accepts arguments of any type and coerces them to strings.
Interpolated strings inherit this behavior through the `tmpl` macro, which
ultimately produces string construction from variable references. No changes to
the type checker are needed — the expanded form is a `str-parts` call, which the
type checker already handles through standard string construction.

### Design Rationale

1. **No breaking change.** Double-quoted strings keep their current semantics.
   `"name"` remains a literal string. Interpolation is opt-in via the `i`
   prefix.

2. **Natural syntax.** tinct's `$` sigil for variables makes `i"Hello $name"`
   read naturally — `$name` already means "the value of name" everywhere else.

3. **Precedent.** Kotlin uses `"Hello $name"` and `"${expr}"` with the same
   `$`-sigil convention. Python f-strings (`f"Hello {name}"`) establish the
   prefix-based opt-in pattern. The `i` prefix is compact and unambiguous.

4. **Desugaring keeps it simple.** No new runtime semantics, no new type rules,
   no new evaluation strategy. The feature is entirely syntactic sugar over an
   existing builtin.

### Alternative Implementation: Macro-Based

If `doc/whatif/macros.md` ships, Phases 1–2 can be implemented via
`[macro tmpl ...]` rather than as a parser feature. The Rust change shrinks
to two steps: the lexer recognizes the `i"` prefix and emits the raw string
content as an opaque `IString` token (no scanning of `$` patterns); the parser
wraps it as `[tmpl "raw content"]`. All `$identifier` and
`$identifier.field.path` parsing then happens in a tinct stdlib macro.

`$parse-template` is an ordinary tinct stdlib function that walks the string
character by character, collecting literal text until `$` and then identifier
characters, emitting a sequence of `{type: "str" ...}` and `{type: "var" ...}`
AST dicts. `$build-str-call` assembles these into the `[str ...]` AST dict.
Both are inspectable tinct code, testable via corpus tests, and modifiable
without touching the Rust compiler.

**Phase 3 does not fit this model.** The macro cannot parse arbitrary tinct
expressions from within a string — doing so requires calling the tinct parser
from tinct code. If expression interpolation (`${expr}`) is needed, the lexer
must still detect `${...}` boundaries and pass the inner expression as a
pre-parsed AST argument. The "extract first" discipline (bind computed values
to names before the template string) keeps the macro model clean.

**Span tracking.** The macro must attach source spans to generated `Var` AST
nodes, computed as character offsets from the `IString` token span, so that
"variable not found" errors point into the template string rather than to the
`[call $tmpl ...]` call site. This requires the dual-span infrastructure from
`macros.md` Phase 3.

## Implementation

### Lexer (`src/lexer.rs`)

Recognizes `i"` as an interpolated string start. All string forms emit the
unified `Token::StringLiteral { prefix, delimiter, content }` token:

- `prefix`: `""` for plain strings, `"i"` for interpolated strings
- `delimiter`: `"\""` for single-line, `"\"\"\""` for triple-quoted
- `content`: raw string bytes — no escape processing, no `$ident` scanning

The `InterpolatedPart` enum and `lex_interpolated_string()` method have been
removed; their responsibilities are now split between the desugar pass (for
`$ident` scanning) and the lowering pass (for escape processing).

### Desugar (`src/desugar.rs`)

`build_interpolated_string_node()` scans the raw content for `$$` (literal `$`)
and `$ident` variable references, building a `[tmpl "template"]` call.
Triple-quoted strings are wrapped in `[unindent ...]`. Only `$ident` is
supported — there is no `${expr}` expression interpolation form.

### Lowering (`src/lower.rs`)

`process_escapes()` converts escape sequences in raw string content: `\\`, `\"`,
`\n`, `\t`, `\r`, and unknown-escape pass-through. Runs after desugar.

### Type Checker (`src/typecheck.rs`)

No change — the `tmpl` macro expands to standard string construction calls.

### Evaluator (`src/eval.rs`)

No change — the `tmpl` macro expands to standard string construction calls.

### Formatter (`src/formatter.rs`)

Interpolated strings desugar before the formatter sees them, so they render
as `[tmpl ...]` calls. The formatter roundtrip preserves the desugared form.

## References

- doc/02-syntax.md §Variable References — "Synergy with string interpolation
  (if added): `"Hello $name"`"
- Kotlin string templates: `"Hello $name"` and `"Hello ${expr}"` —
  closest precedent for `$`-based interpolation with expression embedding.
- Python f-strings (PEP 498, 2015): `f"Hello {name}"` — prefix-based
  opt-in pattern. Established that interpolation prefixes are learnable and
  unambiguous.
- Ruby string interpolation: `"Hello #{name}"` — expression interpolation
  with `#{}` delimiters.
- Nix string interpolation: `"Hello ${name}"` — configuration language
  precedent. Nix uses `${expr}` syntax inside all double-quoted strings (no
  prefix). tinct's `i` prefix avoids the breaking change Nix's approach
  would require.
- Launchbury, J. (1993). "A natural semantics for lazy evaluation."
  *POPL '93*, pp. 144–154. — Desugaring to `[str-parts ...]` via `tmpl` preserves
  Launchbury's sharing semantics: each interpolated segment is a thunk, forced at most once.
- doc/whatif/macros.md — If the macro system ships, Phases 1–2 of this
  feature can be implemented as `[macro tmpl ...]` in tinct stdlib with a
  minimal opaque `IString` lexer token; see §Alternative Implementation:
  Macro-Based above.
- Pombrio, J. & Krishnamurthi, S. (2014). "Resugaring: lifting evaluation
  sequences through syntactic sugar." *PLDI '14*, pp. 361–371. — Span tracking
  for macro-generated AST nodes; needed to report errors inside template strings
  at the correct source location rather than at the macro call site.
