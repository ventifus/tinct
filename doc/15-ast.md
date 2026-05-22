# AST & Parser Internals

Every grammar rule maps to an AST node. All nodes carry source span information for error reporting.

## Parser Implementation Overview

Tinct uses a **hand-written iterative descent parser** composed of two phases:

1. **Tokenization** (`src/lexer.rs`): Converts raw input into a flat token stream with accurate source spans. The lexer handles whitespace-sensitive disambiguation (e.g., `word@annotation` vs `word @annotation`) by tracking whitespace gaps and emitting context-aware tokens (`ImmediateAt`, `Dot`). Note: `[` is not whitespace-sensitive — `a[0]` parses as two separate expressions.

2. **Parsing** (`src/parser.rs`): Consumes the token stream using an explicit `Vec<StackFrame>` to avoid Rust call-stack recursion. The iterative parser enforces a maximum nesting depth (`MAX_PARSE_DEPTH = 256`) before allocating stack frames, preventing unbounded memory use.

The hand-written parser provides precise control over error messages, whitespace sensitivity, and stack depth limits.

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
    /// Capability declarations from `--- caps: [...]`.
    caps: Option<Spanned<Vec<(String, Annotation)>>>,
    /// Document stage from `--- stage: type`.
    /// Determines how the document is evaluated. Defaults to `Stage::Runtime` if `None`.
    stage: Option<Stage>,
}

