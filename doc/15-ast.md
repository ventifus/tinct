# AST & Parser Internals

Every grammar rule maps to an AST node. All nodes carry source span information for error reporting.

## Parser Implementation Overview

Tinct uses a **hand-written iterative descent parser** composed of two phases:

1. **Tokenization** (`src/lexer.rs`): Converts raw input into a flat token stream with accurate source spans. The lexer handles whitespace-sensitive disambiguation (e.g., `a.b` vs `a .b`, `word@annotation` vs `word @annotation`) by tracking whitespace gaps and emitting context-aware tokens (`BracketAccess`, `ImmediateAt`, `Dot`).

2. **Parsing** (`src/parser.rs`): Consumes the token stream using an explicit `Vec<StackFrame>` to avoid Rust call-stack recursion. The iterative parser enforces a maximum nesting depth (`MAX_PARSE_DEPTH = 256`) before allocating stack frames, preventing unbounded memory use.

**Historical note:** Tinct originally used a pest PEG grammar. The hand-written parser replaced pest in sprint parser-core-c3 (commit cc8333c) to gain precise control over error messages, whitespace sensitivity, and stack depth limits.

## AST Node Types

### Top-Level Types

```rust
/// A complete tinct file — one or more documents separated by ---
struct File {
    documents: Vec<Spanned<Document>>,
}

/// A document — one or more expressions forming a scope chain
struct Document {
    expressions: Vec<Spanned<Expr>>,
}
```

The `parse()` function returns `Result<Spanned<File>, ParseError>`.

The `parse_expression(input)` function is a test and convenience helper that parses the input and returns the first expression of the first document. Multi-expression inputs discard all but the first expression; multi-document inputs discard all but the first document (`---`-separated multi-doc input returns only the first document). No scope chain is built — bindings from earlier expressions are not preserved. This is parse-level convenience, not an evaluator.

