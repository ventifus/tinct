# LLT Language Specification

Version 0.1 — derived from DESIGN.md (61 confirmed decisions)

This document is a formal specification of the LLT language syntax. It defines the lexical grammar (tokenization), syntactic grammar (parsing), AST node types, desugaring rules, and static constraints. A conforming parser must accept all inputs described by this grammar and reject all others.

For design rationale, see [DESIGN.md](DESIGN.md).

---

## 1. Notation

This specification uses PEG (Parsing Expression Grammar) notation:

| Notation | Meaning |
|----------|---------|
| `a ~ b` | Sequence: `a` followed by `b` |
| `a \| b` | Ordered choice: try `a` first, then `b` |
| `a*` | Zero or more repetitions |
| `a+` | One or more repetitions |
| `a?` | Optional (zero or one) |
| `!a` | Negative lookahead: succeed if `a` does NOT match, consume nothing |
| `&a` | Positive lookahead: succeed if `a` matches, consume nothing |
| `"text"` | Literal string |
| `'c'` | Literal character |
| `'a'..'z'` | Character range |
| `ANY` | Any single character |
| `SOI` | Start of input |
| `EOI` | End of input |
| `PUSH(a)` | Match `a` and push to stack (pest-specific) |

**Conventions:**

- `UPPER_CASE` — token-level rules (leaf nodes in the parse tree)
- `lower_case` — syntactic rules (branch nodes)
- `@{ ... }` — atomic rule: no implicit whitespace skipping inside
- `${ ... }` — compound atomic: sub-rules produce pairs, but no implicit whitespace
- `_{ ... }` — silent rule: matches but produces no pair in the parse tree

---

## 2. Lexical Grammar

### 2.1 Whitespace and Comments

Whitespace and comments are implicitly skipped between tokens in non-atomic rules.

```pest
WHITESPACE = _{ " " | "\t" | "\r" | "\n" }
COMMENT    = _{ "#" ~ (!NEWLINE ~ ANY)* ~ (NEWLINE | EOI) }
```

The `(NEWLINE | EOI)` anchor ensures a comment consumes through the end of the line (or end of input if the comment is on the last line).

**Whitespace significance:** Although whitespace is skipped between tokens in most contexts, it is *significant* for distinguishing access chains from separate expressions:

- `$a.b` — dot access (no whitespace before `.`)
- `$a .b` — VarRef `$a` followed by bare word `.b`
- `$a[0]` — bracket access (no whitespace before `[`)
- `$a [0]` — VarRef `$a` followed by nested expression `[0]`

This is handled by making access chain rules atomic (see section 3.4).

### 2.2 Brackets and Punctuation

The following punctuation characters are used as inline literals throughout the grammar (not as named token rules):

| Character | Purpose |
|-----------|---------|
| `[`, `]` | Bracket expressions, param lists, access chains |
| `:` | Key-value separator |
| `;` | Entry separator (via `semicolon = _{ ";" }`) |
| `...` | Variadic parameter prefix |
| `..` | Range operator (inside bracket access) |
| `---` | Document separator (via `doc_separator` rule) |

### 2.3 Literals

Literals are recognized in precedence order. The first matching rule wins.

#### 2.3.1 Variable References

`$` starts a variable reference. The identifier after `$` follows these character rules:

```pest
var_ref = @{ "$" ~ var_ident }
var_ident = @{ var_ident_char+ }
var_ident_char = _{
    !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | ".")
    ~ ANY
}
```

Identifier characters use a denylist approach — any character is valid except structural delimiters and `.` (which triggers dot access). This means `$` itself is a valid identifier character: `$$` is VarRef("$") (the inter-document pipeline), `$$foo` is VarRef("$foo"), and `$0` is VarRef("0").

A bare `$` not followed by any valid identifier character is a parse error.

Examples: `$name`, `$has?`, `$my-var`, `$_private`, `$get-or`, `$+`, `$>=`, `$->`, `$$`, `$$foo`, `$0`

#### 2.3.2 Numeric Literals

```pest
float_lit = @{ "-"? ~ ASCII_DIGIT+ ~ "." ~ ASCII_DIGIT+ }
int_lit   = @{ "-"? ~ ASCII_DIGIT+ }
```

`float_lit` must be tried before `int_lit` (longer match). Negative integers and floats are supported as literals.

Examples: `42`, `-1`, `3.14`, `-0.5`

#### 2.3.3 Boolean Literals

```pest
bool_lit = @{ ("true" | "false") ~ !ident_char }
```

The `!ident_char` lookahead ensures `truename` is a bare word, not `true` followed by `name`.

```pest
ident_char = _{
    !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "$")
    ~ ANY
}
```

`ident_char` uses a denylist matching `bare_word_char` so that e.g. `call!` is a bare word, not keyword `call` + `!`.

#### 2.3.4 Quoted Strings

