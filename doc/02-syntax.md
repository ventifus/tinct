# Syntax

This document describes the tinct language syntax: its design rationale, formal grammar, tokenization rules, and quick reference. For evaluation semantics (how these constructs execute), see [Evaluation](08-evaluation.md). For the complete language documentation see [doc/index.md](index.md).

---

## 1. Overview

### Single Bracket Syntax

**`[]` is the only bracket type.** No `()` in the language. Every expression uses `[]`. A bare identifier in head position signals function application — the bracket type is not needed for this. Positional and named entries may be freely interleaved — auto-indices are assigned sequentially to positional entries regardless of where named entries appear. See Principle 2 for the auto-indexing and parsing rules.

**Why single brackets:**
- Simpler — one bracket type, one concept
- `()` and `{}` are both freed for future use
- Bare identifier in head position signals application, approaching Lisp's `(f x y)` ergonomics
- `[]` is familiar from JSON, Python, JavaScript
- True unification: there's one data structure, so there's one syntax

**Parser complexity trade-off:** Single brackets with overloaded semantics require careful disambiguation: head-position classification (call vs dict vs data sequence), keyword recognition (`call`/`fn`/`type` vs dict entries), access chain whitespace sensitivity (`a.b` vs `a .b`), and special-form parsing. This complexity is concentrated in the parser — the evaluator and user-facing syntax remain simple.

### Special Forms vs Stdlib Functions

**Lazy evaluation means most "control flow" is just regular functions.** In an eager language, `if` must be a special form because both branches would be evaluated before `if` runs. In tinct, all arguments are thunks — the unused branch is never materialized.

Only constructs that affect **binding structure** or **dict construction** need to be special forms (built into the language). The parser recognizes these by checking the first entry of every `[]`:

| Language-level (special forms) | Why |
|-------------------------------|-----|
| `call` | Triggers function application (exact arity required) |
| `fn` | Introduces parameter bindings, creates a new scope |
| `type` | Compile-time type declaration, not a runtime value |
| `quote` | Captures AST as data without evaluating (code-as-data) |
| `unquote` | Splices evaluated values into quoted templates (inside `quote` only) |
| `unquote-splice` | Splices sequence elements into quoted list positions (inside `quote` only) |
| `defmacro` | Registers compile-time AST transformation (runs before evaluation) |

Everything else can be a regular function in the stdlib:

| Stdlib function | How it works with lazy eval |
|----------------|----------------------------|
| `if` | Materializes `cond`, returns the matching branch thunk (other branch never materialized) |
| `cond` | Materializes conditions in order, returns first matching branch |
| `when` | Like one-armed `if`; materializes condition, returns body or `[]` |
| `unless` | Inverse of `when`; materializes condition, returns body or `[]` |
| `and` | Materializes first arg; if false, returns false without materializing second |
| `or` | Materializes first arg; if true, returns true without materializing second |
| `not` | Materializes its argument; returns the boolean inverse |

```tinct
# These are stdlib functions, not special forms:
[if [> x 0] positive non-positive]
[and [valid? input] [process input]]  # process never called if invalid
[or cached-value [expensive-compute]]        # compute skipped if cached
```

### Special Form Recognition

**The parser recognizes special forms by keyword.** When the first entry of a `[]` is an identifier matching `call`, `fn`, or `type` (not followed by `:`), the parser emits a specialized AST node. Any other identifier in head position (not keyword, not followed by `:`) triggers implied call — a `Call` node with the identifier as the function. A `$`-prefixed identifier in head position produces a data sequence.

```tinct
[f x]                 # CallExpr — implied call: bare identifier "f" in head
[f x y]              # CallExpr — call f(x, y)
[call f x]           # CallExpr — explicit call (identical AST to implied)
[fn [x] x]           # FnExpr
[type [Fn@b [a]]]    # TypeExpr

[call: something]    # Dict — "call" followed by ":" is a key, not a keyword
[$f x y]             # Dict (data sequence) — $-prefixed head prevents call
[f]                  # CallExpr — zero-argument call to f
[$f]                 # Dict — single-element sequence containing ref(f)
[call
: value]             # CallExpr — newline breaks colon_ahead (it only matches spaces/tabs, not newlines)
```

**Note on `colon_ahead`:** The lookahead pattern that rejects `call:` as a dict key only matches horizontal whitespace (spaces and tabs). A newline between a head identifier and a colon breaks the pattern — `[name\n: val]` is a (malformed) implied call, not a dict entry. This matches the existing rule for `[call\n: x]` and is documented formally in §6 Complete Grammar (`colon_ahead = ws_chars* ~ ":"` where `ws_chars = " " | "\t"`).

**Why parser-level:** The distinction between calls and data must be unambiguous before evaluation. The head-position rule classifies brackets at parse time, before any thunks are created. This preserves lazy evaluation semantics — the evaluator never needs to eagerly inspect the head of a bracket expression to determine its role.

---

## 2. Lexical Grammar

### 2.1 Whitespace and Comments

**`#` to end of line.** Python/shell style. No block comments.

```tinct
# This is a comment
[x: 5  y: 10]  # Inline comment
```

Whitespace and comments are implicitly skipped between tokens.

```ebnf
WHITESPACE = " " | "\t" | "\r" | "\n"
COMMENT    = "#" ~ (!NEWLINE ~ ANY)* ~ (NEWLINE | EOI)
```

The `(NEWLINE | EOI)` anchor ensures a comment consumes through the end of the line (or end of input if the comment is on the last line). `NEWLINE` matches line endings (`\n`, `\r\n`, `\r`).

**Whitespace significance:** Whitespace is significant only for `@` (annotation), not for `.` or `[`:

- `a.b` — dot access
- `a .b` — also dot access (whitespace before `.` is allowed, same behavior as Nix/Jsonnet)
- `a[0]` — two separate expressions: bare identifier `a` followed by nested expression `[0: 0]` (bracket access was removed in access-pipeline-phase2; use `[get 0 a]` instead)
- `word@Annotation` — annotation (no whitespace before `@`)
- `word @Annotation` — bare identifier `word` followed by separate expression

`@` (ImmediateAt) is the only whitespace-sensitive token: a space before `@` prevents annotation detection. Both `.` and `[` are not whitespace-sensitive. `@` detection is handled by the hand-written lexer using `last_was_identifier: bool` tracking.

### 2.2 Brackets and Punctuation

The following punctuation characters are used as inline literals throughout the grammar (not as named token rules):

