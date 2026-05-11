# AST & Parser Internals

Every grammar rule maps to an AST node. All nodes carry source span information for error reporting.

## Parser Implementation Overview

Tinct uses a **hand-written iterative descent parser** composed of two phases:

1. **Tokenization** (`src/lexer.rs`): Converts raw input into a flat token stream with accurate source spans. The lexer handles whitespace-sensitive disambiguation (e.g., `word@annotation` vs `word @annotation`) by tracking whitespace gaps and emitting context-aware tokens (`ImmediateAt`, `Dot`). Note: `[` is not whitespace-sensitive — bracket access was removed in access-pipeline-phase2; `a[0]` now parses as two separate expressions.

2. **Parsing** (`src/parser.rs`): Consumes the token stream using an explicit `Vec<StackFrame>` to avoid Rust call-stack recursion. The iterative parser enforces a maximum nesting depth (`MAX_PARSE_DEPTH = 256`) before allocating stack frames, preventing unbounded memory use.

**Historical note:** Tinct originally used a pest PEG grammar. The hand-written parser (commit cc8333c) replaced pest to gain precise control over error messages, whitespace sensitivity, and stack depth limits.

## AST Node Types

### Top-Level Types

```rust
/// A complete tinct file — one or more documents separated by ---
struct File {
    documents: Vec<Spanned<Document>>,
}

/// A document — one or more expressions forming a scope chain,
/// optionally carrying section-header metadata from the preceding `---` line.
struct Document {
    expressions: Vec<Rc<Spanned<Expr>>>,
    /// Section name from `--- %name`, e.g. `"config"` (bare name, no `%` sigil).
    /// `None` for anonymous documents.
    name: Option<String>,
    /// Output type annotation from `--- %name@Type` or `--- @Type`.
    /// Stored as a parsed `Annotation` with its source span.
    output_type: Option<Spanned<Annotation>>,
    /// Input contract from `--- expects: Type`.
    /// The type checker emits an advisory error if the incoming `%` type does not match.
    expects: Option<Spanned<Annotation>>,
}
```