```pest
quoted_string = ${ "\"" ~ inner_string ~ "\"" }
inner_string  = @{ (escape_seq | !("\"" | "\\") ~ ANY)* }
escape_seq    = @{ "\\" ~ ("\"" | "\\" | "n" | "t" | "r") }
```

`quoted_string` uses compound-atomic (`${}`) so that its sub-rules (`inner_string`, `escape_seq`) produce pairs in the parse tree while still preventing implicit whitespace skipping between the quotes and content.

Quoting forces string interpretation: `"true"` is the string `"true"`, `"42"` is the string `"42"`.

#### 2.3.5 Bare Words

Bare words are the fallback — any token that doesn't match a prior rule.

```pest
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

**Bare word terminators** — a bare word ends at:
- Whitespace (space, tab, newline)
- `[`, `]`, `:`, `;`, `#`, `"`, `@`

Examples: `hello`, `some.file.txt`, `path/to/file`, `my-key`, `config..bak`

**Note:** The tree-sitter grammar excludes `.` from `bare_word_char` to simplify access chain parsing. This is a divergence from the pest grammar, which allows `.` in bare words (requiring compound-atomic rules to disambiguate `$a.b` from `$a .b`). The tree-sitter grammar uses `token.immediate()` for access chains instead.

### 2.4 Token Precedence

When classifying a bare token, the tokenizer applies rules in this order:

1. `$` sigil → `var_ref`
2. Numeric → `float_lit` or `int_lit`
3. `true`/`false` → `bool_lit`
4. `"` → `quoted_string`
5. Everything else → `bare_word`

This order is enforced by PEG's ordered choice in the `atom` rule.

---

## 3. Syntactic Grammar

### 3.1 File, Document, and Expression

An LLT file contains one or more documents separated by `---`. Each document contains one or more expressions. This is the top-level grammar:

```pest
file          = { SOI ~ document ~ (doc_separator ~ document)* ~ EOI }
document      = { expression* }
expression    = { !doc_separator ~ value }
doc_separator = @{ "---" ~ !bare_word_char }
```

**File:** The outermost unit. Contains documents separated by `---`.

**Document:** A sequence of expressions that form a scope chain. Each expression's result becomes the parent scope for the next expression. Documents are isolated from each other — the only connection is `$$`, which carries the previous document's output as a lazy value. For the first document, `$$` is `[]`.

**Expression:** A single value (bracket expression, atom, access expression, etc.). The `!doc_separator` negative lookahead prevents `---` from being consumed as a bare word.

**`doc_separator`:** Three hyphens `---` not followed by a `bare_word_char`. This prevents `----` or `---foo` from matching as a separator. The rule is atomic (`@{}`) so that whitespace is not skipped between the hyphens and the lookahead.

An empty file (or one containing only whitespace/comments) is valid and produces a file with one document containing zero expressions. An empty document produces an empty Dict `[]`.

### 3.2 Bracket Expressions

A bracket expression is the fundamental syntactic unit. The parser examines the first entry to determine whether it is a special form or a dict:

```pest
bracket_expr = {
    "[" ~ "]"                           // empty: []
    | "[" ~ type_assert_body ~ "]"      // type assertion: [@Type expr]
    | "[" ~ special_form ~ "]"          // call, fn, type
    | "[" ~ dict_entries ~ "]"          // data: entries
}
```

### 3.3 Special Forms

Special forms are recognized when the first token in a `[]` is a bare keyword (not followed by `:`). PEG ordered choice tries each form before falling back to `dict_entries`.

```pest
special_form = {
    call_form
    | fn_form
    | type_form
}
```

#### 3.3.1 `call` — Function Application

```pest
call_form = { keyword_call ~ value ~ call_args }

call_args = { (named_arg | value)* }

named_arg = { named_arg_key ~ ":" ~ value }

named_arg_key = @{ "$" ~ var_ident | bare_word }
```

`call` requires exact arity — the number of positional arguments must match the function's parameter count (enforced at evaluation time, not parse time). Named arguments follow positional arguments.

Examples:
```
[call $f $x $y]
[call $fetch "https://example.com" timeout: 60]
```

#### 3.3.2 `fn` — Function Definition

```pest
fn_form = { keyword_fn ~ fn_annotation? ~ param_list ~ value }

fn_annotation = ${ "@" ~ annotation_value }

param_list = { "[" ~ (variadic_param | param)* ~ "]" }

param = ${ param_name ~ param_annotation? }

param_name = @{ (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_" | "-")* ~ "?"? }

param_annotation = ${ "@" ~ annotation_value }

variadic_param = !{ "..." ~ param_name }
```

Examples:
```
[fn [x] $x]
[fn@Number [x@Number y@Number] [call $+ $x $y]]
[fn@[type: Number  doc: "Sum"] [x@Number  y@[type: Number  default: 0]] [call $+ $x $y]]
[fn [f ...args] [call $map $f $args]]
```

