# AST & Parser Internals

Every grammar rule maps to an AST node. All nodes carry source span information for error reporting.

## Parser Implementation Overview

Tinct uses a **hand-written iterative descent parser** composed of two phases:

1. **Tokenization** (`src/lexer.rs`): Converts raw input into a flat token stream with accurate source spans. The lexer handles whitespace-sensitive disambiguation (e.g., `word@annotation` vs `word @annotation`) by tracking whitespace gaps and emitting context-aware tokens (`ImmediateAt`, `Dot`). Note: `[` is not whitespace-sensitive — `a[0]` parses as two separate expressions.

2. **Parsing** (`src/parser.rs`): Consumes the token stream using an explicit `Vec<StackFrame>` to avoid Rust call-stack recursion. The iterative parser enforces a maximum nesting depth (`MAX_PARSE_DEPTH = 256`) before allocating stack frames, preventing unbounded memory use.

The hand-written parser provides precise control over error messages, whitespace sensitivity, and stack depth limits.

## AST Node Types

### Top-Level Types (Surface AST)

The parser natively constructs a **Surface AST** composed of these key types:

```rust
/// Top-level container for a complete parsed file
struct SurfaceProgram {
    documents: Vec<Spanned<SurfaceDocument>>,
}

/// A document — one or more items (expressions or declarations) forming a scope chain,
/// optionally carrying section-header metadata from the preceding `---` line.
struct SurfaceDocument {
    items: Vec<SurfaceItem>,
    /// Section name from `--- %name`, e.g. `"config"` (bare name, no `%` sigil).
    /// `None` for anonymous documents.
    name: Option<String>,
    /// Output type annotation from `--- %name@Type` or `--- @Type`.
    output_type: Option<Spanned<Annotation>>,
    /// Input contract from `--- expects: Type`.
    expects: Option<Spanned<Annotation>>,
    /// Capability declarations from `--- caps: [...]`.
    caps: Option<Spanned<Vec<(String, Annotation)>>>,
    /// Document stage from `--- stage: type` (defaults to `Stage::Runtime` if `None`).
    stage: Option<Stage>,
}

/// Document stage — determines how the document is evaluated
enum Stage {
    Runtime,  // Default: evaluated in the main pipeline
    Type,     // Type-stage: evaluated for type aliases and class declarations
}

/// Document items: either expressions or declarations
enum SurfaceItem {
    Expr(Arc<SurfaceNode>),
    Decl(Spanned<SurfaceDeclaration>),
}

/// Wrapper node with source span (Arc for shared ownership)
struct SurfaceNode {
    expr: SurfaceExpression,
    span: Span,
}

/// Expression enum — literals, references, calls, functions, etc.
enum SurfaceExpression {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    VarRef { name: String, escaped: bool },
    Dict(Vec<SurfaceEntry>),
    Call { func: Arc<SurfaceNode>, args: Vec<Arc<SurfaceNode>>, named_args: Vec<SurfaceNamedArg>, implied: bool },
    Fn { return_ann: Option<Spanned<Annotation>>, params: Vec<Spanned<SurfaceParam>>, body: Arc<SurfaceNode>, desugared: bool },
    DotAccess { expr: Arc<SurfaceNode>, field: DotKey },
    Pipe { lhs: Arc<SurfaceNode>, rhs: Arc<SurfaceNode> },
    // ... other variants (Sequential, Match, TypeAssert, Quote, Unquote, etc.)
}

/// Declarations (TypeAlias, ClassDecl, InstanceDecl, DefMacro, etc.)
enum SurfaceDeclaration {
    TypeAlias { params: Vec<String>, body: Arc<SurfaceNode> },
    ClassDecl { /* ... */ },
    InstanceDecl { /* ... */ },
    DefMacro { /* ... */ },
    // ... other variants
}
```

The Surface AST is the parser's native output and the authoritative representation before lowering. Key characteristics:

- **Arc-wrapped nodes:** `Arc<SurfaceNode>` enables shared ownership across multiple references without copying the AST.
- **Side-table design:** Variable resolution (`ResolutionTable`) and type annotations (`TypeAnnotationTable`) are stored separately, keyed by `NodeId` (derived from `Arc` raw pointer). This replaces the old `RefCell<Option<...>>` in-node mutation pattern.
- **Pipe is preserved:** `SurfaceExpression::Pipe` remains in the Surface AST. The **lowering pass** (`src/lower.rs`) eliminates Pipe by rewriting it to `Call` before evaluation (see §Pipe Desugaring below).
- **Expr/Decl separation:** Documents contain `SurfaceItem` entries, which are either expressions (`SurfaceItem::Expr`) or declarations (`SurfaceItem::Decl`). This separates top-level declarations (TypeAlias, ClassDecl, etc.) from expressions at the type level.
- **ParseOutput returns SurfaceProgram:** `parse()` returns `ParseOutput { program: SurfaceProgram, ... }`. The `.program` field is the native Surface AST.

