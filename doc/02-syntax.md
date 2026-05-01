# Syntax

This document describes the tinct language syntax: its design rationale, formal grammar, tokenization rules, and quick reference. For evaluation semantics (how these constructs execute), see [Evaluation](08-evaluation.md). For the complete language documentation see [doc/index.md](index.md).

---

## 1. Overview

### Single Bracket Syntax

**`[]` is the only bracket type.** No `()` in the language. Every expression uses `[]`. The `call` keyword distinguishes function application from data — the bracket type is not needed for this. Positional and named entries may be freely interleaved — auto-indices are assigned sequentially to positional entries regardless of where named entries appear. See Principle 2 for the auto-indexing and parsing rules.

**Why single brackets:**
- Simpler — one bracket type, one concept
- `()` and `{}` are both freed for future use
- `call` already signals function application — `()` was redundant
- `[]` is familiar from JSON, Python, JavaScript
- True unification: there's one data structure, so there's one syntax

**Parser complexity trade-off:** Single brackets with overloaded semantics require careful disambiguation: keyword recognition (`call`/`fn`/`type` vs dict entries), access chain whitespace sensitivity (`$a.b` vs `$a .b`), and special-form parsing. This complexity is concentrated in the parser — the evaluator and user-facing syntax remain simple.

### Special Forms vs Stdlib Functions

**Lazy evaluation means most "control flow" is just regular functions.** In an eager language, `if` must be a special form because both branches would be evaluated before `if` runs. In tinct, all arguments are thunks — the unused branch is never materialized.

Only constructs that affect **binding structure** or **dict construction** need to be special forms (built into the language). The parser recognizes these by checking the first entry of every `[]`:

| Language-level (special forms) | Why |
|-------------------------------|-----|
| `call` | Triggers function application (exact arity required) |
| `fn` | Introduces parameter bindings, creates a new scope |
| `type` | Compile-time type declaration, not a runtime value |

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
[call $if [call $> $x 0] positive non-positive]
[call $and [call $valid? $input] [call $process $input]]  # process never called if invalid
[call $or $cached-value [call $expensive-compute]]        # compute skipped if cached
```

### Special Form Recognition

**The parser recognizes special forms by keyword.** When the first entry of a `[]` is a bare word matching `call`, `fn`, or `type`, the parser emits a specialized AST node. Otherwise it emits a `Dict` node.

```tinct
[call $f $x]          # CallExpr — first entry is bare word "call"
[fn [x] $x]           # FnExpr
[type [Fn@b [a]]]     # TypeExpr

