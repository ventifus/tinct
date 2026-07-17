# Parser

The parser converts tinct source text into a `SurfaceProgram` — a tree of `Arc<SurfaceNode>` values, each carrying a source span. It is a hand-written iterative parser that uses an explicit bracket stack rather than Rust's call stack, making it safe for deeply nested inputs.

---

## Design

The lexer (`src/lexer.rs`) produces a flat token stream with accurate source spans. The parser (`src/parser.rs`) consumes that stream and builds the Surface AST.

Key properties:
- **Iterative, not recursive.** Bracket nesting is tracked with an explicit stack. No Rust stack growth per nesting level.
- **Error recovery inside brackets.** When a parse error occurs inside a bracket form, the parser records a `ParseError` in `ParseOutput.errors` and continues. The broken sub-expression becomes a `SurfaceExpression::Error` node in the AST. Fatal errors (at the top level, or when recovery is not possible) return `Err(ParseError)`.
- **Comment collection.** Comments are extracted separately into `leading_comments` and `trailing_comments` maps keyed by line number, for use by the formatter. They do not appear in the AST.

---

## Limits

| Limit | Value | Purpose |
|---|---|---|
| `MAX_PARSE_DEPTH` | 256 | Max bracket nesting depth; deeper inputs are rejected |
| `MAX_SOURCE_SIZE` | 100 MB | Max source text size; larger inputs are rejected |

---

## Entry Points

```rust
pub fn parse(input: &str) -> Result<ParseOutput, ParseError>
```

Parse a complete tinct source string using a synthetic `SourceFile`. Returns `Ok(ParseOutput)` on success (including partial recovery from bracket-internal errors) or `Err(ParseError)` on fatal parse failure.

```rust
pub fn parse_with_file(
    source: &str,
    file: Arc<SourceFile>,
) -> Result<ParseOutput, ParseError>
```

Same as `parse`, but associates the output spans with a provided `SourceFile` (for accurate file path and content in error messages and stack traces). Used by the main evaluation pipeline.

```rust
pub fn parse_surface_expression(input: &str) -> Result<Arc<SurfaceNode>, ParseError>
```

Parse a single tinct expression (not a full program). Used by tests and the REPL.

---

## Output Types

### `ParseOutput`

```rust
pub struct ParseOutput {
    pub program: SurfaceProgram,                       // primary output
    pub errors: Vec<ParseError>,                       // recovered errors inside bracket forms
    pub leading_comments: BTreeMap<usize, Vec<String>>, // line → comments before that line
    pub trailing_comments: BTreeMap<usize, String>,    // line → comment at end of that line
    pub blank_before: BTreeMap<usize, bool>,           // line → whether a blank line precedes it
}
```

`program` is the primary output. The other fields are for the formatter. `errors` is non-empty when the parser recovered from one or more bracket-internal errors; those positions in the AST contain `SurfaceExpression::Error(span)` nodes.

### `ParseError`

```rust
pub struct ParseError {
    pub message: String,
    pub span: Option<Span>,
}
```

Returned for fatal parse errors. Also used as elements of `ParseOutput.errors` for recovered errors.

---

## Token Types

The lexer emits these key token variants:

| Token | Source |
|---|---|
| `OpenBracket` / `CloseBracket` | `[` / `]` |
| `Colon` | `:` |
| `Dot` | `.` |
| `Pipe` | `\|` |
| `At` / `ImmediateAt` | `@` / `@` immediately after a bare word |
| `Ellipsis` | `...` |
| `DocSeparator` | `---` |
| `Identifier(String)` | bare word |
| `EscapedRef(String)` | `$name` — disambiguates variable references in head/key positions |
| `StringLiteral { prefix, delimiter, content }` | raw content, no escape processing yet |
| `Int(i64)` / `U64Lit(u64)` / `Float(f64)` | numeric literals |
| `Let` / `Case` | reserved keywords |
| `Comment(String)` | `# ...` to end of line |
| `Newline` | significant newline (used by formatter for blank-line tracking) |

`StringLiteral.content` is the raw text between delimiters. Escape sequences (`\n`, `\t`, etc.) and interpolation (`i"..."` prefix) are processed later by the lowering pass, not by the parser or lexer.

---

## AST Output: `SurfaceProgram`

```
SurfaceProgram
└── documents: Vec<Spanned<Arc<SurfaceDocument>>>

SurfaceDocument
├── header: IndexMap<String, Arc<SurfaceNode>>   ← from "--- key: val" separator lines
└── items: Vec<SurfaceItem>

SurfaceItem
├── Expr(Arc<SurfaceNode>)
└── Decl(Spanned<SurfaceDeclaration>)

SurfaceNode
├── expr: SurfaceExpression
├── span: Span
├── type_guard: TypeAnnotation    ← OnceLock, written by type checker
└── provenance: Provenance        ← OnceLock, written by macro expander
```

Every node carries a `Span` pointing into the original source. Spans are preserved through all downstream passes. The parser sets `type_guard` and `provenance` to their empty defaults; later passes fill them.

---

## Key Grammar Rules

The parser detects the form of a bracket expression from its first token:

| First token | Form |
|---|---|
| `fn` / `match` / `type` / `let` / `class` / `instance` / `macro` / `case` | special form |
| Bare word (not followed by `:`) | call |
| Bare word followed by `:` | keyed dict entry |
| `$name` | escaped variable reference in head position |
| Integer, float, string | literal |
| `---` | document separator |
| Everything else | dict |

**Pipe:** `a \| b` is parsed as `Pipe(a, b)`. Multi-stage pipes `a \| b \| c` are left-associative: `Pipe(Pipe(a, b), c)`.

**Dot access:** `expr.field` is parsed as `Field { expr: Some(expr), field: Ident(field) }`. Leading-dot `.field` (no left-hand expression) is `Field { expr: None, field: Ident(field) }`.

**Annotations:** `name@Type` and `expr@Type` attach a `Spanned<Annotation>` to the preceding node. `@` directly adjacent to an identifier is `ImmediateAt`; `@` after whitespace is `At`.

**Document separator:** `---` on its own line ends one `SurfaceDocument` and begins the next. Key-value pairs after `---` on the same line (`--- caps: [...]`) populate the next document's `header`.

---

## Invariants

1. **Depth limit enforced at parse time.** Inputs with more than 256 nested brackets are rejected before any AST is built.
2. **All nodes carry spans.** No `SurfaceNode` is produced without a source span.
3. **Error nodes are localized.** A `SurfaceExpression::Error(span)` node appears only where a bracket-internal parse error occurred. The surrounding expression is otherwise complete.
4. **`ParseOutput.program` is always present.** Even when `ParseOutput.errors` is non-empty, the program is a complete (possibly error-containing) AST.
5. **The parser does not interpret semantics.** String escape sequences, interpolation, pipe rewrites, and `$_` desugaring all happen in later passes. The parser produces the AST verbatim.