| Character | Purpose |
|-----------|---------|
| `[`, `]` | Bracket expressions, param lists, access chains |
| `:` | Key-value separator |
| `;` | Entry separator |
| `@` | Annotation separator |
| `\|` | Pipe operator (desugar-only infix; `a \| f` → `[f a]`) |
| `...` | Variadic parameter prefix |
| `---` | Document separator (via `doc_separator` rule) |

### 2.3 Literals

#### Literal Recognition

**The lexer recognizes literals by pattern, not the evaluator.** This is consistent with parser-level special form recognition — the distinction between literal types is made at lexing/parsing time, before any evaluation occurs. The hand-written lexer at `src/lexer.rs` implements this recognition.

**Quoting forces string interpretation.** `"true"` is the string `"true"`, `"42"` is the string `"42"`. Quoting is the escape hatch from literal recognition.

**Why lexer-level:** If `true` and `42` were bare identifiers that the evaluator later reinterpreted, it would break the reference model in confusing ways — `hello` would resolve as a variable reference but `true` would secretly be a boolean. By having the lexer recognize these patterns first, the rule becomes precise: bare words that don't match any prior pattern (sigil, numeric, boolean, quoted string) are variable references.

Literals are recognized in precedence order. The first matching rule wins.

#### 2.3.1 Variable References and `$` Disambiguation

**Bare words are variable references.** Any identifier that isn't a numeric literal, boolean literal, or quoted string resolves as a variable reference — no sigil required. String literals must be quoted.

```tinct
[
    name: "Alice"                # key "name", value is string "Alice" (quoted)
    greeting: [str "Hello " name]  # name references the binding -> "Alice"
    $computed-key: val           # $-prefixed key: computed key (reference)
    env: "production"            # strings must be quoted in value position
]
```

**`$` is a position-dependent disambiguator**, not a universal reference sigil:

| Position | Default interpretation | `$` overrides to |
|----------|----------------------|------------------|
| Key (before `:`) | String key | Computed key (reference) |
| Head (first in `[]`) | Call (when bare identifier) | Data — NOT a call |
| Other value | Reference | Reference (redundant, harmless) |

```tinct
[f x y]              # call: f(x, y) — bare identifier in head
[$f x y]             # data: sequence [ref(f), ref(x), ref(y)] — $-head prevents call
[$key: val]          # computed key: resolves key, uses as key
[f $x y]             # call f(x, y) — $x and x are identical in non-head position
```

**`%` and `%name` are ordinary identifiers** used by convention for pipeline references. `%` refers to the previous document's output; `%name` refers to a named section's output. See §5. The lexer uses `lex_percent_word()` to handle `%name` identifiers — they are lexed identically to escaped references (stopping at `.` so that `%base.x` tokenises as `Identifier("%base")`, `Dot`, `Identifier("x")`; sets `last_was_identifier = true` for ImmediateAt detection) but emit `Identifier` tokens (not `EscapedRef`) because `%` is not a `$` sigil.

**Identifier character rules** (denylist approach — any character valid except structural delimiters):

```ebnf
identifier = @{ ident_char+ }
ident_char = _{
    !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "." | "|")
    ~ ANY
}
```

The denylist provides extensibility for new operators without reserved keywords and enables full Unicode identifier support. `.` is excluded so `a.b` is dot access, not a single identifier. `|` is excluded so `a | b` is a pipe expression, not a single identifier.

**`$` (escaped reference)** follows the same character rules:

```ebnf
escaped_ref = @{ "$" ~ ident_char+ }
```

`$` itself is a valid identifier character: `$+`, `$>=`, `$->`, `$0` are all valid escaped references. A bare `$` not followed by any identifier character is a parse error.

**Access chains** attach to identifiers the same way they attach to escaped references. Whitespace before `.` is allowed (the dot is always a `Dot` token):

```tinct
name.field           # Tokens: Identifier("name"), Dot, Identifier("field")
name .field          # Tokens: Identifier("name"), Dot, Identifier("field") — also dot access (whitespace ignored)
name.0               # Tokens: Identifier("name"), Dot, Int(0) — integer dot access (looks up Key::Int(0))
```

Note: Bracket access (`name[0]`) was removed in access-pipeline-phase2. Use `[get 0 name]` for integer key access and `name.field` for string key access.

**Unicode homograph risk:** Unicode homographs (e.g., Cyrillic `а` vs Latin `a`) create invisible name collisions; tinct accepts all Unicode identifier characters without NFC normalization.

**`fn` parameter lists:** Bare words inside `[fn [x y] ...]` parameter lists are parameter name declarations, not variable references. They are parsed by a dedicated `parse_param_list` path and do not follow value-position rules.

#### 2.3.2 Numeric Literals

```ebnf
float_lit = @{ "-"? ~ ASCII_DIGIT+ ~ "." ~ ASCII_DIGIT+ }
int_lit   = @{ "-"? ~ ASCII_DIGIT+ }
```

`float_lit` must be tried before `int_lit` (longer match). Negative integers and floats are supported as literals.

Examples: `42`, `-1`, `3.14`, `-0.5`

#### 2.3.3 Boolean Literals

```ebnf
bool_lit = @{ ("true" | "false") ~ !ident_char }
```

The `!ident_char` lookahead ensures `truename` is a bare word, not `true` followed by `name`.

```ebnf
ident_char = _{
    !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "$")
    ~ ANY
}
```

`ident_char` uses a denylist matching `bare_word_char` so that e.g. `call!` is a bare word, not keyword `call` + `!`.

#### 2.3.4 Quoted Strings

```ebnf
quoted_string = ${ "\"" ~ inner_string ~ "\"" }
inner_string  = @{ (escape_seq | !("\"" | "\\") ~ ANY)* }
escape_seq    = @{ "\\" ~ ("\"" | "\\" | "n" | "t" | "r") }
```

Currently supports these 5 sequences. Unicode escapes (`\uXXXX`) are not yet supported — use `$from-json` for full Unicode string parsing.

The parser handles quoted strings as atomic units — no implicit whitespace skipping between the quotes and content.

Quoting forces string interpretation: `"true"` is the string `"true"`, `"42"` is the string `"42"`.

#### 2.3.5 Interpolated Strings

Interpolated strings allow embedding variable references directly in string literals using the `i"..."` prefix syntax. They desugar to `str` calls at parse time.

```tinct
i"Hello $name"                    # Desugars to: [str "Hello " name]
i"Count: $count items"            # Desugars to: [str "Count: " count " items"]
i"Price: $$$amount"               # $$ escapes to literal $ → "Price: $42"
```