### Core Expression Type

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
        // True if created by `_` underscore desugaring (src/desugar.rs), false if user-written.
        // Used for origin tracking (Pombrio & Krishnamurthi 2014) — tooling can distinguish
        // sugar-generated from explicit lambdas.
        desugared: bool,
    },
    TypeAlias(Box<Spanned<Expr>>),

    // Type expressions
    TypeAssert {
        annotation: Spanned<Annotation>,
        expr: Box<Spanned<Expr>>,
        // Filled by type checker (write-once elaboration). RefCell provides interior mutability
        // so the type checker can write the resolved type through a shared reference (&Spanned<Expr>)
        // without changing function signatures throughout the pipeline. Parser initializes to None.
        resolved_type: RefCell<Option<Type>>,
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

### Supporting Types

```rust
/// A dict entry — keyed or auto-indexed
struct Entry {
    key: Option<Spanned<Expr>>,   // None = auto-indexed
    value: Spanned<Expr>,
}

/// A named argument in a call expression
struct NamedArg {
    name: String,  // The name field is always the bare identifier without $ prefix even when source uses $key: syntax.
    value: Spanned<Expr>,
}
```

**Named Argument Key Normalization:** The `name` field always contains a bare identifier. Only `key: val` syntax is supported for named arguments in call forms — the parser does not accept `$key: val` (a variable reference followed by `:` in a call context is a syntax error). Named arguments represent parameter name bindings (matched against `Param.name` strings by the evaluator), not value expressions.

```rust
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

### Node Semantics

| AST Node | Source Syntax | Semantics |
|----------|--------------|-----------|
| `Int(42)` | `42` | Integer literal |
| `Float(3.14)` | `3.14` | Float literal |
| `Bool(true)` | `true` | Boolean literal |
| `Str("hello")` | `"hello"` | String literal (quoted) |
| `VarRef("x")` | `x` or `$x` | Variable reference (bare identifier or escaped) |
| `DotAccess` | `a.b` | Key access: `b` on `a` |
| `BracketAccess` | `a[0]` | Key access: `0` on `a` |
| `RangeAccess` | `a[2..5]` | Key-range slice |
| `Dict(entries)` | `["a" "b" "c"]` or `[k: v]` | Dict/list literal |
| `Call` | `[f x]` or `[call f x]` | Function application (implied or explicit) |
| `Fn` | `[fn [x] body]` | Function definition |
| `TypeAlias` | `[type expr]` | Type alias declaration |
| `TypeAssert` | `[@T expr]` | Type assertion |
| `Annotated` | `Fn@Number` | Annotated bare word |
| `Rest(None)` | `...` | Open record marker |
| `Rest(Some("r"))` | `...r` | Named row variable |

---

## Static Constraints

These constraints are enforced by the parser. Violations produce parse errors with source locations.

### Mixed Positional and Named Ordering

Positional (auto-indexed) and keyed (named) entries may appear in any order within `[]`, for both dict entries and call arguments. Auto-indices are assigned sequentially to positional entries regardless of interleaving:

```tinct
[$a $b key: val]        # valid: 0: ref(a), 1: ref(b), key: val
[key: val $a $b]        # valid: key: val, 0: ref(a), 1: ref(b)
[$a key: val $b]        # valid: 0: ref(a), key: val, 1: ref(b)
```

Rest entries (`...` and `...name`) may also appear at any position.

**Call argument binding.** While the parser allows any ordering, the evaluator binds arguments to parameters using a priority chain: positional arguments bind by index first, then named arguments fill remaining parameters, then defaults apply. See doc/04-functions.md §Call Convention for the formal C-PRIORITY binding rules.

### Special Form Arity

| Form | Minimum args | Notes |
|------|-------------|-------|
| `call` | 1 (the function) | Additional args depend on function arity |
| `fn` | 2 (param list + body) | Optional annotation between `fn` and param list |
| `type` | 1 | Exactly one type expression |

### Duplicate Key Detection

Duplicate keys within a single `[]` literal are parse errors:

```tinct
[name: "Alice"  name: "Bob"]          # ERROR: duplicate key "name"
["a" "b" "a"]                         # Not a duplicate — auto-indexed as 0: "a", 1: "b", 2: "a"
```

Duplicate detection applies to explicit keys only. Auto-indexed entries cannot duplicate because the counter always increments.

**Note:** The parser detects duplicates among literal keys and VarRef keys at parse time. VarRef keys are compared by variable name -- `[$k: a  $k: b]` is a parse error because `$k` appears twice as a key, regardless of what value `$k` might resolve to at runtime. Bracket expression keys (`[[expr]: value]`) bypass the parse-time duplicate check; the evaluator performs runtime duplicate detection to catch computed keys (e.g., `[$k1: a  $k2: b]` where `$k1` and `$k2` resolve to the same value, or `[[call $f]: a  [call $g]: b]` where both calls produce the same key). Both checks produce errors with source locations.

### `fn` Parameter List Structure

The parameter list in `fn` must be a `[]` containing zero or more `param` entries, optionally ending with one variadic parameter (`...name`). Parameters are bare words (not variable references — no `$`):

```tinct
[fn [x y] body]                   # valid
[fn [x@Number y] body]            # valid: x has annotation
[fn [x ...rest] body]             # valid: variadic
[fn [...a ...b] body]             # ERROR: multiple variadics
[fn [...rest x] body]             # ERROR: parameter after variadic
[fn [$x] body]                    # ERROR: $x is a var ref, not a param name
```

### Bracket Nesting Depth Limit

`MAX_LEX_DEPTH = 256` is enforced at the lexer level: when the 257th `[` is encountered the lexer immediately returns an error, so for pure bracket nesting the lexer check fires before the parser ever sees a `Token::OpenBracket`. The iterative parser (`Vec<StackFrame>` main loop) also bounds nesting depth by heap, not the native call stack. `MAX_PARSE_DEPTH` (256) is checked on `stack.len()` before each push, firing before any allocation — inputs exceeding this limit produce a clear parse error.

### Parser Output

`parse()` returns `Result<Spanned<File>, ParseError>`. The underlying `parse2()` function returns `ParseOutput { file: Spanned<File>, source: String, leading_comments: BTreeMap<usize, Vec<String>>, trailing_comments: BTreeMap<usize, String> }` with comment side-tables for formatter support. The main `parse()` entry point extracts `.file` from `ParseOutput` for evaluator and type checker use; formatters can call `parse2()` directly to access comments.

### Annotation Bracket Restriction

Annotation bracket expressions (e.g., `x@[type: Number  default: 30]`) must contain only dict entries. Special forms within annotations are parse errors. When a `type:` key is present, rest entries (`...` or `...name`) are also forbidden — they have no defined semantics in property dict context:

```tinct
x@[type: Number  default: 30]    # valid: property dict
x@Number                         # valid: simple annotation
[@[name: String  ...] $val]      # valid: type expression with rest (no type: key)
x@[call $f $x]                   # ERROR: special form in annotation bracket
x@[fn [a] $a]                    # ERROR: special form in annotation bracket
x@[type Number]                  # ERROR: special form in annotation bracket
x@[@Number $val]                 # ERROR: type_assert_body in annotation bracket
x@[type: Int  ...]               # ERROR: rest entry alongside type: key
```

The following constructs are rejected inside annotation brackets:

| Rejected form | Why |
|--------------|-----|
| `call` | Special form — produces `Expr::Call`, not `Expr::Dict` |
| `fn` | Special form — produces `Expr::Fn`, not `Expr::Dict` |
| `type` | Special form — produces `Expr::TypeAlias`, not `Expr::Dict` |
| `type_assert_body` (`[@Annotation expr]`) | Produces `Expr::TypeAssert`, not `Expr::Dict` — rejected even though it is not a named special form keyword |

All four are caught by the same check in `parse_annotation`: after re-parsing the bracket sub-string via `parse2`, any result that is not `Expr::Dict` is a parse error. `type_assert_body` is rejected on this basis, not because of keyword disambiguation.

When no `type:` key is present, the bracket is interpreted as a type expression (record type), and rest entries are allowed for row polymorphism.

---

## Desugaring Rules

Sections below describe transformations applied by the parser when building the AST.

### Access Chains

Dot notation and bracket notation desugar to nested access nodes:

| Surface syntax | AST |
|---------------|-----|
| `data.name` | `DotAccess(VarRef("data"), "name")` |
| `data[5]` | `BracketAccess(VarRef("data"), Int(5))` |
| `data[key]` | `BracketAccess(VarRef("data"), VarRef("key"))` |
| `data[2..5]` | `RangeAccess(VarRef("data"), Some(Int(2)), Some(Int(5)))` |
| `data[2..]` | `RangeAccess(VarRef("data"), Some(Int(2)), None)` |
| `data[..3]` | `RangeAccess(VarRef("data"), None, Some(Int(3)))` |
| `a.b[0].c` | `DotAccess(BracketAccess(DotAccess(VarRef("a"), "b"), Int(0)), "c")` |

### Auto-Indexing

Entries without explicit keys receive auto-incrementing integer keys. The counter starts at 0 and increments only for unkeyed entries:

| Surface syntax | Logical structure |
|---------------|------------------|
| `[$a $b $c]` | `[0: ref(a)  1: ref(b)  2: ref(c)]` |
| `[greet "Andrew" timeout: 60]` | `[0: ref(greet)  1: "Andrew"  timeout: 60]` |

In the AST, auto-indexed entries have `key: None`. The integer keys are assigned during evaluation, not parsing.

**Note:** Auto-indexing is an eval-time key assignment, not an AST transformation. The AST preserves `key: None` for positional entries. This differs from `_` implicit lambda desugaring (§Desugaring Rules), which is a true AST rewrite performed by the parser.

### Annotation Shorthand

`x@Number` is shorthand for `x@[type: Number]`:

| Surface syntax | Annotation AST |
|---------------|---------------|
| `x@Number` | `Annotation::Simple("Number")` |
| `x@[type: Number  default: 30]` | `Annotation::PropertyDict(...)` |
| `fn@String` | `Annotation::Simple("String")` |

The expansion to `[type: Number]` happens during evaluation, not parsing. The AST preserves the shorthand form.

### Bare Keyword Detection

The parser examines the first token of every `[]` to detect special forms:

| First token | Followed by | AST node |
|------------|-------------|----------|
| `call` | not followed by (optional horizontal whitespace then) `:` | `Call` |
| `fn` | not followed by (optional horizontal whitespace then) `:` | `Fn` |
| `type` | not followed by (optional horizontal whitespace then) `:` | `TypeAlias` |
| `@` | (at bracket start) | `TypeAssert` |
| anything else | — | `Dict` |

Edge cases:
- `[call: something]` — `call` followed by `:` (with no newline between) makes it a key, not a keyword. Parsed as `Dict`.
- `[call\n: something]` — newline between `call` and `:` allows keyword recognition; parsed as `Call` with no arguments (the `: something` is parsed separately as an error or discarded depending on context).
- `[$call x]` — `$call` is an escaped reference in head position, not the bare keyword `call`. Parsed as `Dict` (data sequence).

### `_` Implicit Lambda Desugaring

See [Functions](04-functions.md) for `_` implicit lambda desugaring rules.

**Origin Tracking:** The `Expr::Fn.desugared: bool` field (line 96) tracks whether a lambda was user-written (`false`) or generated by `_` desugaring (`true`). This origin tagging follows Pombrio & Krishnamurthi (2014)'s approach to preserving provenance through AST transformations. Tooling can use this field to distinguish sugar-generated lambdas from explicit lambdas — useful for error messages, IDE navigation, and debugging. For example, a type error in a desugared lambda could point to the original `_` expression rather than the generated `[fn [_] ...]` form.