#### 3.3.3 `type` — Type Alias

```pest
type_form = { keyword_type ~ value }
```

Examples:
```
[type [Fn@b [a]]]
[type [name: String  age: Number]]
```

### 3.4 Access Chains

Access chains attach to variable references and bracket accesses. Whitespace-sensitivity is achieved by making the chain atomic — no implicit whitespace skipping between the variable reference and the `.` or `[`.

```pest
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

range_value = { float_lit | int_lit | var_ref }
```

Range values are limited to numeric literals and variable references.

Because `access_expr` is compound-atomic (`$`), `$a.b` is parsed as a single access expression, but `$a .b` (with space) does not match — `$a` matches as a plain `var_ref` and `.b` is a separate bare word.

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

```pest
dict_entries = { (entry ~ semicolon?)* }

semicolon = _{ ";" }

entry = { keyed_entry | rest_entry | auto_entry }

keyed_entry = { key ~ ":" ~ value }

rest_entry = @{ "..." ~ annotation_word? }

auto_entry = { value }

key = { bracket_expr | var_ref | quoted_string | bare_token }

bare_token = { float_lit | int_lit | bool_lit | bare_word }
```

The `rest_entry` rule is atomic (`@`) to prevent whitespace between `...` and the optional name. `...` alone produces `Expr::Rest(None)` (anonymous open record marker). `...name` produces `Expr::Rest(Some("name"))` (named row variable). Rest entries are used in type expressions to indicate open records and row polymorphism.

Quoted strings are valid as keys, allowing keys that contain spaces, colons, or other special characters: `["my key": value]`.

**Note on key types:** The `bare_token` rule in `key` allows `float_lit` and `bool_lit` to parse successfully as keys. However, the evaluator only accepts `String` and `Int` as runtime key types and will reject `Float` and `Bool` keys with a type error. The parser is intentionally permissive here for forward compatibility — if future language versions support additional key types, no grammar changes will be needed.

**Value boundary rule:** every entry's value is exactly one token or one bracket expression. After parsing a key's value, the next whitespace-separated token starts a new entry.

**Positional-before-named constraint:** all auto-indexed (positional) entries must precede all keyed (named) entries within a single `[]`. See section 5.1.

**Semicolons:** `;` acts as an entry separator, equivalent to whitespace. It allows multiple entries on one line:

```
[a: 1; b: 2; c: 3]
```

### 3.6 Values

A value is a single expression — one atom, one access expression, or one bracket expression:

```pest
value = { access_expr | bracket_expr | atom }

atom = { float_lit | int_lit | bool_lit | quoted_string | var_ref | annotated_bare | bare_word }
```

The ordering in `atom` enforces literal precedence (section 2.4). `float_lit` before `int_lit` ensures `3.14` matches as float, not int `3` followed by `.14`. `bool_lit` before `bare_word` ensures `true` matches as boolean. `annotated_bare` before `bare_word` ensures `Fn@Number` is parsed as an annotated value, not as a bare word containing `@`.

`var_ref` appears in both `value` (as a plain reference) and `access_expr` (as the start of an access chain). PEG ordered choice tries `access_expr` first — if the var_ref is followed immediately by `.` or `[`, it becomes an access expression. Otherwise it falls through to `atom` where it matches as a plain var_ref.

### 3.7 Annotations

**`@` is always a structural separator.** It is not a valid bare word character. Wherever `@` appears immediately after a bare word (no whitespace), it separates the word from an annotation value. Strings containing `@` must be quoted: `"email@example.com"`.

**In parameter position** (inside a `param_list`):
```pest
param_annotation = ${ "@" ~ annotation_value }
```
`x@Number` splits into param `x` with annotation `Number`.

**On `fn` keyword** (return type):
```pest
fn_annotation = ${ "@" ~ annotation_value }
```
`fn@Number` means the function returns `Number`.

**In value position** (generalized annotation):
```pest
annotated_bare = ${ bare_word ~ "@" ~ annotation_value }
```
`Fn@Number` produces an `Annotated` node with name `"Fn"` and annotation `Number`. This is used for function type constructors (`Fn@Return [Params]`) and is available for future use on any bare word.

**As type assertion** (first token inside `[]`):
```pest
type_assert_body = { "@" ~ annotation_value ~ value }
```
`[@Number $expr]` asserts `$expr` has type `Number`. When a `default:` is provided (e.g., `[@[type: Number  default: 0] $expr]`), the default value is evaluated in the same environment as the asserted expression.

### 3.8 Type Expressions

Type expressions appear in type annotations and `[type ...]` declarations. They use the same `[]` syntax as data but are distinguished by context (after `@`, inside `type` form).