**Syntax:**
- Prefix: `i"..."` signals an interpolated string
- Variable references: `$identifier` embeds the value of a variable
- Escape: `$$` produces a literal `$` character
- Regular escape sequences (`\"`, `\\`, `\n`, `\t`, `\r`) work as in regular strings

**Variable name boundaries:** In interpolated strings, variable names stop at common punctuation (`,`, `.`, `!`, `?`) in addition to the usual delimiters. This allows natural text like `i"Hello $name, welcome!"` where the comma is not part of the variable name.

**Desugaring:** Interpolated strings are pure syntactic sugar. The parser converts them to `[str ...]` calls with the literal and variable segments as arguments. This preserves lazy evaluation — each interpolated segment is a thunk.

```tinct
# Source
i"Hello $name, you are $age years old"

# Desugars to
[str "Hello " name ", you are " age " years old"]
```

**Type coercion:** The `str` builtin coerces all argument types to strings, so you can interpolate any value: `i"Count: $num"` works whether `num` is an integer, float, or string.

#### 2.3.6 Identifiers (Variable References)

Identifiers are the fallback — any token that doesn't match a prior rule. They are variable references, not string literals. Strings require quotes.

```ebnf
identifier = @{ ident_start ~ ident_cont* }

ident_start = _{
    !("$" | "#" | "[" | "]" | ":" | ";" | "\"" | "@" | "|"
      | " " | "\t" | "\r" | "\n"
      | "...")
    ~ ident_char
}

ident_cont = _{
    !(" " | "\t" | "\r" | "\n"
      | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "." | "|")
    ~ ident_char
}

ident_char = _{
    !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "." | "|")
    ~ ANY
}
```

`ident_char` uses a denylist — any character except structural delimiters and `.`. Both `@` and `.` are excluded, so they always end an identifier. `|` is excluded so `a|b` tokenizes as `a`, `|`, `b` (pipe expression), not as the identifier `a|b`. The `"..."` exclusion is only needed at token start (variadic sigil context).

**Valid characters (denylist approach):** Any character is valid *except* structural delimiters: whitespace, `[`, `]`, `:`, `;`, `#`, `"`, `@`, `.`, and `|`. This means `[a-zA-Z0-9_/-]`, Unicode, `%`, `$`, and most other characters are all valid in identifiers. Identifiers are the default — anything that isn't a recognized special token is an identifier (a variable reference).

**Cannot start with:** `$`, `@`, `#`, `[`, `]`, `:`, `;`, `"`, or `...` (variadic sigil). These characters have special meaning at the start of a token.

**Identifier terminators** — an identifier ends at:

| Character | Purpose |
|-----------|---------|
| Whitespace | Ends the identifier |
| `[` | Ends the identifier (starts a new bracket expression) |
| `]` | Ends the identifier (closes enclosing expression) |
| `:` | Ends the identifier (key-value separator) |
| `;` | Ends the identifier (entry separator) |
| `#` | Ends the identifier (starts comment) |
| `@` | Ends the identifier (starts annotation) |
| `$` | Ends the identifier (starts escaped reference) |
| `.` | Ends the identifier (starts dot access) |

```tinct
hello                    # Identifier: ref "hello"
my-key                   # Identifier: ref "my-key"
%config                  # Identifier: ref "%config" (pipeline convention)
日本語                    # Identifier: ref "日本語" (Unicode)
"has spaces"             # NOT an identifier — quoted string
"hello"                  # Quoted string (the only way to get a string value)

# Strings containing dots, slashes, etc. must be quoted under new syntax:
"some.file.txt"          # Quoted string — dot would trigger access chain on identifier
"path/to/file"           # Quoted string
```

**Note on dot in identifiers:** The tinct lexer excludes `.` from identifier characters (`is_var_ident_char`), so `file.txt` tokenizes as `Identifier("file"), Dot, Identifier("txt")` — three separate tokens, not a single identifier. Whitespace before `.` is allowed and does not change this: `file .txt` tokenizes identically. Values containing dots that are not intended as access chains must be quoted: `"file.txt"`.

**Tree-sitter divergence:** The tinct lexer excludes `.` from identifier characters entirely, and whitespace before `.` is permitted (dot access is whitespace-insensitive). The tree-sitter grammar (`tree-sitter-llt/grammar.js`) follows the same rule: `.` is never part of an identifier. This is consistent between the two implementations.

### 2.4 Token Precedence

When classifying a bare token, the tokenizer applies rules in this order:

1. `$` sigil → `escaped_ref` (disambiguator — prevents call in head position, computes key in key position)
2. Numeric → `float_lit` or `int_lit`
3. `true`/`false` → `bool_lit`
4. `"` → `quoted_string` (the only way to produce a string literal in value position)
5. If followed by `@` immediately (no whitespace), treat as annotated value (`Fn@Number` → `annotated_bare`). This rule applies at the `atom` level only (value position). At the bracket-expression level, `[fn@Type ...]` is handled by `fn_form`'s explicit `fn_annotation?` component, making `fn` there a keyword, not an annotated identifier.
6. Everything else → `identifier` (variable reference)

This order is enforced by the hand-written parser's token dispatch logic.

### 2.5 Tokenization Rules for `.`, `[`, `..`, and `@`

These characters have context-dependent meaning and require careful disambiguation.

#### `.` Dot Access

`.` always emits a `Dot` token regardless of preceding whitespace. Dot access is **whitespace-insensitive**: `$a.b` and `$a .b` tokenize identically. The parser reads the next identifier as the field name.

Whitespace (including newlines) after `.` is also permitted — the field name may appear on the next line for readability.

```tinct
$person.name             # Dot access: get key "name" from $person
$config.database.host    # Chained dot access: $config -> "database" -> "host"
[get 0 data].name        # get integer key, then dot access field

$config
  .database              # Line-continuation: same as $config.database (whitespace before . is OK)
  .host                  # Chained: $config.database.host

some.file.txt            # Dot access chain: identifier "some", then "file", then "txt"
$x . y                   # $x is an EscapedRef; . y is also dot access (whitespace before . is allowed)
```

| Input | Tokens | Interpretation |
|-------|--------|----------------|
| `$a.b` | `EscapedRef("a")`, `Dot`, `Identifier("b")` | Dot access |
| `a.b` | `Identifier("a")`, `Dot`, `Identifier("b")` | Dot access chain |
| `$a .b` | `EscapedRef("a")`, `Dot`, `Identifier("b")` | Dot access (whitespace before `.` ignored) |
| `$a.0.b` | `EscapedRef("a")`, `Dot`, `Int(0)`, `Dot`, `Identifier("b")` | Integer dot access then field access |