The `parse()` function returns `Result<ParseOutput, ParseError>`.

The `parse_expression(input)` function is a test and convenience helper that parses the input and returns the first expression of the first document. Multi-expression inputs discard all but the first expression; multi-document inputs discard all but the first document (`---`-separated multi-doc input returns only the first document). No scope chain is built — bindings from earlier expressions are not preserved. This is parse-level convenience, not an evaluator.

### Core AST (Evaluation)

After the Surface AST is resolved and type-checked, the **lowering pass** (`src/lower.rs`) transforms `SurfaceNode` into `CoreExpr` for evaluation. The Core AST is simpler than the Surface AST:

- **Pipe eliminated:** `SurfaceExpression::Pipe` is rewritten to nested `Call` expressions.
- **Resolution baked in:** Variable references are resolved to de Bruijn indices or environment lookups.
- **Type assertions removed:** Type checking happens before lowering; runtime evaluation uses `CoreExpr` without type information.

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

/// The core expression type (post-lowering, used by evaluator)
pub enum CoreExpr {
    // Literals
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),

    // VarRef with resolved de Bruijn coordinates
    Var {
        name: String,
        level: u32,
        slot: u32,
    },
    // Unresolvable ref (include-introduced bindings) — name-based env lookup at runtime
    FreeVar(String),

    DotAccess {
        expr: Arc<Spanned<CoreExpr>>,
        field: DotKey,
    },

    // No Pipe variant — the lowering pass rewrites Pipe to Call before evaluation.
    Sequential(Vec<Arc<Spanned<CoreExpr>>>),
    Dict(Vec<Spanned<CoreEntry>>),
    Call {
        func: Arc<Spanned<CoreExpr>>,
        args: Vec<Arc<Spanned<CoreExpr>>>,
        named_args: Vec<Spanned<CoreNamedArg>>,
        implied: bool,
    },
    Fn {
        return_ann: Option<Spanned<Annotation>>,
        params: Vec<Spanned<CoreParam>>,
        body: Arc<Spanned<CoreExpr>>,
        desugared: bool,
    },
    // Statically type-checked TypeAssert — resolved_type set from TypeAnnotationTable during lowering.
    // Runtime behavior: structural check against resolved_type at force time.
    TypeAssert {
        annotation: Spanned<Annotation>,
        expr: Arc<Spanned<CoreExpr>>,
        resolved_type: Type,
    },
    // TypeAssert for nodes absent from TypeAnnotationTable (macro-synthesized, bypassed typechecking).
    // Falls back to default if present, raises error otherwise.
    RuntimeTypeCheck {
        annotation: Spanned<Annotation>,
        expr: Arc<Spanned<CoreExpr>>,
        default: Option<Arc<Spanned<CoreExpr>>>,
    },
    Annotated {
        name: String,
        annotation: Spanned<Annotation>,
    },
    Rest(Option<String>),
    Match {
        scrutinee: Arc<Spanned<CoreExpr>>,
        arms: Vec<CoreMatchArm>,
    },
    Quote(Arc<Spanned<CoreExpr>>),
    Unquote(Arc<Spanned<CoreExpr>>),
    UnquoteSplice(Arc<Spanned<CoreExpr>>),
    PatternDecl {
        bindings: Vec<Spanned<CoreExpr>>,
    },
    LetDecl {
        bindings: Vec<Spanned<CoreExpr>>,
    },
    CaseArm {
        pattern: Arc<Spanned<CoreExpr>>,
        body: Arc<Spanned<CoreExpr>>,
    },
    TypeApp {
        func: Arc<Spanned<CoreExpr>>,
        arg: Arc<Spanned<CoreExpr>>,
    },
    Placeholder,
    Error(Span),
}
```

### Supporting Types

```rust
/// A dict/list entry in a CoreExpr::Dict.
pub struct CoreEntry {
    pub key: Option<Arc<Spanned<CoreExpr>>>,
    pub value: Arc<Spanned<CoreExpr>>,
}

/// A named argument in a CoreExpr::Call.
pub struct CoreNamedArg {
    pub name: String,
    pub value: Arc<Spanned<CoreExpr>>,
}