**Function types** use `Fn@Return [ParamTypes]`, mirroring function definitions (`fn@Return [params] body`):
```
[Fn@b [a]]              # function from a to b
[Fn@Bool [a]]           # predicate
[Fn@c [a b]]            # two-arg function
```

The parser handles this via the `annotated_bare` rule -- `Fn@b` parses as `Annotated { name: "Fn", annotation: Simple("b") }`. The type checker interprets `Fn` as a function type constructor. All types in a type definition must be explicit -- there is no body to infer from.

**Note:** `Fn@Number` in a bare context (not inside `[]`) is also valid and parsed via the `annotated_bare` grammar rule, producing the same AST structure.

**Row polymorphism** is supported via `rest_entry` syntax in type expressions. `...` marks an open record type (any additional fields are permitted), and `...name` introduces a named row variable for polymorphic record operations:

```
[name: String ...]            # open record: has name, allows other fields
[name: String ...r]           # named row variable r captures the remaining fields
```

**Type conventions** (not enforced by parser, enforced by type checker):
- Uppercase first letter = concrete type (`String`, `Number`, `Person`, `Fn`)
- Lowercase first letter = type variable (`a`, `b`, `k`, `v`)
- `Any` = dynamic escape hatch

---

## 4. AST Node Types

Every grammar rule maps to an AST node. All nodes carry source span information for error reporting.

### 4.1 Top-Level Types

```rust
/// A complete LLT file — one or more documents separated by ---
struct File {
    documents: Vec<Spanned<Document>>,
}

/// A document — one or more expressions forming a scope chain
struct Document {
    expressions: Vec<Spanned<Expr>>,
}
```

The `parse()` function returns `Result<Spanned<File>, ParseError>`.

### 4.2 Core Expression Type

```rust
/// Source location
struct Span {
    start: Position,
    end: Position,
}

struct Position {
    offset: usize,  // byte offset
    line: usize,    // 1-based
    column: usize,  // 1-based
}

/// A node with source span
struct Spanned<T> {
    node: T,
    span: Span,
}

/// The central expression type
enum Expr {
    // Literals
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),

    // References and access
    VarRef(String),
    DotAccess {
        expr: Box<Spanned<Expr>>,
        field: String,
    },
    BracketAccess {
        expr: Box<Spanned<Expr>>,
        key: Box<Spanned<Expr>>,
    },
    RangeAccess {
        expr: Box<Spanned<Expr>>,
        start: Option<Box<Spanned<Expr>>>,
        end: Option<Box<Spanned<Expr>>>,
    },

    // Data
    Dict(Vec<Spanned<Entry>>),

    // Special forms
    Call {
        func: Box<Spanned<Expr>>,
        args: Vec<Spanned<Expr>>,
        named_args: Vec<Spanned<NamedArg>>,
    },
    Fn {
        return_ann: Option<Spanned<Annotation>>,
        params: Vec<Spanned<Param>>,
        body: Box<Spanned<Expr>>,
    },
    TypeAlias(Box<Spanned<Expr>>),

    // Type expressions
    TypeAssert {
        annotation: Spanned<Annotation>,
        expr: Box<Spanned<Expr>>,
    },

    // Generalized annotation in value position
    Annotated {
        name: String,
        annotation: Spanned<Annotation>,
    },

    // Row polymorphism marker in type expressions
    Rest(Option<String>),       // ... or ...name
}
```

### 4.3 Supporting Types

```rust
/// A dict entry — keyed or auto-indexed
struct Entry {
    key: Option<Spanned<Expr>>,   // None = auto-indexed
    value: Spanned<Expr>,
}

/// A named argument in a call expression
struct NamedArg {
    name: String,
    value: Spanned<Expr>,
}

/// A function parameter
struct Param {
    name: String,
    annotation: Option<Spanned<Annotation>>,
    variadic: bool,               // true if preceded by ...
}

/// An annotation (type or property dict)
enum Annotation {
    Simple(String),               // x@Number — shorthand
    PropertyDict(Vec<Spanned<Entry>>),  // x@[type: Number  default: 30]
}
```

### 4.4 Node Semantics

| AST Node | Source Syntax | Semantics |
|----------|--------------|-----------|
| `Int(42)` | `42` | Integer literal |
| `Float(3.14)` | `3.14` | Float literal |
| `Bool(true)` | `true` | Boolean literal |
| `Str("hello")` | `hello` or `"hello"` | String literal (bare word or quoted) |
| `VarRef("x")` | `$x` | Variable reference |
| `DotAccess` | `$a.b` | Key access: `b` on `$a` |
| `BracketAccess` | `$a[0]` | Key access: `0` on `$a` |
| `RangeAccess` | `$a[2..5]` | Key-range slice |
| `Dict(entries)` | `[a b c]` or `[k: v]` | Dict/list literal |
| `Call` | `[call $f $x]` | Function application |
| `Fn` | `[fn [x] body]` | Function definition |
| `TypeAlias` | `[type expr]` | Type alias declaration |
| `TypeAssert` | `[@T $expr]` | Type assertion |
| `Annotated` | `Fn@Number` | Annotated bare word |