/// Document stage — determines how the document is evaluated
enum Stage {
    Runtime,  // Default: evaluated in the main pipeline
    Type,     // Type-stage: evaluated for type aliases and class declarations
}
```

The optional fields — `name`, `output_type`, `expects`, `caps`, `stage` — are populated by the section-header parser when a `---` line carries metadata (e.g. `--- %config@Config expects: InputType caps: [%nc: @NetCap] stage: type`). All are `None` for anonymous documents (bare `---` or the implicit first document). The `name` string is the bare section name without the `%` sigil (`"config"`, not `"%config"`). Callers (the evaluator and type checker) add the `%` prefix when binding the value into scope (e.g. `format!("%{}", name)`). `output_type` and `expects` store the full source span of the annotation so the type checker can point to the right source location in advisory error messages.

The `stage` field controls how the document is evaluated. `Stage::Type` documents are evaluated during type inference to populate the type-stage environment with type aliases and class declarations. `Stage::Runtime` documents (the default when `stage` is `None`) are evaluated in the main pipeline. The parser accepts `--- stage: type` as a section-header pragma; `type` is the only valid stage name. See `doc/09-documents.md` for section-header pragma syntax.

The `parse()` function returns `Result<ParseOutput, ParseError>`.

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
        body: Rc<Spanned<Expr>>,
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
    value: Rc<Spanned<Expr>>,
}

/// A named argument in a call expression
struct NamedArg {
    name: String,  // Always a bare identifier. `$key: val` syntax is rejected in call forms (only dict entries accept $-prefixed keys; see §Named Argument Key Normalization below).
    value: Rc<Spanned<Expr>>,
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
    Annotated(String, Box<Annotation>),  // Seq@Int — chained annotation
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
| `Fn` | `[fn [let x y] body]` | Function definition with binding list parameters |
| `TypeAlias` | `[type expr]` | Type alias declaration |
| `TypeAssert` | `[@T expr]` | Type assertion |
| `Annotated` | `Fn@Number` | Annotated bare word |
| `LetDecl { bindings }` | `[let x@Int y]` | Binding declaration list for fn params, case arms, and pattern contexts |
| `CaseArm { pattern, body }` | `[case [let x] body]` | Match arm with explicit scoping — pattern can be LetDecl (binding) or exact-value match |
| `Placeholder` | `...` | Placeholder expression — type Unknown, evaluates to lazy error on materialization |
| `Rest(None)` | `...` | Open record marker in type expressions |
| `Rest(Some("r"))` | `...r` | Named row variable |
| `Quote(expr)` | `[quote expr]` | Quote special form — prevents evaluation of expr |
| `Unquote(expr)` | `[unquote expr]` | Unquote inside quote — evaluates expr and splices result into quoted AST |
| `UnquoteSplice(expr)` | `[unquote-splice expr]` | Unquote-splice inside quote — evaluates expr (must be list) and splices each element into enclosing list |
| `DefMacro { name, params, body }` | `[defmacro name [params] body...]` | Macro definition — registers compile-time transformer function |
| `MacroDecl { name, params, body }` | `[macro name [let ...] body]` | Macro special form — compile-time syntax transformer, produced by `macro` keyword |
| `SyntaxClass { name, pattern, message }` | `[syntax-class name pattern: [...] message: "..."]` | Syntax class declaration — names a set of syntactic patterns with a diagnostic message |
| `Splice(Vec<Spanned<Expr>>)` | (internal) | Macro-expansion-internal splice — not a parser keyword; produced during macro expansion, not by direct source parsing |
| `Match { scrutinee, arms }` | `[match val pat1: body1 ...]` | Pattern matching with arms (pattern, optional guard, body) |
| `ClassDecl { name, params, superclasses, methods, determines, resolver, resolver_injective }` | `[class [Name a] super... methods...]` | Type class declaration with type parameters, method signatures, optional functional dependencies (`determines`), optional resolver function (`resolver`), and `resolver_injective: bool` flag for CHR constraint head uniqueness |
| `InstanceDecl { class_name, arms }` | `[instance ClassName [pattern [...]]: methods...]` | Type class instance with match-arm syntax; each arm pairs a `PatternDecl` expression with method entries |
| `PatternDecl { bindings }` | `[pattern [a@Int b@Float]]` | Pattern declaration for instance match arms; bindings are typically `Annotated` nodes |

#### LetDecl, CaseArm, and Placeholder Details

**LetDecl** — `Expr::LetDecl { bindings: Vec<Spanned<Expr>> }` — is a binding declaration list introduced by the `let` keyword. Each binding is one of:

- `VarRef { name, escaped: false, .. }` — bare identifier binding (e.g., `x`)
- `Annotated { name, annotation }` — typed binding (e.g., `x@Int`) or structural test (e.g., `v: Ok`)
- `Placeholder` (represented as `_`) — wildcard match, introduces no binding
- Nested `LetDecl` — multi-level pattern for constructor payloads (e.g., `[let [let inner]]`)

LetDecl appears in:

- Function parameter lists: `[fn [let x@Int y] body]`
- Case arm patterns: `[case [let x@Int] body]`
- Type class declarations: `[class [let Equatable a] ...]` (TypeVar binding list)
- Instance patterns: `[instance Class [pattern [let a@Int b@Float]] ...]`

**CaseArm** — `Expr::CaseArm { pattern, body }` — is a match arm with explicit scoping introduced by the `case` keyword. The pattern can be:

- `LetDecl` — binding pattern that introduces variables into the body's scope (e.g., `[case [let x@Int] body]`)
- Any other expression — exact-value match (e.g., `[case 42 body]`, `[case "hello" body]`)

CaseArm is used inside `[match ...]` expressions. The `match` evaluator tries each arm's pattern in order. For LetDecl patterns, the type checker validates that the binding constraints are satisfiable and binds the matched value. For exact-value patterns, the evaluator compares the scrutinee for equality.

**Placeholder** — `Expr::Placeholder` — represents the `...` token when used as an expression (not as a Rest marker in type contexts). The type checker assigns it type `Unknown`, meaning it satisfies any constraint without producing a type error. At evaluation time, forcing a Placeholder thunk raises an `UnimplementedError` with the message `"placeholder \`...\` was evaluated — replace with an implementation"`. This allows developers to write incomplete code that type-checks but defers implementation details.

Example use in a try block:

```tinct
[try [fn [] ...]]
=== out
{"Err":"placeholder `...` was evaluated — replace with an implementation"}
```

---

## Static Constraints

These constraints are enforced by the parser. Violations produce parse errors with source locations.

### Mixed Positional and Named Ordering

Positional (auto-indexed) and keyed (named) entries may appear in any order within `[]`, for both dict entries and call arguments. Auto-indices are assigned sequentially to positional entries regardless of interleaving:

```tinct
[$a $b key: val]        # valid: 0: ref(a), 1: ref(b), key: val
[key: val $a $b]        # valid: key: val, 0: ref(a), 1: ref(b)
[$a key: val $b]        # valid: 0: ref(a), key: val, 1: ref(b)
=== error
type errors:
  undefined variable: a at 1:2-1:4
  undefined variable: b at 1:5-1:7
  undefined variable: val at 1:13-1:16
  undefined variable: val at 2:7-2:10
  undefined variable: a at 2:11-2:13
  undefined variable: b at 2:14-2:16
  undefined variable: a at 3:2-3:4
  undefined variable: val at 3:10-3:13
  undefined variable: b at 3:14-3:16

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
=== error
error: duplicate key "name"
 --> block 3:1:17
  |
  1 | [name: "Alice"  name: "Bob"]          # ERROR: duplicate key "name"
    |                 ^^^^