/// A function parameter in a CoreExpr::Fn.
pub struct CoreParam {
    pub name: String,
    pub annotation: Option<Spanned<Annotation>>,
    pub variadic: bool,
}

/// A match arm in a CoreExpr::Match.
pub struct CoreMatchArm {
    pub pattern: Spanned<Pattern>,
    pub guard: Option<Arc<Spanned<CoreExpr>>>,
    pub body: Arc<Spanned<CoreExpr>>,
}

/// An annotation (type or property dict)
enum Annotation {
    Simple(String),               // x@Number — shorthand
    PropertyDict(Vec<Spanned<SurfaceEntry>>),  // x@[type: Number  default: 30]
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
| `VarRef { name: "x", .. }` | `x` or `$x` | Variable reference (bare identifier or escaped); resolution results live in `ResolutionTable` keyed by `NodeId` |
| `DotAccess { field: DotKey::Ident("b"), .. }` | `a.b` | String key access: looks up `Key::String("b")` on `a` |
| `DotAccess { field: DotKey::Int(0), .. }` | `a.0` | Integer key access: looks up `Key::Int(0)` on `a` (auto-indexed dicts) |
| `Pipe { lhs, rhs }` | `a \| f` | **Pipe is present in the Surface AST and eliminated by the lowering pass (`src/lower.rs`) before evaluation. The evaluator never sees `SurfaceExpression::Pipe`.** Lowering rewrites `Pipe { lhs, rhs }` to `CoreExpr::Call { func: rhs, args: [lhs], ... }` (equivalent to `f(a)` for `a \| f`). The type checker operates on the Surface AST and handles Pipe directly. |
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

**LetDecl** — `SurfaceExpression::LetDecl { bindings: Vec<Arc<SurfaceNode>> }` — is a binding declaration list introduced by the `let` keyword. Each binding is one of:

- `VarRef { name, escaped: false, .. }` — bare identifier binding (e.g., `x`)
- `Annotated { name, annotation }` — typed binding (e.g., `x@Int`) or structural test (e.g., `v: Ok`)
- `Placeholder` (represented as `_`) — wildcard match, introduces no binding
- Nested `LetDecl` — multi-level pattern for constructor payloads (e.g., `[let [let inner]]`)

LetDecl appears in:

- Function parameter lists: `[fn [let x@Int y] body]`
- Case arm patterns: `[case [let x@Int] body]`
- Type class declarations: `[class [let Equatable a] ...]` (TypeVar binding list)
- Instance patterns: `[instance Class [pattern [let a@Int b@Float]] ...]`

**CaseArm** — `SurfaceExpression::CaseArm { pattern, body }` — is a match arm with explicit scoping introduced by the `case` keyword. The pattern can be:

- `LetDecl` — binding pattern that introduces variables into the body's scope (e.g., `[case [let x@Int] body]`)
- Any other expression — exact-value match (e.g., `[case 42 body]`, `[case "hello" body]`)

CaseArm is used inside `[match ...]` expressions. The `match` evaluator tries each arm's pattern in order. For LetDecl patterns, the type checker validates that the binding constraints are satisfiable and binds the matched value. For exact-value patterns, the evaluator compares the scrutinee for equality.

**Placeholder** — `SurfaceExpression::Placeholder` — represents the `...` token when used as an expression (not as a Rest marker in type contexts). The type checker assigns it type `Unknown`, meaning it satisfies any constraint without producing a type error. At evaluation time, forcing a Placeholder thunk raises an `UnimplementedError` with the message `"placeholder \`...\` was evaluated — replace with an implementation"`. This allows developers to write incomplete code that type-checks but defers implementation details.

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
```

When a named argument is provided for a positional parameter, the type checker raises an error.

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

The formatter always emits `[fn [let x y] body]`.

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
| `fn` | Special form — produces `SurfaceExpression::Fn`, not `SurfaceExpression::Dict` |
| `type` | Special form — produces `SurfaceDeclaration::TypeAlias`, not `SurfaceExpression::Dict` |
| `type_assert_body` (`[@Annotation expr]`) | Produces `SurfaceExpression::TypeAssert`, not `SurfaceExpression::Dict` — rejected even though it is not a named special form keyword |

All four are caught by the same check in `parse_annotation`: after re-parsing the bracket sub-string, the result is classified as follows:

- `SurfaceExpression::Dict` → accepted as a property dict annotation (named entries, e.g. `[type: Number  default: 30]`).
- `SurfaceExpression::Call { implied: true, func: VarRef(..), .. }` → accepted as a positional union type annotation: the func and each arg become auto-indexed `Entry` values in a `PropertyDict`. This handles `fn@[Int Null]` (parameterized) and `fn@[a Null]` (type variable). Both uppercase and lowercase VarRef heads are accepted.
- Anything else (explicit `call` form with `implied: false`, `SurfaceExpression::Fn`, `SurfaceDeclaration::TypeAlias`, `SurfaceExpression::TypeAssert`) → parse error: "property dict annotation must be a dict expression".

`type_assert_body` is rejected on the "anything else" basis, not because of keyword disambiguation.

When no `type:` key is present, the bracket is interpreted as a type expression (record type), and rest entries are allowed for row polymorphism.

---

## Desugaring Rules

**Pre-typecheck/eval transformations:** Most desugaring rules in this section are applied in the desugar pass (`src/desugar.rs`) which runs immediately after parsing. **Exception:** Pipe (`|`) is NOT eliminated by the desugar pass — it remains in the Surface AST and is handled by the type checker. Pipe is eliminated later by the **lowering pass** (`src/lower.rs`) after type checking but before evaluation.

### Access Chains

Dot notation and bracket notation desugar to nested access nodes:

| Surface syntax | AST |
|---------------|-----|
| `data.name` | `DotAccess { field: DotKey::Ident("name"), .. }` |
| `data.0` | `DotAccess { field: DotKey::Int(0), .. }` — integer key; looks up `Key::Int(0)` at eval time |
| `[get 5 data]` | `Call(VarRef("get"), [Int(5), VarRef("data")])` — use `get` builtin for dynamic key access |
| `a.b.0.c` | `DotAccess(DotAccess(DotAccess(VarRef("a"), Ident("b")), Int(0)), Ident("c"))` |

### Pipe Lowering

`|` is preserved in the Surface AST and eliminated by the **lowering pass** (`src/lower.rs`). The parser emits `SurfaceExpression::Pipe { lhs, rhs }`, which remains intact through desugar, resolve, and typecheck. The lowering pass (which runs after type checking but before evaluation) rewrites Pipe to `CoreExpr::Call` before the evaluator runs. The evaluator never sees Pipe.

Three lowering rules, applied in priority order:

| Surface syntax | Lowering rule | Call form |
|---------------|--------------|-----------|
| `$_ \| f` | WRAP-PIPE: `lhs` is `$_` (implicit arg) | `[fn [_] [f _]]` — wraps the pipeline in a lambda |
| `a \| [f ...]` | CALL-EXTEND: `rhs` is an explicit `Call` | `[f ... a]` — prepends `lhs` as first arg |
| `a \| f` | CALL-WRAP: `rhs` is anything else (VarRef, DotAccess, …) | `[f a]` — wraps `lhs` as the single arg |

Left-associativity: `a | f | g` parses as `(a | f) | g`, which lowers to `[g [f a]]`.

**Note:** Pipe is eliminated in the lowering pass AFTER type checking. The type checker must handle `SurfaceExpression::Pipe` explicitly.

---

## AST Dict Schema

`surface_node_to_dict` (`src/ast_dict.rs`) serializes the Surface AST to tinct dicts. `dict_to_surface_node` converts dicts back to `Arc<SurfaceNode>`. These two functions are the shared primitive for quasiquoting (`[quote]`), macros (`[defmacro]`), and the tinct-hosted formatter. The canonical schema is defined in `doc/feature/ast-schema.md`.

### Conventions

- **`Value::Variant` tag on Expr nodes** — `Variant("Call", {fn: ..., args: ...})`, `Variant("VarRef", {name: "x", span: ...})`. Tags are PascalCase. Structural nodes (SurfaceEntry, Annotation, Pattern, SurfaceDocument, SurfaceProgram) remain plain dicts with a `type:` string discriminator
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

### dict_to_surface_node

`dict_to_surface_node(v: &Value, ctx: &DictToAstContext) -> Result<Arc<SurfaceNode>, AstError>` validates and reconstructs a `SurfaceNode`:

- Accepts both `Value::Variant` (new format from `surface_node_to_dict`) and legacy plain dicts with a `type:` string discriminator (backward compat)
- Required fields must be present and of the correct shape
- `span:` is optional — absent nodes get a synthetic zero span
- Unknown fields are ignored (forward-compatible)

Synthetic nodes (from `dict_to_surface_node` with absent `span:`) receive a `SyntheticId(u64)` for tracking in the macro expansion blackhole detection set.

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