[call: something]     # Dict — "call" followed by ":" is a key, not a keyword
[$call $x $y]         # Dict — $call is a variable reference, not the keyword
[mycall $f $x]        # Dict — "mycall" is not a recognized keyword
[call
: value]              # CallExpr — newline breaks colon_ahead (it only matches spaces/tabs, not newlines)
```

**Note on `colon_ahead`:** The lookahead pattern that rejects `call:` as a dict key only matches horizontal whitespace (spaces and tabs). A newline between the keyword and colon breaks the pattern, so `[call\n: x]` is a CallExpr, not a dict entry. This is documented formally in §6 Complete Grammar (`colon_ahead = ws_chars* ~ ":"` where `ws_chars = " " | "\t"`).

**Why parser-level:** The distinction between special forms and data must be unambiguous before evaluation. If deferred to the evaluator, `[call $f $x]` would first be constructed as a dict `[0: call  1: $f  2: $x]`, then the evaluator would need to inspect key 0 — but at that point the dict is already a thunk and the string `"call"` is indistinguishable from user data that happens to contain the word "call". Parser-level recognition avoids this ambiguity entirely.

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

**Whitespace significance:** Although whitespace is skipped between tokens in most contexts, it is *significant* for distinguishing access chains from separate expressions:

- `$a.b` — dot access (no whitespace before `.`)
- `$a .b` — VarRef `$a` followed by bare word `.b`
- `$a[0]` — bracket access (no whitespace before `[`)
- `$a [0]` — VarRef `$a` followed by nested expression `[0]`

This is handled by the hand-written lexer at `src/lexer.rs:120-129` using `last_significant_token` tracking for O(1) whitespace-sensitive access detection.

### 2.2 Brackets and Punctuation

The following punctuation characters are used as inline literals throughout the grammar (not as named token rules):

| Character | Purpose |
|-----------|---------|
| `[`, `]` | Bracket expressions, param lists, access chains |
| `:` | Key-value separator |
| `;` | Entry separator |
| `@` | Annotation separator |
| `...` | Variadic parameter prefix |
| `..` | Range operator (inside bracket access) |
| `---` | Document separator (via `doc_separator` rule) |

### 2.3 Literals

#### Literal Recognition

**The lexer recognizes literals by pattern, not the evaluator.** This is consistent with parser-level special form recognition — the distinction between literal types is made at lexing/parsing time, before any evaluation occurs. The hand-written lexer at `src/lexer.rs` implements this recognition.

**Quoting forces string interpretation.** `"true"` is the string `"true"`, `"42"` is the string `"42"`. Quoting is the escape hatch from literal recognition.

**Why lexer-level:** If `true` and `42` were bare-word strings that the evaluator later reinterpreted, it would break the "bare words are always strings" rule in confusing ways — `hello` would be a string but `true` would secretly be a boolean. By having the lexer recognize these patterns first, the rule becomes precise: bare words that don't match any prior pattern (sigil, numeric, boolean) are strings.

Literals are recognized in precedence order. The first matching rule wins.

#### 2.3.1 Variable References

**`$` sigils.** Bare words are always string literals. `$word` is always a variable reference. This applies uniformly — no positional rules, no special cases.

```tinct
[
    name: Alice                  # key "name", value is string "Alice"
    greeting: [call $str "Hello " $name]  # $name references the binding -> "Alice"
    $computed-key: some-value    # key is a reference (computed), value is string
]
```

**Why `$`:**
- Dict keys can be references too — `[$key: $value]` — no special syntax needed for computed keys
- `[name: $name]` is visually clear: key "name" gets the *value* of `name`
- Functions are values: `[call $map ...]` makes it obvious `map` is a reference being looked up, not a keyword
- Bare strings don't need quotes: `[env: production]` just works
- Synergy with string interpolation (if added): `"Hello $name"`

**No special case for `call`.** The function position uses `$` like any other reference. This reinforces that functions are regular values.

`$` starts a variable reference. The identifier after `$` follows these character rules:

```ebnf
var_ref = @{ "$" ~ var_ident }
var_ident = @{ var_ident_char+ }
var_ident_char = _{
    !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | ".")
    ~ ANY
}
```

Identifier characters use a denylist approach — any character is valid except structural delimiters and `.` (which triggers dot access). This means `$` itself is a valid identifier character: `$$` is VarRef("$") (the inter-document pipeline), `$$foo` is VarRef("$foo"), and `$0` is VarRef("0").

**Denylist rationale:** The denylist approach (allow-by-default, exclude structural delimiters) provides extensibility for new operators without reserved keywords, and enables full Unicode identifier support (emoji, non-Latin scripts) without explicit allow-lists.

**Unicode homograph risk:** Unicode homographs (e.g., Cyrillic `а` vs Latin `a`) create invisible name collisions; tinct currently accepts all Unicode identifier characters without NFC normalization.

The token ends at the first excluded character. `.` and `[` are **not** part of the variable name — they are separate access operators that the parser chains onto the reference.

A bare `$` not followed by any valid identifier character is a parse error.

Examples: `$name`, `$has?`, `$my-var`, `$_private`, `$get-or`, `$+`, `$>=`, `$->`, `$$`, `$$foo`, `$0`

```tinct
$name                    # Token: VarRef("name")
$has?                    # Token: VarRef("has?")
$my-var                  # Token: VarRef("my-var")
$_private                # Token: VarRef("_private")
$$                       # Token: VarRef("$") — inter-document pipeline
$$foo                    # Token: VarRef("$foo")
$0                       # Token: VarRef("0")
$data.name               # Tokens: VarRef("data"), Dot, BareWord("name")
$data[0]                 # Tokens: VarRef("data"), BracketAccess, Int(0), CloseBracket
```

A bare `$` not followed by any valid identifier character is a parse error.

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

#### 2.3.5 Bare Words

Bare words are the fallback — any token that doesn't match a prior rule. They are unquoted string literals.

```ebnf
bare_word = @{ bare_word_start ~ bare_word_cont* }

bare_word_start = _{
    !("$" | "#" | "[" | "]" | ":" | ";" | "\"" | "@"
      | " " | "\t" | "\r" | "\n"
      | "...")
    ~ bare_word_char
}

bare_word_cont = _{
    !(" " | "\t" | "\r" | "\n"
      | "[" | "]" | ":" | ";" | "#" | "\"" | "@")
    ~ bare_word_char
}

bare_word_char = _{
    !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "$")
    ~ ANY
}
```

`bare_word_char` uses a denylist — any character except structural delimiters and `$` (which starts variable references). Both `@` and `$` are excluded from `bare_word_char`, meaning they are excluded from `bare_word_cont` as well (since `bare_word_cont` requires `bare_word_char`). `bare_word_start` and `bare_word_cont` add additional exclusions on top of `bare_word_char`: `bare_word_start` additionally excludes `$`, `"..."`, and `#`; `bare_word_cont` additionally excludes `#`. The `"..."` exclusion is only needed at token start (variadic sigil context).

