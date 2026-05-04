# What If: String Interpolation for tinct

What would it take to add string interpolation (`i"Hello name"`) to
tinct?

## Current State

tinct uses `str` for string concatenation:

```lisp
greeting: [str "Hello " name ", you are " age " years old"]
```

This works but is verbose for multi-part strings. The `$` sigil for
variable references makes interpolation syntactically natural — `name`
inside a string could reference the variable.

doc/02-syntax.md §Variable References notes: "Synergy with string
interpolation (if added): `"Hello name"`"

Existing alternatives:

```lisp
# str concatenation (current)
msg: [str "Hello " name]

# words for space-separated (current)
[words "Hello" name "welcome"]  # → "Hello Alice welcome"
```

### What's Missing

1. Inline string construction without explicit `str` calls.
2. Readable multi-part string assembly — current approach requires
   counting arguments and interleaving literals with references.
3. Expression embedding inside strings — no way to inline a computed
   value without breaking out to a `str` call.

## What String Interpolation Would Provide

1. **Reduced verbosity.** `i"Hello name"` vs
   `[str "Hello " name]` — fewer tokens, more readable.

2. **Formatter ergonomics.** `doc/whatif/templating.md` formatters
   build strings heavily. Interpolation makes formatter code
   significantly more readable:
   ```lisp
   # Before
   [str indent key ": " [quote-yaml val] "\n"]

   # After
   i"$indent$key: ${[quote-yaml val]}\n"
   ```

3. **LLM token efficiency.** `str` calls produce high token counts.
   Interpolation reduces token count for string-heavy code, improving
   LLM generation quality.

## Design

Use a distinct `i"..."` prefix for interpolated strings, keeping
regular `"..."` strings unchanged:

```lisp
# Regular string (no interpolation, current behavior)
"Hello name"           # literal string containing "name"

# Interpolated string (new syntax)
i"Hello name"          # → "Hello Alice"
```

### Syntax

**Simple interpolation:** `$identifier` and `$expr.field` expand to
variable values, converted to strings via `str` semantics.

```lisp
i"Host: $config.host"       # dot access
i"Count: $n"                # simple variable
```

**Expression interpolation:** `${expr}` evaluates an arbitrary tinct
expression inside the string.

```lisp
i"Total: ${[+ x y]}"
i"Name: ${record.name}"
```

**Escaping:** `$$` produces a literal `$` inside an interpolated string.

```lisp
i"Price: $$$amount"     # → "Price: $42"
```

### Semantics — Desugaring to `str`

An interpolated string desugars to a `str` call at parse time. This
is a pure syntactic transformation — no new evaluation semantics:

```lisp
# Source
i"Hello name, you are $age years old"

# Desugars to
[str "Hello " name ", you are " age " years old"]
```

This desugaring preserves laziness: each interpolated segment is a
normal expression, evaluated on demand like any `str` argument.

### Internal Representation

The lexer recognizes the `i"` prefix and tokenizes the interpolated
string into a sequence of literal segments and expression references.
The parser assembles these into a `str` call node in the AST — no
new AST variant is needed.

```rust
// Lexer output for i"Hello name, age ${[+ x 1]}"
Token::IStringStart
Token::StringLiteral("Hello ")
Token::VarRef("name")
Token::StringLiteral(", age ")
Token::ExprStart          // ${
Token::BracketOpen        // [
Token::Identifier("+")
Token::Identifier("x")
Token::Int(1)
Token::BracketClose       // ]
Token::ExprEnd            // }
Token::IStringEnd
```

The parser transforms this token sequence into the equivalent of
`[str "Hello " name ", age " [+ x 1]]`.

### Interaction with Lazy Evaluation

Because interpolated strings desugar to `str` calls, they inherit
tinct's lazy evaluation semantics. Each interpolated segment becomes
a thunk that is forced only when the resulting string value is
demanded. This is consistent with Launchbury (1993) — the desugaring
introduces no new evaluation forms.

### Interaction with Type Inference

`str` already accepts arguments of any type and coerces them to
strings. Interpolated strings inherit this behavior through
desugaring. No changes to the type checker are needed — the
desugared form is a standard `str` call, which the type checker
already handles.

### Design Rationale

1. **No breaking change.** Double-quoted strings keep their current
   semantics. `"name"` remains a literal string. Interpolation is
   opt-in via the `i` prefix.

2. **Natural syntax.** tinct's `$` sigil for variables makes
   `i"Hello name"` read naturally — `name` already means "the value
   of name" everywhere else.

3. **Precedent.** Kotlin uses `"Hello name"` and `"${expr}"` with
   the same `$`-sigil convention. Python f-strings (`f"Hello {name}"`)
   establish the prefix-based opt-in pattern. The `i` prefix is
   compact and unambiguous.

4. **Desugaring keeps it simple.** No new runtime semantics, no new
   type rules, no new evaluation strategy. The feature is entirely
   syntactic sugar over an existing builtin.

## What Would Change

### Lexer (src/lexer.rs)

**Current:** The lexer treats `"..."` as a single string literal
token. No prefix recognition.
**Proposed:** Recognize `i"` as an interpolated string start. Tokenize
the interior into alternating literal segments and expression
references. Track nesting depth for `${...}` expression blocks.
**Impact:** Moderate — new token types and a sub-lexer state machine
for interpolated string interiors. The main lexer loop gains a new
entry point but existing string tokenization is unchanged.

### Parser (src/parser.rs)

**Current:** String literals parse to `Expr::String`.
**Proposed:** Interpolated string token sequences parse to
`Expr::Call` nodes equivalent to `[str seg1 seg2 ...]`. This
is a desugaring step in the parser — no new AST node type.
**Impact:** Minor — new parse rule that assembles existing AST nodes.