---

## 5. Static Constraints

These constraints are enforced by the parser. Violations produce parse errors with source locations.

### 5.1 Positional-Before-Named Ordering

Within any `[]` (dict entries or call arguments), all positional (auto-indexed) entries must appear before all named (keyed) entries:

```
[a b key: val]        # valid: positional a, b; then named key
[key: val a b]        # ERROR: positional a after named key
[a key: val b]        # ERROR: positional b after named key
```

Rest entries (`...` and `...name`) are exempt from positional-before-named ordering -- they may appear at any position within a bracket expression. For example, `[name: String ...]` is valid even though `...` is an unkeyed entry appearing after a keyed entry.

### 5.2 Special Form Arity

| Form | Minimum args | Notes |
|------|-------------|-------|
| `call` | 1 (the function) | Additional args depend on function arity |
| `fn` | 2 (param list + body) | Optional annotation between `fn` and param list |
| `type` | 1 | Exactly one type expression |

### 5.3 Duplicate Key Detection

Duplicate keys within a single `[]` literal are parse errors:

```
[name: Alice  name: Bob]          # ERROR: duplicate key "name"
[a b a]                           # Not a duplicate — auto-indexed as 0: a, 1: b, 2: a
```

Duplicate detection applies to explicit keys only. Auto-indexed entries cannot duplicate because the counter always increments.

**Note:** The parser detects duplicates among literal keys and VarRef keys at parse time. VarRef keys are compared by variable name -- `[$k: a  $k: b]` is a parse error because `$k` appears twice as a key, regardless of what value `$k` might resolve to at runtime. Bracket expression keys (`[[expr]: value]`) bypass the parse-time duplicate check; the evaluator performs runtime duplicate detection to catch computed keys (e.g., `[$k1: a  $k2: b]` where `$k1` and `$k2` resolve to the same value, or `[[call $f]: a  [call $g]: b]` where both calls produce the same key). Both checks produce errors with source locations.

### 5.4 `fn` Parameter List Structure

The parameter list in `fn` must be a `[]` containing zero or more `param` entries, optionally ending with one variadic parameter (`...name`). Parameters are bare words (not variable references — no `$`):

```
[fn [x y] body]                   # valid
[fn [x@Number y] body]            # valid: x has annotation
[fn [x ...rest] body]             # valid: variadic
[fn [...a ...b] body]             # ERROR: multiple variadics
[fn [...rest x] body]             # ERROR: parameter after variadic
[fn [$x] body]                    # ERROR: $x is a var ref, not a param name
```

### 5.5 Bracket Nesting Depth Limit

Pest recurses on Rust's call stack for nested bracket expressions, so deeply nested inputs (~500+ levels) may overflow the default 8MB stack before reaching any application-level check. `MAX_PARSE_DEPTH` (256) is the policy limit enforced during AST construction to fail fast with a clear parse error. Inputs exceeding this policy limit produce a parse error. See Phase 7 (hand-written parser) for a planned resolution.

### 5.6 Annotation Bracket Restriction

Annotation bracket expressions (e.g., `x@[type: Number  default: 30]`) must contain only dict entries. Special forms within annotations are parse errors. When a `type:` key is present, rest entries (`...` or `...name`) are also forbidden — they have no defined semantics in property dict context:

```
x@[type: Number  default: 30]    # valid: property dict
x@Number                         # valid: simple annotation
[@[name: String  ...] $val]      # valid: type expression with rest (no type: key)
x@[call $f $x]                   # ERROR: special form in annotation bracket
x@[type: Int  ...]               # ERROR: rest entry alongside type: key
```

When no `type:` key is present, the bracket is interpreted as a type expression (record type), and rest entries are allowed for row polymorphism.

---

## 6. Desugaring Rules

These transformations are applied by the parser when building the AST. They are not separate passes — they describe how surface syntax maps to AST nodes.

### 6.1 Access Chains

Dot notation and bracket notation desugar to nested access nodes:

| Surface syntax | AST |
|---------------|-----|
| `$data.name` | `DotAccess(VarRef("data"), "name")` |
| `$data[5]` | `BracketAccess(VarRef("data"), Int(5))` |
| `$data[$key]` | `BracketAccess(VarRef("data"), VarRef("key"))` |
| `$data[2..5]` | `RangeAccess(VarRef("data"), Some(Int(2)), Some(Int(5)))` |
| `$data[2..]` | `RangeAccess(VarRef("data"), Some(Int(2)), None)` |
| `$data[..3]` | `RangeAccess(VarRef("data"), None, Some(Int(3)))` |
| `$a.b[0].c` | `DotAccess(BracketAccess(DotAccess(VarRef("a"), "b"), Int(0)), "c")` |