**Valid characters (denylist approach):** Like variable identifiers, bare words use a denylist — any character is valid *except* structural delimiters: whitespace, `[`, `]`, `:`, `;`, `#`, `"`, `@`, and `$`. This means `[a-zA-Z0-9_/.-]`, Unicode, and most other characters are all valid in bare words. Bare words are the default — anything that isn't a recognized special token is a bare word.

**Cannot start with:** `$`, `@`, `#`, `[`, `]`, `:`, `;`, `"`, or `...` (variadic sigil). These characters have special meaning at the start of a token.

**Bare word terminators** — a bare word ends at:

| Character | Purpose |
|-----------|---------|
| Whitespace | Ends the bare word |
| `[` | Ends the bare word (starts bracket access or new expression) |
| `]` | Ends the bare word (closes enclosing expression) |
| `:` | Ends the bare word (key-value separator) |
| `;` | Ends the bare word (entry separator) |
| `#` | Ends the bare word (starts comment) |
| `@` | Ends the bare word (starts annotation) |
| `$` | Ends the bare word (starts variable reference) |

```tinct
hello                    # Bare word: "hello"
some.file.txt            # Bare word: "some.file.txt"
path/to/file             # Bare word: "path/to/file"
my-key                   # Bare word: "my-key"
config..bak              # Bare word: "config..bak"
日本語                    # Bare word: "日本語" (Unicode)

# These are NOT bare words — first character is special
$name                    # Variable reference (starts with $)
"has spaces"             # Quoted string (starts with ")
#comment                 # Comment (starts with #)
[list]                   # Bracketed expression (starts with [)
```

Examples: `hello`, `some.file.txt`, `path/to/file`, `my-key`, `config..bak`

**Tree-sitter divergence on dot in bare words:** The tinct parser allows `.` in `bare_word_char`, enabling filenames like `file.txt` as bare words. It uses whitespace-sensitive lexing to disambiguate `$a.b` (access) from `$a .b` (two tokens). The tree-sitter grammar (`tree-sitter-llt/grammar.js`) excludes `.` from `bare_word_char` entirely, requiring such values to be quoted: `"file.txt"`. This divergence is intentional: tree-sitter's incremental parsing model makes whitespace-sensitive lookahead expensive, and excluding `.` from bare words simplifies the access chain tokenization by using `token.immediate()` instead. This is a confirmed design decision: the main parser prioritizes ergonomics (unquoted filenames), tree-sitter prioritizes incremental parsing performance.

### 2.4 Token Precedence

When classifying a bare token, the tokenizer applies rules in this order:

1. `$` sigil → `var_ref`
2. Numeric → `float_lit` or `int_lit`
3. `true`/`false` → `bool_lit`
4. `"` → `quoted_string`
5. If followed by `@` (in value position), treat as annotated value (`Fn@Number` → `annotated_bare`). This rule applies at the `atom` level only (value position). At the bracket-expression level, `[fn@Type ...]` is handled by `fn_form`'s explicit `fn_annotation?` component, making `fn` there a keyword, not an annotated bare word.
6. Everything else → `bare_word`

This order is enforced by the hand-written parser's token dispatch logic.

### 2.5 Tokenization Rules for `.`, `[`, `..`, and `@`

These characters have context-dependent meaning and require careful disambiguation.

#### `.` Dot Access

`.` immediately after a `$ref` token or after a `]` closing a bracket access (no whitespace) triggers dot-access parsing. The parser reads the next bare word as the key name.

`.` with whitespace before it is part of a bare word string — it has no special meaning.

Whitespace (including newlines) after `.` is permitted — the field name may appear on the next line for readability.

```tinct
$person.name             # Dot access: get key "name" from $person
$config.database.host    # Chained dot access: $config -> "database" -> "host"
$data[0].name            # Dot access after bracket access

$config
  .database              # Line-continuation: same as $config.database
  .host                  # Chained: $config.database.host

some.file.txt            # Bare word string: "some.file.txt" (no $ prefix)
$x . y                   # $x is a VarRef, ". y" is not dot access (whitespace before .)
```

| Input | Tokens | Interpretation |
|-------|--------|----------------|
| `$a.b` | `VarRef("a")`, `Dot`, `BareWord("b")` | Dot access |
| `a.b` | `BareWord("a.b")` | String containing a dot |
| `$a .b` | `VarRef("a")`, `BareWord(".b")` | VarRef then separate string |
| `$a[0].b` | `VarRef("a")`, `BracketAccess`, `Int(0)`, `CloseBracket`, `Dot`, `BareWord("b")` | Bracket then dot |

#### `[` Bracket Access

`[` immediately after a `$ref` token or after `.key` or after `]` (no whitespace) triggers bracket-access parsing. The tokenizer reads the contents as an expression (variable ref, integer, or range) up to the matching `]`.

`[` with whitespace before it starts a new nested `[]` expression (a dict/list/call).

```tinct
$data[5]                 # Bracket access: key 5 on $data
$data[$key]              # Bracket access: computed key
$data[2..5]              # Bracket access with range
$config.services[0].host # Bracket access in a chain

$data [5]                # Two separate things: VarRef("data"), then list [0: 5]
```

| Input | Tokens | Interpretation |
|-------|--------|----------------|
| `$a[0]` | `VarRef("a")`, `BracketAccess`, `Int(0)`, `CloseBracket` | Bracket access |
| `$a [0]` | `VarRef("a")`, `OpenBracket`, `Int(0)`, `CloseBracket` | VarRef then new list |
| `$a[0][1]` | `VarRef("a")`, `BracketAccess`, `Int(0)`, `CloseBracket`, `BracketAccess`, `Int(1)`, `CloseBracket` | Chained bracket access |
| `$a.b[0]` | `VarRef("a")`, `Dot`, `BareWord("b")`, `BracketAccess`, `Int(0)`, `CloseBracket` | Dot then bracket |

#### `..` Range Operator

Inside bracket access (`$data[2..5]`), two consecutive dots form the range operator. The tokenizer recognizes `..` only in the bracket-access context. Outside bracket access, `..` is literal — it is part of a bare word string.

```tinct
$data[2..5]              # Range: keys in [2, 5)
$data[2..]               # Range: keys >= 2
$data[..3]               # Range: keys < 3

config..bak              # Bare word string: "config..bak"
path/to/../file          # Bare word string: "path/to/../file"
```

| Input | Context | Interpretation |
|-------|---------|----------------|
| `$data[2..5]` | Inside bracket access | Range operator: keys 2 to 5 |
| `$data[..]` | Inside bracket access | Range operator: all keys |
| `file..bak` | Bare word | String: "file..bak" |
| `a..b` | Bare word | String: "a..b" |

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