### Grammar (src/lexer.rs + src/parser.rs)

**Current:** String grammar handles `"..."` with escape sequences.
**Proposed:** Add `istring` rule with `i"` prefix and interpolation segment alternatives in the hand-written lexer at `src/lexer.rs`.
**Impact:** Minor — additive lexer/parser rule.

### Type Checker (src/typecheck.rs)

**Current:** `str` calls type-check normally.
**Proposed:** No change — desugared form is a standard `str` call.
**Impact:** None.

### Evaluator (src/eval.rs)

**Current:** `str` evaluation handles variable-arity string
concatenation.
**Proposed:** No change — desugared form evaluates as a normal `str`
call.
**Impact:** None.

### Formatter (src/formatter.rs)

**Current:** Formats string literals as-is.
**Proposed:** Recognize interpolated string tokens and format them
as `i"..."` with appropriate line-breaking for long interpolated
strings.
**Impact:** Minor — new formatting case for `i"..."` tokens.

## Phased Adoption

### Phase 1: Simple Variable Interpolation

`i"Hello name"` where `$identifier` expands to the variable's
string representation (via `str` semantics).

Implementation:
- Lexer recognizes `i"` as an interpolated string token
- Parser splits into literal segments and variable references
- Desugars to `[str "Hello " name]` (AST rewrite)

### Phase 2: Dot Access in Interpolation

`i"Host: $config.host:$config.port"` — dot access chains expand
inside interpolated strings, following the same compound-atomic rules
as regular access expressions (doc/02-syntax.md §Tokenization Rules).

### Phase 3: Expression Interpolation

`i"Total: ${[+ x y]}"` — arbitrary tinct expressions inside
`${ }` delimiters. Requires tracking brace nesting depth in the
lexer.

### Prerequisites

- No hard dependencies beyond current codebase.
- Lexer change to recognize `i"` prefix (ideally after the
  hand-written lexer is stable).
- If Phase 2 is included: compound-atomic access chain parsing must
  be solid (already implemented per commit 5f8856f).

### Alternative Implementation: Macro-Based

If `doc/whatif/macros.md` ships before this feature, implement
Phases 1–2 via `[defmacro tmpl ...]` rather than as a parser
feature. The Rust change shrinks to two steps: the lexer recognizes
the `i"` prefix and emits the raw string content as an opaque
`IString` token (no scanning of `$` patterns); the parser wraps it
as `[tmpl "raw content"]`. All `$identifier` and
`$identifier.field.path` parsing then happens in a tinct stdlib
macro:

```tinct
[defmacro tmpl [template-ast]
  # template-ast is {type: "str" value: "Hello name, count: $count"}
  segments: [parse-template template-ast.value]
  # → [{type: "str" value: "Hello "} {type: "var" name: "name"} ...]
  [build-str-call segments]]
```

`$parse-template` is an ordinary tinct stdlib function that walks
the string character by character, collecting literal text until `$`
and then identifier characters, emitting a sequence of `{type: "str"
...}` and `{type: "var" ...}` AST dicts. `$build-str-call`
assembles these into the `[str ...]` AST dict. Both are
inspectable tinct code, testable via corpus tests, and modifiable
without touching the Rust compiler.

**Phase 3 does not fit this model.** The macro cannot parse
arbitrary tinct expressions from within a string — doing so would
require calling the tinct parser from tinct code. If expression
interpolation (`${expr}`) is ever needed, the lexer must still
detect `${...}` boundaries and pass the inner expression as a
pre-parsed AST argument. Consider enforcing the "extract first"
discipline instead: bind computed values to names before the
template string, keeping the macro model clean.

**Span tracking.** The macro must attach source spans to generated
`Var` AST nodes, computed as character offsets from the `IString`
token span, so that "variable not found" errors point into the
template string rather than to the `[call $tmpl ...]` call site.
This requires the dual-span infrastructure from `macros.md` Phase 3.

### Trigger

- When formatter work (templating.md Phase 2) begins and `str`
  verbosity becomes a concrete pain point.
- When token count for LLM-generated tinct becomes a measurable
  concern.
- When the lexer is rewritten for other reasons (good time to add
  the `i"` prefix recognition).

## References

- doc/02-syntax.md §Variable References — "Synergy with string interpolation
  (if added): `"Hello name"`"
- Kotlin string templates: `"Hello name"` and `"Hello ${expr}"` —
  closest precedent for `$`-based interpolation with expression
  embedding.
- Python f-strings (PEP 498, 2015): `f"Hello {name}"` — prefix-based
  opt-in pattern. Established that interpolation prefixes are
  learnable and unambiguous.
- Ruby string interpolation: `"Hello #{name}"` — expression
  interpolation with `#{}` delimiters.
- Nix string interpolation: `"Hello ${name}"` — configuration
  language precedent. Nix uses `${expr}` syntax inside all double-
  quoted strings (no prefix). tinct's `i` prefix avoids the breaking
  change Nix's approach would require.
- Launchbury, J. (1993). "A natural semantics for lazy evaluation."
  *POPL '93*, pp. 144–154. — Desugaring to `str` preserves
  Launchbury's sharing semantics: each interpolated segment is a
  thunk, forced at most once.
- doc/whatif/macros.md — If the macro system ships first, Phases 1–2
  of this feature can be implemented as `[defmacro tmpl ...]` in
  tinct stdlib with a minimal opaque `IString` lexer token; see
  §Alternative Implementation: Macro-Based above.
- Pombrio, J. & Krishnamurthi, S. (2014). "Resugaring: lifting
  evaluation sequences through syntactic sugar." *PLDI '14*, pp.
  361–371. — Span tracking for macro-generated AST nodes; needed to
  report errors inside template strings at the correct source
  location rather than at the macro call site.