### 6.2 Auto-Indexing

Entries without explicit keys receive auto-incrementing integer keys. The counter starts at 0 and increments only for unkeyed entries:

| Surface syntax | Logical structure |
|---------------|------------------|
| `[a b c]` | `[0: a  1: b  2: c]` |
| `[greet Andrew timeout: 60]` | `[0: greet  1: Andrew  timeout: 60]` |

In the AST, auto-indexed entries have `key: None`. The integer keys are assigned during evaluation, not parsing.

### 6.3 Annotation Shorthand

`x@Number` is shorthand for `x@[type: Number]`:

| Surface syntax | Annotation AST |
|---------------|---------------|
| `x@Number` | `Annotation::Simple("Number")` |
| `x@[type: Number  default: 30]` | `Annotation::PropertyDict(...)` |
| `fn@String` | `Annotation::Simple("String")` |

The expansion to `[type: Number]` happens during evaluation, not parsing. The AST preserves the shorthand form.

### 6.4 Bare Keyword Detection

The parser examines the first token of every `[]` to detect special forms:

| First token | Followed by | AST node |
|------------|-------------|----------|
| `call` | not followed by (optional whitespace then) `:` | `Call` |
| `fn` | not followed by (optional whitespace then) `:` | `Fn` |
| `type` | not followed by (optional whitespace then) `:` | `TypeAlias` |
| `@` | (at bracket start) | `TypeAssert` |
| anything else | — | `Dict` |

Edge cases:
- `[call: something]` — `call` followed by `:` makes it a key, not a keyword. Parsed as `Dict`.
- `[$call $x]` — `$call` is a variable reference, not the bare keyword `call`. Parsed as `Dict`.

---

## 7. Complete Grammar

The full pest grammar, consolidated from all sections above. This is the normative grammar definition.

```pest
// === Whitespace and Comments ===

WHITESPACE = _{ " " | "\t" | "\r" | "\n" }
COMMENT    = _{ "#" ~ (!NEWLINE ~ ANY)* ~ (NEWLINE | EOI) }

// === File and Document Structure ===

file          = { SOI ~ document ~ (doc_separator ~ document)* ~ EOI }
document      = { expression* }
expression    = { !doc_separator ~ value }
doc_separator = @{ "---" ~ !bare_word_char }

// === Bracket Expressions ===

bracket_expr = {
    "[" ~ "]"
    | "[" ~ type_assert_body ~ "]"
    | "[" ~ special_form ~ "]"
    | "[" ~ dict_entries ~ "]"
}

// === Special Forms ===

special_form = {
    call_form
    | fn_form
    | type_form
}

call_form    = { keyword_call ~ value ~ call_args }
fn_form      = { keyword_fn ~ fn_annotation? ~ param_list ~ value }
type_form    = { keyword_type ~ value }

keyword_call    = @{ "call" ~ !ident_char ~ !colon_ahead }
keyword_fn      = @{ "fn" ~ !ident_char ~ !colon_ahead }
keyword_type    = @{ "type" ~ !ident_char ~ !colon_ahead }

colon_ahead     = _{ ws_chars* ~ ":" }
ws_chars        = _{ " " | "\t" }

ident_char = _{
    !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "$")
    ~ ANY
}

call_args = { (named_arg | value)* }

named_arg = { named_arg_key ~ ":" ~ value }
named_arg_key = @{ "$" ~ var_ident | bare_word }

// === Type Assertions ===

type_assert_body = { "@" ~ annotation_value ~ value }

// === Functions ===

fn_annotation = ${ "@" ~ annotation_value }

param_list = { "[" ~ (variadic_param | param)* ~ "]" }

param = ${ param_name ~ param_annotation? }

param_name = @{ (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_" | "-")* ~ "?"? }

param_annotation = ${ "@" ~ annotation_value }

variadic_param = !{ "..." ~ param_name }

annotation_value = !{ bracket_expr | annotation_word }

annotation_word = @{ (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_" | "-")* ~ "?"? }

// === Dict Entries ===

dict_entries = { (entry ~ semicolon?)* }

semicolon = _{ ";" }

entry = { keyed_entry | rest_entry | auto_entry }

rest_entry = @{ "..." ~ annotation_word? }

keyed_entry = { key ~ ":" ~ value }
auto_entry  = { value }

key = { bracket_expr | var_ref | quoted_string | bare_token }

bare_token = { float_lit | int_lit | bool_lit | bare_word }

// === Values ===

value = { access_expr | bracket_expr | atom }

atom = { float_lit | int_lit | bool_lit | quoted_string | var_ref | annotated_bare | bare_word }

// === Generalized Annotations ===

annotated_bare = ${ bare_word ~ "@" ~ annotation_value }

// === Access Chains ===

access_expr = ${ var_ref ~ access_chain+ }

access_chain = ${ dot_access | bracket_access_chain }

dot_access = ${ "." ~ access_field }

access_field = @{
    (ASCII_ALPHA | "_")
    ~ (ASCII_ALPHANUMERIC | "_" | "-")*
    ~ "?"?
}

bracket_access_chain = ${
    "[" ~ bracket_access_inner ~ "]"
}

bracket_access_inner = {
    range_expr | value
}

range_expr = { range_value? ~ ".." ~ range_value? }

// Values inside range expressions — limited to atoms (no nested brackets in ranges)
range_value = { float_lit | int_lit | var_ref }

// === Literals ===

var_ref = @{ "$" ~ var_ident }

var_ident = @{ var_ident_char+ }

var_ident_char = _{
    !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | ".")
    ~ ANY
}

float_lit = @{ "-"? ~ ASCII_DIGIT+ ~ "." ~ ASCII_DIGIT+ }

int_lit = @{ "-"? ~ ASCII_DIGIT+ }

bool_lit = @{ ("true" | "false") ~ !ident_char }

quoted_string = ${ "\"" ~ inner_string ~ "\"" }
inner_string  = @{ (escape_seq | !("\"" | "\\") ~ ANY)* }
escape_seq    = @{ "\\" ~ ("\"" | "\\" | "n" | "t" | "r") }

bare_word = @{ bare_word_start ~ bare_word_cont* }

bare_word_start = _{
    !( "$" | "#" | "[" | "]" | ":" | ";" | "\"" | "@"
     | " " | "\t" | "\r" | "\n"
     | "..." )
    ~ bare_word_char
}

bare_word_cont = _{
    !( " " | "\t" | "\r" | "\n"
     | "[" | "]" | ":" | ";" | "#" | "\"" | "@" )
    ~ bare_word_char
}

bare_word_char = _{
    !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "$")
    ~ ANY
}
```