```

Duplicate detection applies to explicit keys only. Auto-indexed entries cannot duplicate because the counter always increments.

**Note:** The parser detects duplicates among literal keys and VarRef keys at parse time. VarRef keys are compared by variable name -- `[$k: a  $k: b]` is a parse error because `$k` appears twice as a key, regardless of what value `$k` might resolve to at runtime. Bracket expression keys (`[[expr]: value]`) bypass the parse-time duplicate check; the evaluator performs runtime duplicate detection to catch computed keys (e.g., `[$k1: a  $k2: b]` where `$k1` and `$k2` resolve to the same value, or `[[call $f]: a  [call $g]: b]` where both calls produce the same key). Both checks produce errors with source locations.

### `fn` Parameter List Structure

The parameter list in `fn` is a `[let ...]` binding declaration containing zero or more binding entries, optionally ending with one variadic parameter (`...name`). Parameters are bare identifiers or annotated bindings:

```tinct
[fn [let x y] body]                   # valid: two parameters
[fn [let x@Number y] body]            # valid: x has type annotation
[fn [let x ...rest] body]             # valid: variadic parameter
[fn [let ...a ...b] body]             # ERROR: multiple variadics
[fn [let ...rest x] body]             # ERROR: parameter after variadic
=== error
error: multiple variadic parameters
 --> block 4:4:15
  |
  4 | [fn [let ...a ...b] body]             # ERROR: multiple variadics
    |               ^^^
