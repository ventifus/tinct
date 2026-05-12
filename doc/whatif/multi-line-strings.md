# What If: Multi-Line Strings for tinct

**State:** Accepted — 2026-05-11

What would it take to write multi-line string literals with automatic
indentation stripping in tinct?

## Current State

Tinct's lexer already accepts literal newlines inside `"..."` and `i"..."` —
the `_` catch-all arm in `lex_quoted_string` pushes any character, including
`\n`, into the string value. There is no prohibition on multi-line string
content.

What is missing is **indentation stripping**. Writing a multi-line string in
indented code captures the raw source indentation:

```tinct
config: [
  query: "SELECT *
    FROM users
    WHERE active = true"
]
# value: "SELECT *\n    FROM users\n    WHERE active = true"
# The four spaces of indentation leak into the string value
```

To avoid this, users currently use explicit `\n` escape sequences or `[str ...]`
concatenation:

```tinct
query: "SELECT *\nFROM users\nWHERE active = true"

message: [str
  "Dear " name ",\n"
  "Your order of " count " items is ready."]
```

### What's Missing

1. A standard function that strips the common leading indentation from a
   multi-line string, using the last line as the baseline.
2. A convenient `"""..."""` syntax that wraps a raw string with that function.

## Why This Matters for tinct

Multi-line content is common in data processing and config generation: SQL
queries, shell scripts, nginx blocks, email bodies, Markdown prose. The
indentation stripping function is independently useful — usable on strings read
from files, built dynamically, or written inline.

## Design

### `unindent` — stdlib function

`unindent` takes a raw multi-line string and strips indentation using the last
line as the baseline:

```tinct
unindent: [fn [s]
  [ls:    [lines s]
   n:     [length [last ls]]
   inner: [slice 1 -1 ls]]
  [join "\n" [map [fn [l] [slice n [length l] l]] inner]]]
```

Algorithm: split into lines; the last line is the whitespace anchor; its length
is the strip count; drop the first (empty, after the opening newline) and last
(anchor) lines; strip `n` characters from each remaining line; rejoin.

```tinct
[unindent "
  SELECT *
  FROM users
  WHERE active = true
  "]
# → "SELECT *\nFROM users\nWHERE active = true\n"
```

`unindent` is a regular stdlib function — composable, testable, and usable
independently of the `"""` syntax:

```tinct
# Strip indentation from a file
[unindent [slurp template-file]]

# Combine with other string operations
[trim [unindent raw-block]]
```

### `"""..."""` — parse-stage macro

`"""..."""` is syntactic sugar that desugars to `[unindent "..."]` at parse
time. The lexer already handles the raw content (newlines are permitted in
`"..."`); the macro just wraps the string with the stdlib function.

```tinct
query: """
  SELECT *
  FROM users
  WHERE active = true
  """
# Desugars to: [unindent "\n  SELECT *\n  FROM users\n  WHERE active = true\n  "]
# Result:      "SELECT *\nFROM users\nWHERE active = true\n"
```

The closing `"""` on its own line — with its leading whitespace — becomes the
last line that `unindent` measures. This gives a clear visual anchor: the
string content aligns with the surrounding code, and the closing delimiter
controls how much is stripped.

`i"""..."""` desugars to `[unindent i"..."]`:

```tinct
email: i"""
  Dear $name,

  Your order #$order-id is ready.

  Regards, $sender
  """
# Desugars to: [unindent i"\n  Dear $name,\n  ..."]
```

### Suppressing the trailing newline

The last content line before the closing `"""` ends with a newline (which
`unindent` preserves). To suppress it, call `[trim-end "\n" ...]` or use
`[str-trim-right ...]`:

```tinct
label: [trim-end "\n" """
  Click here
  """]
```

Or more concisely, compose with `unindent` directly:

```tinct
label: [trim [unindent "
  Click here
  "]]
```

### Single `"` and `""` inside triple-quoted content

Since the closing delimiter is three quotes, one or two consecutive `"` inside
the content require no escaping:

```tinct
json-doc: """
  {"key": "value", "nested": {"a": 1}}
  """
```

To include `"""` literally, escape the first quote: `\"\"\"`.

## What Would Change

### `stdlib/prelude.llt` — `unindent` function

**Current:** Not present.
**Proposed:** Add `unindent` alongside other string utilities. Implementation
is pure tinct using `lines`, `last`, `length`, `slice`, `map`, `join`.
**Impact:** Minor. Small stdlib addition.

### `stdlib/macros.llt` — `"""` parse-stage macro

**Current:** Not present.
**Proposed:** Register `"""` and `i"""` as parse-stage macros that wrap their
content string with `[unindent ...]`. The lexer already tokenizes the content
correctly — the macro just adds the wrapper call.
**Impact:** Minor. Parse-stage macro registration; no AST changes.

### `doc/02-syntax.md` — String literals section

**Current:** Documents `"..."` and `i"..."`. Does not mention that literal
newlines are permitted.
**Proposed:** Add a note that `"..."` strings permit embedded newlines. Document
`"""..."""` and `i"""..."""` as the idiomatic form when indentation stripping
is desired. Document `unindent` as the underlying function.
**Impact:** Minor. Documentation only.

## Prerequisites

None. The lexer already supports literal newlines in `"..."`. `unindent` is
pure stdlib. The `"""` macro requires only the parse-stage macro infrastructure,
which is already complete.

## References

- Dhall Language Standard. "Multi-line string literals." — [closing-delimiter
  indentation baseline; the stripping model `unindent` implements]
- Python Software Foundation. "String literals." *Python Language Reference* §2.4.
  — [`textwrap.dedent()` is the stdlib equivalent of `unindent`; same algorithm]
- Kotlin Documentation. "`trimIndent()`." — [library-side stripping; same
  approach as tinct's `unindent`]