Note: Bracket access and range access were removed in access-pipeline-phase2. Use `[get key data]` for dynamic key access and `[slice data start end]` / `[take n data]` / `[drop n data]` for subsequences.

```tinct
# Old syntax (removed):  $data[5]   $data[$key]   $data[2..5]
# New syntax:
[get 5 data]             # Integer key access via builtin
[get $key data]          # Dynamic key access via builtin
data.name                # String key access via dot notation
data.0                   # Integer dot access (looks up Key::Int(0))
[slice data 2 5]         # Subsequence by position
```

#### `..` Tokenization

`..` always emits two consecutive `Dot` tokens regardless of context — there is no `Token::Range`. A bare `1..5` lexes as `Int(1)`, `Dot`, `Dot`, `Int(5)` and produces a parse error at top level (dot-access on `Int(1)` requires a field name).

```tinct
config..bak              # Identifier("config"), Dot, Dot, Identifier("bak") — NOT a single string
"config..bak"            # Quoted string: "config..bak" — use quotes for literal dots
```

#### `@` Annotation

**`@` is always a structural separator.** It is not a valid bare word character. Wherever `@` appears immediately after a bare word (no whitespace), it separates the word from an annotation value.

This applies uniformly:

1. **Parameter annotation** — `x@Number` in a param list
2. **Return type annotation** — `fn@Number` on function definitions
3. **Value annotation** — `Fn@Number` in any value position (e.g., type constructors)
4. **Type assertion** — `[@Type $expr]` at the start of a bracket expression

```tinct
# Parameter context
x@Number                 # param "x" with annotation Number
timeout@[type: Number]   # param "timeout" with property dict annotation

# Function return type
fn@String                # function returning String

# Value context (generalized annotation)
Fn@Number                # annotated value: "Fn" with annotation Number
Fn@[Fn@c [b]]           # nested: function returning a function type

# Type assertion
[@Number $expr]          # assert $expr is Number

# Strings containing @ must be quoted
"email@example.com"      # quoted string
```

| Input | Interpretation |
|-------|----------------|
| `x@Number` | Annotation: "x" with type Number |
| `fn@String` | fn with return annotation String |
| `Fn@b` | Annotated value: "Fn" annotated with "b" |
| `[@String $x]` | Type assertion expression |
| `"a@b"` | Quoted string "a@b" |

---

## 3. Syntactic Grammar

### 3.1 File, Document, and Expression

A tinct file contains one or more documents separated by `---`. Each document contains one or more expressions. This is the top-level grammar:

```ebnf
file          = { SOI ~ document ~ (doc_separator ~ document)* ~ EOI }
document      = { expression* }
expression    = { !doc_separator ~ value }
doc_separator = @{ "---" ~ !bare_word_char }
```

**File:** The outermost unit. Contains documents separated by `---`.

**Document:** A sequence of expressions that form a scope chain. Each expression's result becomes the parent scope for the next expression. Documents are isolated from each other — the only connection is `%`, which carries the previous document's output as a lazy value. For the first document, `%` is `[]`. For evaluation semantics of `%` binding, `$include` cycle detection, and document pipeline caching, see [Documents](09-documents.md).

**Expression:** A single value (bracket expression, atom, access expression, etc.). The `!doc_separator` negative lookahead prevents `---` from being consumed as a bare word.

**`doc_separator`:** Three hyphens `---` not followed by a `bare_word_char`. This prevents `----` or `---foo` from matching as a separator. The parser treats this as atomic so that whitespace is not skipped between the hyphens and the lookahead.

An empty file (or one containing only whitespace/comments) is valid and produces a file with one document containing zero expressions. An empty document produces an empty Dict `[]`.

### 3.2 Bracket Expressions

A bracket expression is the fundamental syntactic unit. The parser examines the first entry to determine interpretation via a priority table:

| Priority | Condition | Interpretation | Example |
|----------|-----------|----------------|---------|
| 1 | Empty `[]` | Empty dict | `[]` |
| 2 | `@` first | Type assertion | `[@Type expr]` |
| 2b | Identifier + `@` immediately (annotated identifier), not followed by `:` | Dict | `[Fn@Number [Int]]` |
| 3 | Keyword in head (`call`, `fn`, `type`), not followed by `:` (horizontal) | Special form | `[fn [x] x]` |
| 4 | First entry is keyed (head followed by `:` with no intervening newline) | Dict | `[name: "Alice"]` |
| 5 | Identifier in head (not keyword) | Implied call | `[f x y]` → `f(x, y)` |
| 6 | `$`-prefixed head (`escaped_ref`) | Data sequence | `[$f x y]` |
| 7 | Literal in head | Data sequence | `[1 2 3]` |

**`[f]` is a zero-argument call** to `f` (Priority 5). To construct a single-element data sequence containing a reference, use `[$f]` (Priority 6).

**Newline before colon breaks Priority 4.** `[name\n: val]` is not a keyed entry — the colon lookahead only checks horizontal whitespace (spaces and tabs), consistent with keyword disambiguation. This produces a zero-argument implied call to `name`, which is a parse error (no body).

```ebnf
bracket_expr = {
    "[" ~ "]"                           // empty: []
    | "[" ~ type_assert_body ~ "]"      // type assertion: [@Type expr]
    | "[" ~ special_form ~ "]"          // call, fn, type
    | "[" ~ call_implied ~ "]"          // implied call: bare identifier in head
    | "[" ~ dict_entries ~ "]"          // data: entries (escaped_ref head, literal head, or keyed)
}
```

### 3.3 Special Forms

Special forms are recognized when the first token in a `[]` is a bare keyword (not followed by `:`). The parser tries each form before falling back to `dict_entries`.

**Horizontal-only lookahead:** The `colon_ahead` rule uses horizontal lookahead via `peek_next_horizontal`, which operates on the token stream and skips `Token::Semicolon` and `Token::Comment(_)` tokens, but stops immediately at `Token::Newline`. This means `[call\n: x]` is a malformed implied call (the newline token halts the scan before the colon is found), not a dict entry. Note: `ws_chars = " " | "\t"` is the grammar-level character-class definition in this document's EBNF appendix — it describes the same horizontal-only intent at the PEG level, but `peek_next_horizontal` implements this intent at the token-stream level, where raw whitespace characters no longer exist.