---

## 8. Examples

### 8.1 Simple Dict

**Input:**
```
[name: Alice  age: 30]
```

**AST:**
```
Dict([
    Entry { key: Some(Str("name")), value: Str("Alice") },
    Entry { key: Some(Str("age")),  value: Int(30) },
])
```

### 8.2 Simple List

**Input:**
```
[a b c]
```

**AST:**
```
Dict([
    Entry { key: None, value: Str("a") },
    Entry { key: None, value: Str("b") },
    Entry { key: None, value: Str("c") },
])
```

### 8.3 Nested Dict

**Input:**
```
[
    database: [host: localhost  port: 5432]
    api: [endpoint: "/v1"]
]
```

**AST:**
```
Dict([
    Entry {
        key: Some(Str("database")),
        value: Dict([
            Entry { key: Some(Str("host")), value: Str("localhost") },
            Entry { key: Some(Str("port")), value: Int(5432) },
        ])
    },
    Entry {
        key: Some(Str("api")),
        value: Dict([
            Entry { key: Some(Str("endpoint")), value: Str("/v1") },
        ])
    },
])
```

### 8.4 Function Call with Named Args

**Input:**
```
[call $fetch "https://example.com" timeout: 60 retries: 3]
```

**AST:**
```
Call {
    func: VarRef("fetch"),
    args: [Str("https://example.com")],
    named_args: [
        NamedArg { name: "timeout", value: Int(60) },
        NamedArg { name: "retries", value: Int(3) },
    ],
}
```

### 8.5 Function Definition with Annotations

**Input:**
```
[fn@Number [x@Number  y@[type: Number  default: 0]] [call $+ $x $y]]
```

**AST:**
```
Fn {
    return_ann: Some(Annotation::Simple("Number")),
    params: [
        Param { name: "x", annotation: Some(Simple("Number")), variadic: false },
        Param { name: "y", annotation: Some(PropertyDict([
            Entry { key: Some(Str("type")), value: Str("Number") },
            Entry { key: Some(Str("default")), value: Int(0) },
        ])), variadic: false },
    ],
    body: Call {
        func: VarRef("+"),
        args: [VarRef("x"), VarRef("y")],
        named_args: [],
    },
}
```

### 8.6 Pipeline with `$_` Shorthand

**Input:**
```
[call $-> $data.users
    [call $filter [call $> $_.age 30] $_]
    [call $map $_.name $_]
    $sort]
```

Note: `$_` desugaring is an evaluator concern, not a parser concern. The parser produces the AST as-is — `$_` is just `VarRef("_")`. The evaluator wraps `[...]` expressions containing `$_` in implicit lambdas. See DESIGN.md for the `$_` lambda scope rule (nested bracket boundary).

