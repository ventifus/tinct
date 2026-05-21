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
   [str indent key ": " [quote-yaml val] "\n"]

   # After
   i"$indent$key: ${[quote-yaml val]}\n"
   ```

3. **LLM token efficiency.** `str` calls produce high token counts.
   Interpolation reduces token count for string-heavy code, improving LLM
   generation quality.

Implemented as `templating-phase3`: `i"..."` + `${expr}` + formatter roundtrip
(completed 2026-05-05).

## Supersession Notes

- **Desugaring path**: The feature doc describes a single-step `desugar_interpolated_string()` that produces `[str ...]` directly. The actual implementation is a two-step flow: the parser calls `emit_tmpl_call()` producing `[tmpl raw-template expr0 ...]`; the macro expander transforms `[tmpl ...]` into `[str ...]` via `tmpl-transformer` in `stdlib/macros.llt`. See [macros.md](macros.md) (2026-05-06).
- **Formatter reconstruction**: The formatter reconstructs `i"..."` syntax from `[str ...]` call patterns — it does not emit `[str ...]` for interpolated strings.
- **`InterpolatedPart` enum**: The enum has three variants: `Literal(String)`, `VarRef(String)`, and `Expr(String)` (the `${expr}` form). The `Expr` variant is missing from the feature doc's §Internal Representation.

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
i"Host: $config.host"       # dot access
i"Count: $n"                # simple variable
```

**Expression interpolation:** `${expr}` evaluates an arbitrary tinct
expression inside the string.

```tinct
i"Total: ${[+ x y]}"
i"Name: ${record.name}"
```

**Escaping:** `$$` produces a literal `$` inside an interpolated string.

```tinct
i"Price: $$$amount"     # → "Price: $42"
```

### Semantics — Desugaring to `str`

An interpolated string desugars to a `str` call at parse time. This is a pure
syntactic transformation — no new evaluation semantics:

```tinct
# Source
i"Hello $name, you are $age years old"

# Desugars to
[str "Hello " name ", you are " age " years old"]
```

This desugaring preserves laziness: each interpolated segment is a normal
expression, evaluated on demand like any `str` argument.

### Internal Representation

The lexer recognizes the `i"` prefix and emits `Token::InterpolatedString(Vec<InterpolatedPart>)`
containing a sequence of `InterpolatedPart::Literal(String)` and
`InterpolatedPart::VarRef(String)`. The parser's `desugar_interpolated_string()`
helper converts this into a `str` call node in the AST — no new AST variant
is needed.

Variable names stop at common punctuation (`,`, `.`, `!`, `?`) in addition to
standard delimiters, enabling natural text like `i"Hello $name, welcome!"` where
the comma is not part of the variable name.

### Interaction with Lazy Evaluation

Because interpolated strings desugar to `str` calls, they inherit tinct's
lazy evaluation semantics. Each interpolated segment becomes a thunk that is
forced only when the resulting string value is demanded. This is consistent
with Launchbury (1993) — the desugaring introduces no new evaluation forms.

### Interaction with Type Inference

`str` already accepts arguments of any type and coerces them to strings.
Interpolated strings inherit this behavior through desugaring. No changes to
the type checker are needed — the desugared form is a standard `str` call,
which the type checker already handles.

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
`[defmacro tmpl ...]` rather than as a parser feature. The Rust change shrinks
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

Recognizes `i"` as an interpolated string start. Added:

- `Token::InterpolatedString(Vec<InterpolatedPart>)` token type
- `InterpolatedPart` enum: `Literal(String)` | `VarRef(String)`
- `lex_interpolated_string()` method: parses string content character by
  character, recognizes `$ident` as variable references, `$$` as escaped
  literal `$`, handles same escape sequences as regular strings (`\"`, `\\`,
  `\n`, `\t`, `\r`), stops variable names at common punctuation

### Parser (`src/parser.rs`)

Added `Token::InterpolatedString` case in `parse2()` main loop. Added
`desugar_interpolated_string()` helper:

- Converts `InterpolatedPart::Literal` → `Expr::Str`
- Converts `InterpolatedPart::VarRef` → `Expr::VarRef`
- Builds `Call` node with `func=Box<VarRef("str")>`, `args=parts`
- Returns `Spanned<Expr::Call>` that desugars `i"Hello $name"` to
  `[str "Hello " name]`

### Type Checker (`src/typecheck.rs`)

No change — desugared form is a standard `str` call.

### Evaluator (`src/eval.rs`)

No change — desugared form evaluates as a normal `str` call.

### Formatter (`src/formatter.rs`)

Interpolated strings desugar before the formatter sees them, so they render
as `[str ...]` calls. The formatter roundtrip preserves the desugared form.

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
  *POPL '93*, pp. 144–154. — Desugaring to `str` preserves Launchbury's
  sharing semantics: each interpolated segment is a thunk, forced at most once.
- doc/whatif/macros.md — If the macro system ships, Phases 1–2 of this
  feature can be implemented as `[defmacro tmpl ...]` in tinct stdlib with a
  minimal opaque `IString` lexer token; see §Alternative Implementation:
  Macro-Based above.
- Pombrio, J. & Krishnamurthi, S. (2014). "Resugaring: lifting evaluation
  sequences through syntactic sugar." *PLDI '14*, pp. 361–371. — Span tracking
  for macro-generated AST nodes; needed to report errors inside template strings
  at the correct source location rather than at the macro call site.