```ebnf
special_form = {
    call_form
    | fn_form
    | type_form
}
```

#### 3.3.1 `call` — Function Application

Function application has two forms — implied and explicit — that produce identical AST nodes:

**Implied call** (preferred): bare identifier in head position:
```tinct
[f x y]              # call f(x, y)
[map double data]    # call map(double, data)
[f x name: "val"]   # call f with named argument
```

**Explicit `call`** (for computed functions or documentation clarity):
```tinct
[call f x y]                        # same AST as [f x y]
[call [get-handler request] data]   # function from another call
[call % data]                       # pipeline value used as function
```

The `call` keyword is required when the function expression is not a bare identifier — e.g., the result of another call or a dot-access.

```ebnf
call_form = { keyword_call ~ value ~ call_args }

call_implied = { identifier ~ call_args }  // identifier not a keyword, not followed by ":"

call_args = { (named_arg | value)* }

named_arg = { named_arg_key ~ ":" ~ value }

named_arg_key = @{ escaped_ref | identifier }
```

Arity enforcement uses per-parameter coverage, not a simple count — each required parameter (no `default:` annotation) must be covered by either a positional argument at its index or a named argument. Parameters with `default:` annotations are optional. This is enforced at evaluation time, not parse time. See doc/04-functions.md for the formal C-COVERAGE, C-PRIORITY, C-NO-OVERLAP, and C-NAMED-VALID constraints.

#### 3.3.2 `fn` — Function Definition

```ebnf
fn_form = { keyword_fn ~ fn_annotation? ~ param_list ~ value }

fn_annotation = ${ "@" ~ annotation_value }

param_list = { "[" ~ (variadic_param | param)* ~ "]" }

param = ${ param_name ~ param_annotation? }

param_name = @{ (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_" | "-")* ~ "?"? }

param_annotation = ${ "@" ~ annotation_value }

variadic_param = @{ "..." ~ param_name }
```

Examples:
```tinct
[fn [x] x]
[fn@Number [x@Number y@Number] [+ x y]]
[fn@[type: Number  doc: "Sum"] [x@Number  y@[type: Number  default: 0]] [+ x y]]
[fn [f ...args] [map f args]]
```

#### 3.3.3 `type` — Type Alias

```ebnf
type_form = { keyword_type ~ value }
```

Examples:
```tinct
[type [Fn@b [a]]]
[type [name: String  age: Number]]
```

### 3.4 Access Chains

Access chains attach to identifiers and escaped references via dot notation. Dot access is whitespace-insensitive — the hand-written lexer always emits a `Dot` token, regardless of preceding whitespace. Both bare identifiers (`name.field`) and escaped references (`$name.field`) trigger access chain detection.

**Note:** Bracket access (`$a[0]`, `$a[$key]`) and range access (`$a[2..5]`) were removed in access-pipeline-phase2. Use `[get key data]` for dynamic key access and `[slice data start end]` / `[take n data]` / `[drop n data]` for subsequences.

```ebnf
access_expr = ${ (identifier | escaped_ref) ~ access_chain+ }

access_chain = ${ dot_access }

dot_access = ${ "." ~ access_field }

access_field = @{ (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_" | "-" | "?")* }
```

**Field name characters:** The `access_field` rule allows `?` anywhere in continuation characters (not just at the end). This supports the predicate naming convention (`int?`, `dict?`, `list?`) where the `?` suffix indicates a boolean-returning function. Field names can contain multiple `?` characters if needed (e.g., `foo?bar?` is valid).

Dot access is whitespace-insensitive — `$a.b` and `$a .b` parse identically. The `.` always emits a `Dot` token regardless of preceding whitespace; see §2.5 for the formal tokenization rule.

**Chaining examples:**

| Input | Parse |
|-------|-------|
| `$a.b` | `access_expr(escaped_ref("a"), dot_access("b"))` |
| `$a.b.c` | `access_expr(escaped_ref("a"), dot("b"), dot("c"))` |
| `$a.0` | `access_expr(escaped_ref("a"), dot_access(Int(0)))` — integer dot access |

### 3.5 Dict Entries

```ebnf
dict_entries = { (entry ~ ";"?)* }

entry = { keyed_entry | rest_entry | auto_entry }

keyed_entry = { key ~ ":" ~ value }

rest_entry = @{ "..." ~ annotation_word? }

auto_entry = { value }

key = { bracket_expr | escaped_ref | quoted_string | bare_token }

bare_token = { float_lit | int_lit | bool_lit | identifier }
```

The parser treats `rest_entry` as atomic to prevent whitespace between `...` and the optional name. `...` alone produces `Expr::Rest(None)` (anonymous open record marker). `...name` produces `Expr::Rest(Some("name"))` (named row variable). Rest entries are used in type expressions to indicate open records and row polymorphism.

Quoted strings are valid as keys, allowing keys that contain spaces, colons, or other special characters: `["my key": value]`.

**Note on key types:** The `bare_token` rule in `key` allows `float_lit` and `bool_lit` to parse successfully as keys. However, the evaluator only accepts `String` and `Int` as runtime key types and will reject `Float` and `Bool` keys with a type error. The parser is intentionally permissive here for forward compatibility — if future language versions support additional key types, no grammar changes will be needed.

**Value boundary rule:** every entry's value is exactly one token or one bracket expression. After parsing a key's value, the next whitespace-separated token starts a new entry.

**Mixed ordering:** positional (auto-indexed) and keyed (named) entries may appear in any order within a single `[]`. Auto-indices are assigned sequentially to positional entries regardless of where keyed entries appear.

**Semicolons:** `;` acts as an optional entry separator for multiple entries on one line. It is not required (whitespace alone suffices), but it enables one-line dict literals:

```tinct
[a: 1; b: 2; c: 3]
```

### 3.6 Values

A value is a single expression — one atom, one access expression, or one bracket expression:

```ebnf
value = { access_expr | bracket_expr | atom }

atom = { float_lit | int_lit | bool_lit | quoted_string | escaped_ref | annotated_bare | identifier }
```

The ordering in `atom` enforces literal precedence (section 2.4). `float_lit` before `int_lit` ensures `3.14` matches as float, not int `3` followed by `.14`. `bool_lit` before `identifier` ensures `true` matches as boolean. `annotated_bare` before `identifier` ensures `Fn@Number` is parsed as an annotated value, not as a bare identifier containing `@`.