**Document:** A sequence of expressions that form a scope chain. Each expression's result becomes the parent scope for the next expression. Documents are isolated from each other — the only connection is `$$`, which carries the previous document's output as a lazy value. For the first document, `$$` is `[]`. For evaluation semantics of `$$` binding, `$include` cycle detection, and document pipeline caching, see [Documents](09-documents.md).

**Expression:** A single value (bracket expression, atom, access expression, etc.). The `!doc_separator` negative lookahead prevents `---` from being consumed as a bare word.

**`doc_separator`:** Three hyphens `---` not followed by a `bare_word_char`. This prevents `----` or `---foo` from matching as a separator. The parser treats this as atomic so that whitespace is not skipped between the hyphens and the lookahead.

An empty file (or one containing only whitespace/comments) is valid and produces a file with one document containing zero expressions. An empty document produces an empty Dict `[]`.

### 3.2 Bracket Expressions

A bracket expression is the fundamental syntactic unit. The parser examines the first entry to determine whether it is a special form or a dict:

```ebnf
bracket_expr = {
    "[" ~ "]"                           // empty: []
    | "[" ~ type_assert_body ~ "]"      // type assertion: [@Type expr]
    | "[" ~ special_form ~ "]"          // call, fn, type
    | "[" ~ dict_entries ~ "]"          // data: entries
}
```

### 3.3 Special Forms

Special forms are recognized when the first token in a `[]` is a bare keyword (not followed by `:`). The parser tries each form before falling back to `dict_entries`.

```ebnf
special_form = {
    call_form
    | fn_form
    | type_form
}
```

#### 3.3.1 `call` — Function Application

```ebnf
call_form = { keyword_call ~ value ~ call_args }

call_args = { (named_arg | value)* }

named_arg = { named_arg_key ~ ":" ~ value }

named_arg_key = @{ "$" ~ var_ident | bare_word }
```

Arity enforcement uses per-parameter coverage, not a simple count — each required parameter (no `default:` annotation) must be covered by either a positional argument at its index or a named argument. Parameters with `default:` annotations are optional. This is enforced at evaluation time, not parse time. See doc/04-functions.md for the formal C-COVERAGE, C-PRIORITY, C-NO-OVERLAP, and C-NAMED-VALID constraints.

Examples:
```tinct
[call $f $x $y]
[call $fetch "https://example.com" timeout: 60]
```

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
[fn [x] $x]
[fn@Number [x@Number y@Number] [call $+ $x $y]]
[fn@[type: Number  doc: "Sum"] [x@Number  y@[type: Number  default: 0]] [call $+ $x $y]]
[fn [f ...args] [call $map $f $args]]
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

Access chains attach to variable references and bracket accesses. Whitespace-sensitivity is achieved by the hand-written lexer using `last_significant_token` tracking — no implicit whitespace skipping between the variable reference and the `.` or `[`.

```ebnf
access_expr = ${ var_ref ~ access_chain+ }

access_chain = ${ dot_access | bracket_access_chain }

dot_access = ${ "." ~ access_field }

access_field = @{ (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_" | "-")* ~ "?"? }

bracket_access_chain = ${ "[" ~ bracket_access_inner ~ "]" }

bracket_access_inner = {
    range_expr
    | value
}

range_expr = { range_value? ~ ".." ~ range_value? }

range_value = { int_lit | var_ref }
```

Range values are limited to integer literals and variable references.

The lexer handles whitespace-sensitivity so that `$a.b` is parsed as a single access expression, but `$a .b` (with space) does not match — `$a` matches as a plain `var_ref` and `.b` is a separate bare word. Note that `.` is excluded from `var_ident_char`, which is what allows `$a.b` to parse as access rather than as a single identifier ending in `.b`.

Similarly, `$a[0]` is bracket access, but `$a [0]` is a VarRef followed by a nested bracket expression.

**Chaining examples:**