```

The older `[fn [x y] body]` syntax (bare parameter list without `let`) is still supported for backward compatibility but the formatter emits `[fn [let x y] body]`.

### Bracket Nesting Depth Limit

`MAX_LEX_DEPTH = 256` is enforced at the lexer level: when the 257th `[` is encountered the lexer immediately returns an error, so for pure bracket nesting the lexer check fires before the parser ever sees a `Token::OpenBracket`. The iterative parser (`Vec<StackFrame>` main loop) also bounds nesting depth by heap, not the native call stack. `MAX_PARSE_DEPTH` (256) is checked on `stack.len()` before each push, firing before any allocation — inputs exceeding this limit produce a clear parse error.

### Parser Output

`parse()` returns `Result<ParseOutput, ParseError>`. `ParseOutput { program: SurfaceProgram, source: String, leading_comments: BTreeMap<usize, Vec<String>>, trailing_comments: BTreeMap<usize, String>, blank_before: BTreeMap<usize, bool>, errors: Vec<ParseError> }` carries the AST plus comment side-tables for formatter support. The evaluator and type checker access `.program`; the formatter uses the comment maps directly.

### Annotation Bracket Restriction

Annotation bracket expressions (e.g., `x@[type: Number  default: 30]`) must contain only dict entries or union type members. Special forms within annotations are parse errors. When a `type:` key is present, rest entries (`...` or `...name`) are also forbidden — they have no defined semantics in property dict context:

```tinct
x@[type: Number  default: 30]    # valid: property dict
x@Number                         # valid: simple annotation
[@[name: String  ...] $val]      # valid: type expression with rest (no type: key)
fn@[Int Null]                    # valid: union return type (two positional entries)
fn@[a Null]                      # valid: union with type variable (lowercase VarRef head)
x@[call $f $x]                   # ERROR: explicit call special form in annotation bracket
x@[fn [a] $a]                    # ERROR: fn special form in annotation bracket
x@[type Number]                  # ERROR: type special form in annotation bracket
x@[@Number $val]                 # ERROR: type_assert_body in annotation bracket
x@[type: Int  ...]               # ERROR: rest entry alongside type: key
=== error
error: property dict annotation must be a dict expression, got: [call f x]
 --> block 5:6:3
  |
  6 | x@[call $f $x]                   # ERROR: explicit call special form in annotation bracket
    |   ^^^^^^^^^^^^
```

The following constructs are rejected inside annotation brackets:

| Rejected form | Why |
|--------------|-----|
| `call` | Explicit call special form — `implied: false`, produces a rejected non-Dict, non-implied-VarRef-call result |
| `fn` | Special form — produces `Expr::Fn`, not `Expr::Dict` |
| `type` | Special form — produces `Expr::TypeAlias`, not `Expr::Dict` |
| `type_assert_body` (`[@Annotation expr]`) | Produces `Expr::TypeAssert`, not `Expr::Dict` — rejected even though it is not a named special form keyword |

All four are caught by the same check in `parse_annotation`: after re-parsing the bracket sub-string, the result is classified as follows:

- `Expr::Dict` → accepted as a property dict annotation (named entries, e.g. `[type: Number  default: 30]`).
- `Expr::Call { implied: true, func: VarRef(..), .. }` → accepted as a positional union type annotation: the func and each arg become auto-indexed `Entry` values in a `PropertyDict`. This handles `fn@[Int Null]` (parameterized) and `fn@[a Null]` (type variable). Both uppercase and lowercase VarRef heads are accepted.
- Anything else (explicit `call` form with `implied: false`, `Expr::Fn`, `Expr::TypeAlias`, `Expr::TypeAssert`) → parse error: "property dict annotation must be a dict expression".

`type_assert_body` is rejected on the "anything else" basis, not because of keyword disambiguation.

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
| `[get 5 data]` | `Call(VarRef("get"), [Int(5), VarRef("data")])` — use `get` builtin for dynamic key access |
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

`ast_to_dict` (`src/ast_dict.rs`) serializes the `Expr` AST to tinct dicts. `dict_to_ast` converts dicts back to `Expr`. These two functions are the shared primitive for quasiquoting (`[quote]`), macros (`[defmacro]`), and the tinct-hosted formatter. The canonical schema is defined in `doc/feature/ast-schema.md`.

### Conventions

- **`Value::Variant` tag on Expr nodes** — `Variant("Call", {fn: ..., args: ...})`, `Variant("VarRef", {name: "x", span: ...})`. Tags are PascalCase. Structural nodes (Entry, Annotation, Pattern, Document, File) remain plain dicts with a `type:` string discriminator
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

- Accepts both `Value::Variant` (new format from `ast_to_dict`) and legacy plain dicts with a `type:` string discriminator (backward compat)
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

**Origin Tracking:** The `Expr::Fn.desugared: bool` field (see `src/ast.rs` `Fn` variant) tracks whether a lambda was user-written (`false`) or generated by `_` desugaring (`true`). This origin tagging follows Pombrio & Krishnamurthi (2014)'s approach to preserving provenance through AST transformations. Tooling can use this field to distinguish sugar-generated lambdas from explicit lambdas — useful for error messages, IDE navigation, and debugging. For example, a type error in a desugared lambda could point to the original `_` expression rather than the generated `[fn [_] ...]` form.