`escaped_ref` appears in both `value` (as a plain reference) and `access_expr` (as the start of an access chain). The parser tries `access_expr` first — if the escaped_ref is followed immediately by `.` or `[`, it becomes an access expression. Otherwise it falls through to `atom` where it matches as a plain escaped_ref.

### 3.7 Annotations

**`@` is always a structural separator.** It is not a valid bare word character. Wherever `@` appears immediately after a bare word (no whitespace), it separates the word from an annotation value. Strings containing `@` must be quoted: `"email@example.com"`.

**In parameter position** (inside a `param_list`):
```ebnf
param_annotation = ${ "@" ~ annotation_value }
```
`x@Number` splits into param `x` with annotation `Number`.

**On `fn` keyword** (return type):
```ebnf
fn_annotation = ${ "@" ~ annotation_value }
```
`fn@Number` means the function returns `Number`.

**In value position** (generalized annotation):
```ebnf
annotated_bare = ${ identifier ~ "@" ~ annotation_value }
```
`Fn@Number` produces an `Annotated` node with name `"Fn"` and annotation `Number`. This is used for function type constructors (`Fn@Return [Params]`) and is available for future use on any bare word.

**As type assertion** (first token inside `[]`):
```ebnf
type_assert_body = { "@" ~ annotation_value ~ value }
```
`[@Number $expr]` asserts `$expr` has type `Number`. When a `default:` is provided (e.g., `[@[type: Number  default: 0] $expr]`), the default value is evaluated in the same environment as the asserted expression.

### 3.8 Type Expressions

Type expressions appear in type annotations and `[type ...]` declarations. They use the same `[]` syntax as data but are distinguished by context (after `@`, inside `type` form).

**Function types** use `Fn@Return [ParamTypes]`, mirroring function definitions (`fn@Return [params] body`):
```tinct
[Fn@b [a]]              # function from a to b
[Fn@Bool [a]]           # predicate
[Fn@c [a b]]            # two-arg function
```

The parser handles this via the `annotated_bare` rule -- `Fn@b` parses as `Annotated { name: "Fn", annotation: Simple("b") }`. The type checker interprets `Fn` as a function type constructor. All types in a type definition must be explicit -- there is no body to infer from.

**Note:** `Fn@Number` in a bare context (not inside `[]`) is also valid and parsed via the `annotated_bare` grammar rule, producing the same AST structure.

**Row polymorphism** is supported via `rest_entry` syntax in type expressions. `...` marks an open record type (any additional fields are permitted), and `...name` introduces a named row variable for polymorphic record operations:

```tinct
[name: String ...]            # open record: has name, allows other fields
[name: String ...r]           # named row variable r captures the remaining fields
```

**Type conventions** (not enforced by parser, enforced by type checker):
- Uppercase first letter = concrete type (`String`, `Number`, `Person`, `Fn`)
- Lowercase first letter = type variable (`a`, `b`, `k`, `v`)
- `Any` = dynamic escape hatch

**Type inference context.** The type system uses type schemes (`forall a1...an. t`) for polymorphic bindings via levels-based let-generalization (Kiselyov 2013). Type variables carry an integer level for scope tracking (`TypeVar(String, u32)`). These are type checker internals — the parser produces bare type names as strings. See doc/06-type-inference.md for details.

### Quasiquoting

`quote`, `unquote`, and `unquote-splice` are special forms for treating code as data.

```tinct
[quote [+ 1 2]]          # → dict representing the Call AST node
[quote [+ [unquote x] 1]] # → Call node with x's value spliced in
```

`[quote expr]` converts `expr` into its AST dict representation (per `doc/15-ast.md` §AST Dict Schema) without evaluating it. The result is an ordinary `Value::Dict` with `type: "call"`, `fn:`, `args:` fields etc.

`[unquote expr]` is valid only inside `[quote ...]`. It evaluates `expr` in the current environment and splices the result into the quoted dict. The parser tracks nesting depth: nested `[quote ...]` increments the depth; `unquote` only evaluates at depth 1 (Bawden 1999).

`[unquote-splice expr]` is valid only inside `[quote ...]` in a *list position* (call args or dict entries). It evaluates `expr` to a sequence and splices each element into the enclosing list. It is a parse error at the top level of a `[quote ...]` where there is no enclosing list to splice into.

### Macro Definition

`[defmacro name [params] body]` registers a compile-time AST transformation. `name` is a bare-word identifier. Macro invocations are syntactically identical to function calls — the expander distinguishes them by name lookup against registered macros.

```tinct
[defmacro my-when [pred body]
  [quote [if [unquote pred] [unquote body] []]]]

[my-when [> x 0] [process x]]  # expands to: [if [> x 0] [process x] []]
```

Macro names cannot shadow registered Rust builtins — `[defmacro str ...]` is rejected at registration time. See `doc/08-evaluation.md` §Macro Expansion Pipeline for expansion semantics.

---

## 4. Value Boundary Rules

**Every entry's value is exactly one token or one `[]` expression.** There are no multi-value entries. Whitespace separates entries — after parsing a key's value (one token or one `[]`), the next whitespace-separated token is the start of a new entry.

```tinct
[name: "Alice" age: 30]         # Two key-value pairs: name->"Alice", age->30
[key: [$a $b $c]]               # One key-value pair: key->[ref(a), ref(b), ref(c)] (nested [] is a single value)
[f x y]                         # Function call — f is the function, x and y are arguments
[x: 1 $y]                       # OK — x->1 is named; ref(y) is auto-indexed as 0
```

**Nested `[]` counts as a single value.** When a key's value starts with `[`, the parser consumes the entire balanced bracket expression as that key's value:

```tinct
[config: [timeout: 30 retries: 3]]   # config -> the entire inner dict
[steps: [a b c]]                      # steps -> the list [a b c]
```

The parser treats `[key: value1 value2 value3]` such that `key` has value `value1`, while `value2` and `value3` become separate auto-indexed entries. Multi-value semantics are achieved by wrapping in `[]`:

```tinct
# Old (removed): key has multi-value [value1 value2 value3]
[key: value1 value2 value3]

# New: key has value "value1"; "value2" and "value3" are auto-indexed entries
# Equivalent to: [key: value1  0: value2  1: value3]

# To associate multiple values with a key, wrap them in []:
[key: [value1 value2 value3]]
```

**Why:** One-token-per-value eliminates ambiguity about where one entry ends and the next begins. The parser never has to guess whether a bare word belongs to the previous entry's value or starts a new entry. Every token's role is unambiguous from left to right.

---

### Testing Requirements