| Input | Parse |
|-------|-------|
| `$a.b` | `access_expr(var_ref("a"), dot_access("b"))` |
| `$a[0]` | `access_expr(var_ref("a"), bracket_access(int(0)))` |
| `$a.b[0].c` | `access_expr(var_ref("a"), dot("b"), bracket(int(0)), dot("c"))` |
| `$a[2..5]` | `access_expr(var_ref("a"), bracket(range(int(2), int(5))))` |
| `$a[2..]` | `access_expr(var_ref("a"), bracket(range(int(2), none)))` |
| `$a[..]` | `access_expr(var_ref("a"), bracket(range(none, none)))` |
| `$a[$key]` | `access_expr(var_ref("a"), bracket(var_ref("key")))` |

### 3.5 Dict Entries

```ebnf
dict_entries = { (entry ~ ";"?)* }

entry = { keyed_entry | rest_entry | auto_entry }

keyed_entry = { key ~ ":" ~ value }

rest_entry = @{ "..." ~ annotation_word? }

auto_entry = { value }

key = { bracket_expr | var_ref | quoted_string | bare_token }

bare_token = { float_lit | int_lit | bool_lit | bare_word }
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

atom = { float_lit | int_lit | bool_lit | quoted_string | var_ref | annotated_bare | bare_word }
```

The ordering in `atom` enforces literal precedence (section 2.4). `float_lit` before `int_lit` ensures `3.14` matches as float, not int `3` followed by `.14`. `bool_lit` before `bare_word` ensures `true` matches as boolean. `annotated_bare` before `bare_word` ensures `Fn@Number` is parsed as an annotated value, not as a bare word containing `@`.

`var_ref` appears in both `value` (as a plain reference) and `access_expr` (as the start of an access chain). The parser tries `access_expr` first — if the var_ref is followed immediately by `.` or `[`, it becomes an access expression. Otherwise it falls through to `atom` where it matches as a plain var_ref.

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
annotated_bare = ${ bare_word ~ "@" ~ annotation_value }
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

---

## 4. Value Boundary Rules

**Every entry's value is exactly one token or one `[]` expression.** There are no multi-value entries. Whitespace separates entries — after parsing a key's value (one token or one `[]`), the next whitespace-separated token is the start of a new entry.