**AST:**
```
Call {
    func: VarRef("->"),
    args: [
        DotAccess { expr: VarRef("data"), field: "users" },
        Call {
            func: VarRef("filter"),
            args: [
                Call {
                    func: VarRef(">"),
                    args: [
                        DotAccess { expr: VarRef("_"), field: "age" },
                        Int(30),
                    ],
                    named_args: [],
                },
                VarRef("_"),
            ],
            named_args: [],
        },
        Call {
            func: VarRef("map"),
            args: [
                DotAccess { expr: VarRef("_"), field: "name" },
                VarRef("_"),
            ],
            named_args: [],
        },
        VarRef("sort"),
    ],
    named_args: [],
}
```

### 8.7 Access Chains

**Input:**
```
$config.services[0].host
```

**AST:**
```
DotAccess {
    expr: BracketAccess {
        expr: DotAccess {
            expr: VarRef("config"),
            field: "services",
        },
        key: Int(0),
    },
    field: "host",
}
```

### 8.8 Range Access

**Input:**
```
$data[2..5]
```

**AST:**
```
RangeAccess {
    expr: VarRef("data"),
    start: Some(Int(2)),
    end: Some(Int(5)),
}
```

### 8.9 Type Assertion

**Input:**
```
[@Number $expr]
```

**AST:**
```
TypeAssert {
    annotation: Annotation::Simple("Number"),
    expr: VarRef("expr"),
}
```

### 8.10 Type Assertion with Fallback

**Input:**
```
[@[type: Number  default: 0] $config.port]
```

**AST:**
```
TypeAssert {
    annotation: Annotation::PropertyDict([
        Entry { key: Some(Str("type")), value: Str("Number") },
        Entry { key: Some(Str("default")), value: Int(0) },
    ]),
    expr: DotAccess { expr: VarRef("config"), field: "port" },
}
```

### 8.11 Type Alias

**Input:**
```
Mapper: [type [Fn@b [a]]]
```

**AST:**
```
Entry {
    key: Some(Str("Mapper")),
    value: TypeAlias(
        Dict([
            Entry { key: None, value: Annotated { name: "Fn", annotation: Simple("b") } },
            Entry { key: None, value: Dict([Entry { key: None, value: Str("a") }]) },
        ])
    ),
}
```

The type checker interprets `Annotated { name: "Fn", ... }` as a function type constructor.

### 8.12 Comments

**Input:**
```
[
    # Configuration
    host: localhost  # server hostname
    port: 8080       # server port
]
```

**AST:**
```
Dict([
    Entry { key: Some(Str("host")), value: Str("localhost") },
    Entry { key: Some(Str("port")), value: Int(8080) },
])
```

Comments are discarded during tokenization and do not appear in the AST.

### 8.13 Variadic Function

**Input:**
```
[fn [f ...args] [call $map $f $args]]
```

**AST:**
```
Fn {
    return_ann: None,
    params: [
        Param { name: "f", annotation: None, variadic: false },
        Param { name: "args", annotation: None, variadic: true },
    ],
    body: Call {
        func: VarRef("map"),
        args: [VarRef("f"), VarRef("args")],
        named_args: [],
    },
}
```

### 8.14 Mixed Positional and Named Entries

**Input:**
```
[call $f $x $y timeout: 60]
```

**AST:**
```
Call {
    func: VarRef("f"),
    args: [VarRef("x"), VarRef("y")],
    named_args: [
        NamedArg { name: "timeout", value: Int(60) },
    ],
}
```

### 8.15 Multi-Expression Document

**Input:**
```
[x: 10]

[y: [call $+ $x 1]]
```

**AST:**
```
File {
    documents: [
        Document {
            expressions: [
                Dict([Entry { key: Some(Str("x")), value: Int(10) }]),
                Dict([Entry { key: Some(Str("y")), value: Call { func: VarRef("+"), args: [VarRef("x"), Int(1)] } }]),
            ]
        }
    ]
}
```

### 8.16 Multi-Document File

**Input:**
```
[data: [name: Alice  age: 30]]
---
[result: $$.data]
```

**AST:**
```
File {
    documents: [
        Document {
            expressions: [
                Dict([Entry { key: Some(Str("data")), value: Dict([...]) }])
            ]
        },
        Document {
            expressions: [
                Dict([Entry { key: Some(Str("result")), value: DotAccess { expr: VarRef("$"), field: "data" } }])
            ]
        }
    ]
}
```

---

## Appendix A: Token Disambiguation Summary

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
| `call:` | Key (not keyword) | Colon makes it a key |
| `$call` | VarRef (not keyword) | `$` makes it a reference |
| `a..b` | Bare word `a..b` | `..` outside bracket access |
| `$a[2..5]` | Range access | `..` inside bracket access |
| `config..bak` | Bare word `config..bak` | `..` outside bracket access |
| `---` (between exprs) | Document separator | `doc_separator` rule |
| `----` | Bare word | `!bare_word_char` prevents separator match |