The three optional fields — `name`, `output_type`, `expects` — are populated by the section-header parser when a `---` line carries metadata (e.g. `--- %config@Config expects: InputType`). All three are `None` for anonymous documents (bare `---` or the implicit first document). The `name` string is the bare section name without the `%` sigil (`"config"`, not `"%config"`). Callers (the evaluator and type checker) add the `%` prefix when binding the value into scope (e.g. `format!("%{}", name)`). `output_type` and `expects` store the full source span of the annotation so the type checker can point to the right source location in advisory error messages.

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
    //
    // `VarRef(name)` shorthand used in doc examples represents:
    //   `VarRef { name, escaped: false, resolved: RefCell::new(None) }`
    // The `resolved` field is a three-state sentinel populated by the variable
    // resolution pass (src/resolve.rs):
    //   - Outer None              = not yet processed (initial state)
    //   - Outer Some(None)        = processed, unresolvable
    //   - Outer Some(Some((l,s))) = resolved to (level, slot) de Bruijn coordinates
    VarRef {
        name: String,
        // True if written as `$name`, false if written as bare `name`.
        // Used by expr_to_pattern (src/parser.rs) to distinguish pin patterns ($x)
        // from bind patterns (x).
        escaped: bool,
        resolved: RefCell<Option<Option<(u32, u32)>>>,
    },
    DotAccess {
        expr: Box<Spanned<Expr>>,
        field: DotKey,  // DotKey::Ident("name") or DotKey::Int(0)
    },
    // Note: BracketAccess and RangeAccess were removed in access-pipeline-phase2.
    // Use [get key data] (builtin) for dynamic key access, and slice/drop/take for ranges.

    // Data
    Dict(Vec<Spanned<Entry>>),

    // Special forms
    Call {
        func: Box<Spanned<Expr>>,
        args: Vec<Rc<Spanned<Expr>>>,
        named_args: Vec<Spanned<NamedArg>>,
        // True if the call was written in implied form `[f x]` (no `call` keyword);
        // false if written in explicit form `[call f x]`.
        // Used by the formatter for roundtrip fidelity: implied calls are printed without
        // `call`, explicit calls are printed with `call`.
        implied: bool,
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
    TypeAlias {
        params: Vec<String>,
        body: Box<Spanned<Expr>>,
    },

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
    name: String,  // Always a bare identifier. `$key: val` syntax is rejected in call forms (only dict entries accept $-prefixed keys; see §Named Argument Key Normalization below).
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
| `VarRef { name: "x", .. }` | `x` or `$x` | Variable reference (bare identifier or escaped); `resolved` cache populated by the variable resolution pass |
| `DotAccess { field: DotKey::Ident("b"), .. }` | `a.b` | String key access: looks up `Key::String("b")` on `a` |
| `DotAccess { field: DotKey::Int(0), .. }` | `a.0` | Integer key access: looks up `Key::Int(0)` on `a` (auto-indexed dicts) |
| `Pipe { lhs, rhs }` | `a \| f` | **Pipe is present in the post-parse AST and eliminated by the desugar pass (`src/desugar.rs`) before type checking and evaluation. The evaluator and type checker never see `Expr::Pipe`.** See §Pipe Desugaring below for the three desugar rules (WRAP-PIPE, CALL-EXTEND, CALL-WRAP). |
| `Sequential(exprs)` | Multi-expression fn body | Sequential expressions with let\* semantics; each expression's result dict extends environment for subsequent expressions |
| `Dict(entries)` | `["a" "b" "c"]` or `[k: v]` | Dict/list literal |
| `Call` | `[f x]` or `[call f x]` | Function application (implied or explicit) |
| `Fn` | `[fn [x] body]` | Function definition |
| `TypeAlias` | `[type expr]` | Type alias declaration |
| `TypeAssert` | `[@T expr]` | Type assertion |
| `Annotated` | `Fn@Number` | Annotated bare word |
| `Rest(None)` | `...` | Open record marker |
| `Rest(Some("r"))` | `...r` | Named row variable |
| `Quote(expr)` | `[quote expr]` | Quote special form — prevents evaluation of expr |
| `Unquote(expr)` | `[unquote expr]` | Unquote inside quote — evaluates expr and splices result into quoted AST |
| `UnquoteSplice(expr)` | `[unquote-splice expr]` | Unquote-splice inside quote — evaluates expr (must be list) and splices each element into enclosing list |
| `DefMacro { name, transformer }` | `[defmacro name fn]` | Macro definition — registers compile-time transformer function |
| `Match { scrutinee, arms }` | `[match val pat1 body1 ...]` | Pattern matching with arms (pattern, optional guard, body) |
| `ClassDecl { name, params, superclasses, methods }` | `[class [Name a] super... methods...]` | Type class declaration with type parameters and method signatures |
| `InstanceDecl { class_name, instance_type, methods }` | `[instance [Name Type] methods...]` | Type class instance with method implementations |

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

**Pre-typecheck/eval transformations:** All desugaring rules in this section are applied before type checking and evaluation. The desugar pass (`src/desugar.rs`) runs immediately after parsing, producing a transformed AST that the type checker and evaluator consume. The type checker and evaluator never see the original sugared forms (e.g., `Expr::Pipe` is always desugared to `Expr::Call` before type checking begins).

### Access Chains

Dot notation and bracket notation desugar to nested access nodes:

| Surface syntax | AST |
|---------------|-----|
| `data.name` | `DotAccess { field: DotKey::Ident("name"), .. }` |
| `data.0` | `DotAccess { field: DotKey::Int(0), .. }` — integer key; looks up `Key::Int(0)` at eval time |
| `[get 5 data]` | `Call(VarRef("get"), [Int(5), VarRef("data")])` — use `get` builtin for dynamic key access (bracket access removed in access-pipeline-phase2) |
| `a.b.0.c` | `DotAccess(DotAccess(DotAccess(VarRef("a"), Ident("b")), Int(0)), Ident("c"))` |

### Pipe Desugaring

`|` is a desugar-only infix operator. The parser emits `Expr::Pipe { lhs, rhs }` and the desugar pass (`src/desugar.rs`) immediately rewrites it to `Expr::Call` before the resolution and evaluation passes run. The evaluator never sees `Expr::Pipe`; the `Expr::Pipe` arm in `src/resolve.rs` is an `unreachable!` guard.

Three desugar rules, applied in priority order:

| Surface syntax | Desugar rule | Call form |
|---------------|-------------|-----------|
| `$_ \| f` | WRAP-PIPE: `lhs` is `$_` (implicit arg) | `[fn [_] [f _]]` — wraps the pipeline in a lambda |
| `a \| [f ...]` | CALL-EXTEND: `rhs` is an explicit `Call` | `[f ... a]` — prepends `lhs` as first arg |
| `a \| f` | CALL-WRAP: `rhs` is anything else (VarRef, DotAccess, …) | `[f a]` — wraps `lhs` as the single arg |

Left-associativity: `a | f | g` parses as `(a | f) | g`, which desugars to `[g [f a]]`.

**Note:** Pipe is eliminated in the desugar pass before type checking and variable resolution run. Any tooling pass that may see `Expr::Pipe` must either run before desugar or handle it explicitly as unreachable.

---

## AST Dict Schema

`ast_to_dict` (`src/ast_dict.rs`) serializes the `Expr` AST to tinct dicts. `dict_to_ast` converts dicts back to `Expr`. These two functions are the shared primitive for quasiquoting (`[quote]`), macros (`[defmacro]`), and the tinct-hosted formatter. The canonical schema is defined in `doc/whatif/ast-schema.md`.

### Conventions

- **`type:` string discriminator** on every node — `[type: "call" ...]`, `[type: "var" ...]`
- **`[]` for absent optionals** — never omit the key (except comment fields, which are absent when empty)
- **`span:` on every node** — `[start: [line: 1 col: 5 offset: 4] end: [line: 1 col: 12 offset: 11]]`. "Every node" means every `Spanned<T>` wrapper, not every sub-element; `DotKey` has no independent span
- **`schema-version: 1`** on the root `File` node — bump on breaking changes

### Modes

`ast_to_dict` accepts an `AstToDictOpts` struct controlling output:

| Field | When set | Effect |
|-------|----------|--------|
| `source: None` | Compact mode, quasiquoting | `bare:` is always `false` on string literals |
| `source: Some(src)` | Full formatter | `bare: true` when source char at token start ≠ `"` |
| `comments: None` | Compact mode, quasiquoting | No comment fields emitted |
| `comments: Some(map)` | Full formatter | `leading-comments:`, `trailing-comment:`, `blank-before:` on entries and documents |

### dict_to_ast

`dict_to_ast(v: &Value) -> Result<Expr, AstError>` validates and reconstructs an `Expr`:
- `type:` must be a known string
- Required fields must be present and of the correct shape
- `span:` is optional — absent nodes get a synthetic zero span
- Unknown fields are ignored (forward-compatible)

Synthetic nodes (from `dict_to_ast` with absent `span:`) receive a `SyntheticId(u64)` for tracking in the macro expansion blackhole detection set.

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