```tinct
[name: Alice age: 30]           # Two key-value pairs: name->Alice, age->30
[key: [a b c]]                  # One key-value pair: key->[a b c] (nested [] is a single value)
[call $f $x $y]                 # Function call — $f is the function, $x and $y are arguments
[x: 1 y]                       # OK — x->1 is named; y is auto-indexed as 0
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

Each static constraint should have at least one test in `tests/corpus/invalid/syntax_errors/` demonstrating parser rejection. The six constraints are: Mixed Positional/Named Ordering (allowed, no constraint), Special Form Arity (function position required), Duplicate Key Detection (runtime), Fn Parameter List Structure (no positional after variadic), Bracket Nesting Depth Limit (MAX_PARSE_DEPTH), and Annotation Bracket Restriction (no nested brackets in annotations).

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
doc_separator = "---" ~ !bare_word_char

// === Bracket Expressions ===

bracket_expr = "[" ~ "]"
             | "[" ~ type_assert_body ~ "]"
             | "[" ~ special_form ~ "]"
             | "[" ~ dict_entries ~ "]"

// === Special Forms ===

special_form = call_form | fn_form | type_form

call_form    = keyword_call ~ value ~ call_args
fn_form      = keyword_fn ~ fn_annotation? ~ param_list ~ value
type_form    = keyword_type ~ value

keyword_call = "call" ~ !ident_char ~ !colon_ahead
keyword_fn   = "fn" ~ !ident_char ~ !colon_ahead
keyword_type = "type" ~ !ident_char ~ !colon_ahead

// Lookahead: optional horizontal whitespace then colon.
// ws_chars matches only spaces and tabs (not newlines), so "call\n:" is a CallExpr, not a Dict entry.
colon_ahead = ws_chars* ~ ":"
ws_chars    = " " | "\t"

ident_char = !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "$") ~ ANY

call_args = (named_arg | value)*

named_arg     = named_arg_key ~ ":" ~ value
named_arg_key = "$" ~ var_ident | bare_word

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

key = bracket_expr | var_ref | quoted_string | bare_token

bare_token = float_lit | int_lit | bool_lit | bare_word

// === Values ===

value = access_expr | bracket_expr | atom

atom = float_lit | int_lit | bool_lit | quoted_string | var_ref | annotated_bare | bare_word

// === Generalized Annotations ===

annotated_bare = bare_word ~ "@" ~ annotation_value

// === Access Chains ===

access_expr = var_ref ~ access_chain+

access_chain = dot_access | bracket_access_chain

dot_access = "." ~ access_field

access_field = (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_" | "-")* ~ "?"?

bracket_access_chain = "[" ~ bracket_access_inner ~ "]"

bracket_access_inner = range_expr | value

range_expr = range_value? ~ ".." ~ range_value?

// Values inside range expressions — limited to atoms (no nested brackets in ranges)
range_value = int_lit | var_ref

// === Literals ===

var_ref = "$" ~ var_ident

var_ident = var_ident_char+

var_ident_char = !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | ".") ~ ANY

float_lit = "-"? ~ ASCII_DIGIT+ ~ "." ~ ASCII_DIGIT+

int_lit = "-"? ~ ASCII_DIGIT+

bool_lit = ("true" | "false") ~ !ident_char

quoted_string = "\"" ~ inner_string ~ "\""
inner_string  = (escape_seq | !("\"" | "\\") ~ ANY)*
escape_seq    = "\\" ~ ("\"" | "\\" | "n" | "t" | "r")

bare_word = bare_word_start ~ bare_word_cont*

bare_word_start = !( "$" | "#" | "[" | "]" | ":" | ";" | "\"" | "@"
                   | " " | "\t" | "\r" | "\n"
                   | "..." )
                  ~ bare_word_char

bare_word_cont = !( " " | "\t" | "\r" | "\n"
                  | "[" | "]" | ":" | ";" | "#" | "\"" | "@" )
                 ~ bare_word_char

bare_word_char = !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "$") ~ ANY
```

---

## 7. Token Disambiguation

| Input | Interpretation | Rule |
|-------|---------------|------|
| `$name` | VarRef | `$` sigil |
| `42` | Int literal | Numeric before bare word |
| `3.14` | Float literal | Float before int |
| `true` | Bool literal | Bool before bare word |
| `hello` | Bare word string | Fallback |
| `"hello"` | Quoted string | Quote delimited |
| `"true"` | Quoted string (value `true`) | Quoting overrides |
| `$a.b` | Access chain | No whitespace before `.` |
| `$a .b` | VarRef then bare word | Whitespace before `.` |
| `$a[0]` | Bracket access | No whitespace before `[` |
| `$a [0]` | VarRef then nested expr | Whitespace before `[` |
| `x@Number` | Param with annotation | `@` in param context |
| `fn@String` | fn with return annotation | `@` after `fn` keyword |
| `Fn@Number` | Annotated value | `@` in value context |
| `[@T $e]` | Type assertion | `@` first in `[]` |
| `call` (first in `[]`) | Keyword | Special form recognition |
| `call@Type` (first in `[]`) | Annotated { name: 'call', ... } (NOT keyword) | @ after bare word converts keyword candidate to annotated value |
| `call:` | Key (not keyword) | Colon makes it a key |
| `$call` | VarRef (not keyword) | `$` makes it a reference |
| `a..b` | Bare word `a..b` | `..` outside bracket access |
| `$a[2..5]` | Range access | `..` inside bracket access |
| `config..bak` | Bare word `config..bak` | `..` outside bracket access |
| `---` (between exprs) | Document separator | `doc_separator` rule |
| `----` | Bare word | `!bare_word_char` prevents separator match |

---

## 8. Syntax Reference