Each static constraint enforced by the parser must have corpus tests demonstrating correct behavior and rejection of invalid inputs. The six documented constraints map to these test patterns:

| Static Constraint | Test Files |
|------------------|------------|
| **Mixed Positional/Named Ordering** (allowed) | `tests/corpus/valid/simple/mixed_positional_named.llt-eval` |
| **Special Form Arity** | `tests/corpus/invalid/syntax_errors/special_form_arity.llt-eval` (`[call]` with no function argument) |
| **Duplicate Key Detection** | `tests/corpus/invalid/syntax_errors/duplicate_key.llt-eval`, `tests/corpus/invalid/syntax_errors/duplicate_varref_key.llt-eval` |
| **Fn Parameter List Structure** | `tests/corpus/invalid/syntax_errors/multiple_variadics.llt-eval`, `tests/corpus/invalid/syntax_errors/param_after_variadic.llt-eval` |
| **Bracket Nesting Depth Limit** | `tests/corpus/invalid/syntax_errors/parser_depth_exceeded.llt-eval` |
| **Annotation Bracket Restriction** | `tests/corpus/invalid/syntax_errors/special_form_in_annotation.llt-eval` |

See doc/15-ast.md §Static Constraints for detailed constraint specifications.

## 5. Document Separator Grammar

The `---` separator is recognized at the file level only. It must appear on its own (not as part of a bare word like `----` or `---foo`):

```ebnf
file          = { SOI ~ document ~ (doc_separator ~ document)* ~ EOI }
document      = { expression* }
expression    = { !doc_separator ~ value }
doc_separator = @{ "---" ~ !bare_word_char }
```

---

## 6. Complete Grammar

**Canonical Source:** The authoritative parser implementation is in `src/parser.rs` (hand-written iterative descent parser) and `src/lexer.rs` (hand-written tokenizer). The EBNF notation in this chapter is illustrative and documents the language grammar — it is not executable.

The grammar rules below consolidate all syntax rules from the sections above. The actual parser implementation in `src/parser.rs` + `src/lexer.rs` follows these rules but uses Rust code rather than a parser generator.

**Historical note:** Tinct originally used a pest PEG grammar, which was removed in sprint parser-core-c3 (commit cc8333c) and replaced with the current hand-written iterative parser.

```ebnf
// === Whitespace and Comments ===

WHITESPACE = " " | "\t" | "\r" | "\n"
COMMENT    = "#" ~ (!NEWLINE ~ ANY)* ~ (NEWLINE | EOI)

// === File and Document Structure ===

file          = SOI ~ document ~ (doc_separator ~ document)* ~ EOI
document      = expression*
expression    = !doc_separator ~ value
doc_separator = "---" ~ !ident_char_body

// === Bracket Expressions ===

bracket_expr = "[" ~ "]"
             | "[" ~ type_assert_body ~ "]"
             | "[" ~ special_form ~ "]"
             | "[" ~ call_implied ~ "]"
             | "[" ~ dict_entries ~ "]"

// === Special Forms ===

special_form = call_form | fn_form | type_form

call_form    = keyword_call ~ value ~ call_args
call_implied = identifier ~ call_args     // identifier not a keyword, not followed by ":"
fn_form      = keyword_fn ~ fn_annotation? ~ param_list ~ value
type_form    = keyword_type ~ value

keyword_call = "call" ~ !ident_char ~ !colon_ahead
keyword_fn   = "fn" ~ !ident_char ~ !colon_ahead
keyword_type = "type" ~ !ident_char ~ !colon_ahead

// Lookahead: optional horizontal whitespace then colon.
// ws_chars matches only spaces and tabs (not newlines), so "call\n:" is a CallExpr, not a Dict entry.
colon_ahead = ws_chars* ~ ":"
ws_chars    = " " | "\t"

ident_char = !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "." | "|") ~ ANY

call_args = (named_arg | value)*

named_arg     = named_arg_key ~ ":" ~ value
named_arg_key = "$" ~ esc_ident | identifier

// === Type Assertions ===

type_assert_body = "@" ~ annotation_value ~ value

// === Functions ===

fn_annotation = "@" ~ annotation_value

param_list = "[" ~ (variadic_param | param)* ~ "]"

param = param_name ~ param_annotation?

param_name = (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_" | "-")* ~ "?"?

param_annotation = "@" ~ annotation_value

variadic_param = "..." ~ param_name

// Annotation value allows whitespace inside property dicts like [type: Number default: 30]
annotation_value = bracket_expr | annotation_word

annotation_word = (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_" | "-")* ~ "?"?

// === Dict Entries ===

dict_entries = (entry ~ ";"?)*

entry = keyed_entry | rest_entry | auto_entry

rest_entry = "..." ~ annotation_word?

keyed_entry = key ~ ":" ~ value
auto_entry  = value

key = bracket_expr | escaped_ref | quoted_string | bare_token

bare_token = float_lit | int_lit | bool_lit | identifier

// === Values ===

value = access_expr | bracket_expr | atom

atom = float_lit | int_lit | bool_lit | quoted_string | escaped_ref | annotated_bare | identifier

// === Generalized Annotations ===

annotated_bare = identifier ~ "@" ~ annotation_value

// === Access Chains ===

access_expr = (identifier | escaped_ref) ~ access_chain+

access_chain = dot_access

dot_access = "." ~ access_field

access_field = (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_" | "-" | "?")*

// === Literals ===

escaped_ref = "$" ~ esc_ident

esc_ident = esc_ident_char+

esc_ident_char = !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "." | "|") ~ ANY

float_lit = "-"? ~ ASCII_DIGIT+ ~ "." ~ ASCII_DIGIT+

int_lit = "-"? ~ ASCII_DIGIT+

bool_lit = ("true" | "false") ~ !ident_char

quoted_string = "\"" ~ inner_string ~ "\""
inner_string  = (escape_seq | !("\"" | "\\") ~ ANY)*
escape_seq    = "\\" ~ ("\"" | "\\" | "n" | "t" | "r")

identifier = ident_start ~ ident_cont*

ident_start = !( "$" | "#" | "[" | "]" | ":" | ";" | "\"" | "@" | "|"
               | " " | "\t" | "\r" | "\n"
               | "..." )
              ~ ident_char_body

ident_cont = !( " " | "\t" | "\r" | "\n"
              | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "|" )
             ~ ident_char_body

ident_char_body = !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "$" | "|") ~ ANY
```

---

## 7. Token Disambiguation