```tinct
# Document structure
# A file contains documents separated by ---
# Each document contains one or more expressions
# Sequential expressions form a scope chain

[x: 10]                         # Expression 1
[y: [call $+ $x 1]]            # Expression 2 (sees x from parent scope)
---                             # Document separator (total isolation)
[z: $$.x]                      # New document ($$ is previous doc's output)

# Data
[key: value]                    # Dict (key and value are strings)
[a b c]                         # List (equivalent to [0: a  1: b  2: c])
[]                              # Empty dict/list
"hello world"                   # Quoted string (needed for spaces/special chars)
hello                           # Bare string
42                              # Int
3.14                            # Float
true false                      # Bool

# Mixed keyed/unkeyed
[call $f $x timeout: 60]        # Positional + named entries in one []
[a b key: val c]                # OK — positional and named freely interleaved

# References
$x                              # Variable reference
[$key: $value]                  # Computed key and value

# Key-based access (brackets and dot — semantically equivalent to $get)
$person.name                    # equivalent to [call $get $person name]
$config.database.host           # equivalent to chained $get
$data[5]                        # equivalent to [call $get $data 5]  key 5
$data[-1]                       # equivalent to [call $get $data -1] key -1, NOT last
$dict[$key]                     # equivalent to [call $get $dict $key]
$data[2..5]                     # key-range slice: keys in [2, 5)
$config.services[0].host        # mixed chaining

# Position-based access (functions, not syntax)
[call $nth $data 0]       # first entry by position
[call $nth $data -1]      # last entry (negative = from end)
[call $last $data]              # last entry (alias)
[call $slice $data 2 5]         # entries at positions 2, 3, 4

# Function application (exact arity required)
[call $f $arg1 $arg2]           # Positional args
[call $f $arg1 opt: $val]       # Named args (bare key-value)

# Implicit lambda ($_ shorthand)
[call $+ $_ 1]                  # desugars to [fn [_] [call $+ $_ 1]]
[call $> $_.age 30]             # desugars to [fn [_] [call $> $_.age 30]]

# Apply (spread list into function args)
[call $apply $f $arg-list]      # Spreads list entries as positional args

# Function definition
[fn@Number [x@Number  y@Number]
  [call $+ $x $y]]

# Named function (just a dict entry)
add: [fn@Number [x@Number  y@Number]
  [call $+ $x $y]]

# Named parameters (Kotlin model: any parameter can be named)
fetch: [fn@String [url@String  timeout@[type: Number  default: 30]]
  ...]

# Variadic parameters
apply-all: [fn [f ...args] [call $map $f $args]]

# Type alias
Name: [type TypeExpression]

# @ property annotations
param@Type                      # Shorthand: param@[type: Type]
param@[type: T  default: val]   # Full form with properties
fn@Type                         # Return type (shorthand)
fn@[type: T  doc: "..."]        # Return type with properties

# @ type assertions (on expressions)
[@Number $expr]                 # Assert type, throw on mismatch
[@[type: Number  default: 0] $expr]  # Safe cast with fallback

# Type expressions
[key: Type ...]                 # Open record type
[key: Type]                     # Closed record type
[Type]                          # List type
[Fn@b [a]]                     # Function type (mirrors fn definition)
Any                             # Dynamic escape hatch

# Materialization (explicit, runtime-supported)
[call $eval $$]                 # Recursively force all thunks into memory

# Include
utils: [call $include "lib/utils.llt"]   # Namespaced
[call $include "lib/utils.llt"]          # Merged into scope (as top-level expression)

# Conditionals (stdlib functions)
[call $if $cond $then $else]    # Returns $then or $else
[call $when $cond $body]        # Returns $body or [] (expression-safe)
[call $unless $cond $body]      # Returns $body or [] (expression-safe)

# Pipelines (using $_ shorthand for multi-arg functions)
[call $-> $data
    [call $filter [call $> $_.age 30] $_]  # two $_ levels: inner = element, outer = collection
    [call $map $_.name $_]                 # inner $_.name = element transform, outer $_ = collection
    $sort]                                 # Already 1-arg, no $_ needed

# Comments
# This is a comment
[x: 5]  # Inline comment
```