| Input | Interpretation | Rule |
|-------|---------------|------|
| `name` | Variable reference | Identifier (fallback) |
| `$name` | Escaped reference (disambiguator) | `$` sigil |
| `42` | Int literal | Numeric before identifier |
| `3.14` | Float literal | Float before int |
| `true` | Bool literal | Bool before identifier |
| `"hello"` | Quoted string literal | Quote delimited |
| `"true"` | Quoted string (value `true`) | Quoting overrides |
| `hello` | Reference to `hello` | Identifier |
| `[f x]` | Call `f(x)` | Identifier in head, not keyword |
| `[$f x]` | Data sequence `[ref(f), ref(x)]` | Escaped ref in head |
| `[f]` | Zero-arg call `f()` | Identifier in head |
| `[$f]` | Single-element sequence `[ref(f)]` | Escaped ref in head |
| `a.b` | Access chain | No whitespace before `.`, identifier enables access |
| `a .b` | Dot access (same as `a.b`) | Dot is whitespace-insensitive |
| `a[0]` | Bracket access | No whitespace before `[`, identifier enables access |
| `a [0]` | Implied call `a()`, then data `[0]` | Whitespace before `[` → two separate expressions |
| `$a.b` | Access chain | No whitespace before `.` |
| `$a [0]` | Escaped ref then nested expr | Whitespace before `[` |
| `x@Number` | Param with annotation | `@` in param context |
| `fn@String` | fn with return annotation | `@` after `fn` keyword |
| `Fn@Number` | Annotated value | `@` in value context |
| `[@T e]` | Type assertion | `@` first in `[]` |
| `call` (first in `[]`) | Keyword | Special form recognition (Priority 3) |
| `call@Type` (first in `[]`) | Annotated (NOT keyword) | `@` after identifier converts keyword candidate |
| `call:` | Dict key | Colon makes it a key |
| `%config` | Reference to pipeline section `config` | Identifier with `%` prefix (convention) |
| `a..b` | `Identifier("a")`, `Dot`, `Dot`, `Identifier("b")` | `.` excluded from identifier chars; both dots are access operators |
| `a[2..5]` | `Identifier("a")`, `OpenBracket`, `Int(2)`, `Dot`, `Dot`, `Int(5)`, `CloseBracket` | Range syntax removed; use `[slice a 2 5]` |
| `---` (between exprs) | Document separator | `doc_separator` rule |
| `----` | Identifier | `!ident_char` prevents separator match |

---

## 8. Syntax Reference

```tinct
# Document structure
# A file contains documents separated by ---
# Each document contains one or more expressions
# Sequential expressions form a scope chain

[x: 10]                         # Expression 1
[y: [+ x 1]]                   # Expression 2 (sees x from parent scope)
---                             # Document separator (total isolation)
[z: %.x]                       # New document (% is previous doc's output)

# Named pipeline sections
--- %defaults
[host: "localhost"  port: 8080]

--- %overrides
[host: "prod.example.com"  tls: true]

---
[merge %defaults %overrides]   # access both named sections

# Data
[key: "value"]                  # Dict (keys are bare, values are quoted strings)
[$a $b $c]                      # Data sequence of references (escaped-ref head)
[]                              # Empty dict/list
"hello world"                   # Quoted string (needed for spaces/special chars)
42                              # Int
3.14                            # Float
true false                      # Bool

# Mixed keyed/unkeyed
[f x timeout: 60]               # Implied call with named argument
[a "b" key: "val" c]           # Positional and named freely interleaved

# References (bare words)
x                               # Variable reference
[$key: value]                   # Computed key ($-prefix) and bare reference value

# Dot access (removed: bracket access — use get builtin instead)
person.name                     # dot access: Key::String("name") lookup
config.database.host            # chained dot access
# Removed in access-pipeline-phase2: data[5], data[$key], data[2..5], config.services[0].host
# Use: [get data 5], [get data key], [slice data 2 5], [get [get config.services 0] "host"]

# Position-based access (functions, not syntax)
[nth data 0]                    # first entry by position
[nth data -1]                   # last entry (negative = from end)
[last data]                     # last entry (alias)
[slice data 2 5]                # entries at positions 2, 3, 4

# Function application — implied call (preferred)
[f arg1 arg2]                   # Positional args
[f arg1 opt: val]               # Named args

# Function application — explicit call (for computed functions)
[call [get-handler request] data]   # function from another call
[call % data]                       # pipeline value used as function

# Implicit lambda (_ shorthand — same as $_ before)
[+ _ 1]                         # desugars to [fn [_] [+ _ 1]]
[> _.age 30]                    # desugars to [fn [_] [> _.age 30]]

# Apply (spread list into function args)
[apply f arg-list]              # Spreads list entries as positional args

# Function definition
[fn@Number [x@Number  y@Number]
  [+ x y]]

# Named function (just a dict entry)
add: [fn@Number [x@Number  y@Number]
  [+ x y]]

# Named parameters (Kotlin model: any parameter can be named)
fetch: [fn@String [url@String  timeout@[type: Number  default: 30]]
  ...]

# Variadic parameters
apply-all: [fn [f ...args] [map f args]]

# Type alias
Name: [type TypeExpression]

# @ property annotations
param@Type                      # Shorthand: param@[type: Type]
param@[type: T  default: val]   # Full form with properties
fn@Type                         # Return type (shorthand)
fn@[type: T  doc: "..."]        # Return type with properties

# @ type assertions (on expressions)
[@Number expr]                  # Assert type, throw on mismatch
[@[type: Number  default: 0] expr]  # Safe cast with fallback

# Type expressions
[key: Type ...]                 # Open record type
[key: Type]                     # Closed record type
[Type]                          # List type
[Fn@b [a]]                     # Function type (mirrors fn definition)
Any                             # Dynamic escape hatch

# Materialization (explicit, runtime-supported)
[eval %]                        # Recursively force all thunks into memory

# Include
utils: [include "lib/utils.llt"]   # Namespaced
[include "lib/utils.llt"]          # Merged into scope (as top-level expression)

# Conditionals (stdlib functions)
[if cond then else]             # Returns then or else
[when cond body]                # Returns body or [] (expression-safe)
[unless cond body]              # Returns body or [] (expression-safe)

# Pipelines (using _ shorthand for multi-arg functions)
[-> data
    [filter [> _.age 30] _]    # two _ levels: inner = element, outer = collection
    [map _.name _]             # inner _.name = element transform, outer _ = collection
    sort]                      # Already 1-arg, no _ needed

# Comments
# This is a comment
[x: 5]  # Inline comment
```
