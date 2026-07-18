# Parser

This document is for Rust contributors working in `src/lexer.rs` and `src/parser.rs`. Tinct developers wanting a high-level picture of what the parser produces and why should focus on the [AST Output](#ast-output-surfaceprogram) and [Key Grammar Rules](#key-grammar-rules) sections.

The parser converts tinct source text into a `SurfaceProgram` — a tree of `Arc<SurfaceNode>` values,
each carrying a source span. It is a hand-written iterative parser that uses an explicit `Vec<StackFrame>`
rather than Rust's call stack, making it safe for deeply nested inputs.

---

## Design

The lexer (`src/lexer.rs`) produces a flat token stream with accurate source spans. The parser
(`src/parser.rs`) consumes that stream and builds the Surface AST.

Key properties:

- **Iterative, not recursive.** Bracket nesting is tracked with an explicit `Vec<StackFrame>`. No Rust
  stack growth per nesting level.
- **Error recovery inside brackets.** When a parse error occurs inside a bracket form, the parser records
  a `ParseError` in `ParseOutput.errors` and continues. The broken sub-expression becomes a
  `SurfaceExpression::Error` node in the AST. Fatal errors (at the top level, or when the lexer fails)
  return `Err(ParseError)`. The depth limit (`MAX_PARSE_DEPTH`) triggers `recover_from_failed_open`
  when it fires inside a nested bracket, so even depth-limit violations inside brackets produce partial
  ASTs rather than fatal failures.
- **Comment collection.** Comments are extracted separately into `leading_comments` and
  `trailing_comments` maps keyed by the **byte offset** of the adjacent node's start position (not line
  number). They do not appear in the AST.
- **Two annotation paths.** Annotations inside bracket forms are collected by `AnnotationCollect`
  stack frames (the main path). Annotations inside `---` document-separator headers are parsed by the
  separate `parse_annotation_direct` function. Unification of these two paths is tracked in T-1617.

---

## Limits

| Limit | Value | Where enforced |
|---|---|---|
| `MAX_LEX_DEPTH` | 256 | Lexer — `[` at nesting depth ≥ 256 is a `LexError` |
| `MAX_PARSE_DEPTH` | 256 | Parser — `OpenBracket` at stack depth ≥ 256 is a `ParseError` |

There is no `MAX_SOURCE_SIZE` limit. The only enforced limits are nesting depth, one at each stage.
Both limits use the same value (256) but are independent checks.

---

## Entry Points

### `parse`

```rust
pub fn parse(source: &str, file: Arc<SourceFile>) -> Result<ParseOutput, ParseError>
```

Parse a complete tinct source string. The caller always provides a `SourceFile` (created with
`Arc::new(SourceFile { path: ..., content: ... })`). Returns `Ok(ParseOutput)` on success
(including partial recovery from bracket-internal errors) or `Err(ParseError)` on fatal parse
failure. There is no anonymous variant — every call carries a meaningful file identity. Tokens
carry the `SourceFile` from the moment the lexer creates them, so all spans have correct
attribution from the start.

Callers outside the parser: `lib.rs` (`run_loader_pipeline`, `typecheck_source`,
`typecheck_source_errors_only`), `src/main.rs` (via re-export from `lib.rs`), and corpus test
harnesses. `parse` is re-exported from `lib.rs` as part of the public crate API.

### `parse_surface_expression`

```rust
pub fn parse_surface_expression(input: &str) -> Result<Arc<SurfaceNode>, ParseError>
```

Parse a single tinct expression (not a full program). Uses `"<expression>"` as the synthetic file
name. Returns the first item of the first document as an `Arc<SurfaceNode>`. If the first item is a
`SurfaceItem::Decl`, it is wrapped in `SurfaceExpression::Decl` so it can be displayed uniformly.
Used by tests and the corpus test runner.

### `parse_with_recovery`

```rust
pub fn parse_with_recovery(input: &str) -> ParseOutput
```

Like `parse`, but always succeeds. Fatal errors are returned in `ParseOutput.errors` rather than as
`Err(...)`, and a synthetic empty `SurfaceProgram` is returned. Uses `"<recovery>"` as the synthetic
file name. Intended for LSP diagnostic passes and batch linting tools that need an AST even in the
presence of unrecoverable errors.

### `format_parse_error`

```rust
pub fn format_parse_error(err: &ParseError, source: &str, file_name: &str) -> String
```

Format a `ParseError` with Rust-style rich diagnostics: a header line, a `-->` location line, and a
source snippet with caret pointing at the error location. Re-exported from `lib.rs` as part of the
public crate API. Used by the CLI to display parse errors to users.

---

## Output Types

### `ParseOutput`

```rust
pub struct ParseOutput {
    pub program: SurfaceProgram,
    pub errors: Vec<ParseError>,
    pub leading_comments: BTreeMap<usize, Vec<String>>,
    pub trailing_comments: BTreeMap<usize, String>,
    pub blank_before: BTreeMap<usize, bool>,
}
```

`program` is the primary output consumed by the evaluator and type checker. The other fields are for
the formatter.

The comment maps are keyed by **byte offset** (`span.start.offset`) of the adjacent AST node, not
by line number. `leading_comments[offset]` holds comments on lines immediately before the node at
that offset. `trailing_comments[offset]` holds the comment on the same line as the node. `blank_before[offset]`
is set to `true` when two or more consecutive newlines (or semicolons) precede the node.

`errors` is non-empty when the parser recovered from one or more bracket-internal errors. Those
positions in the AST contain `SurfaceExpression::Error(span)` nodes.

`ParseOutput::as_surface_program()` is a convenience accessor returning `&self.program`; prefer
accessing the field directly.

### `ParseError`

```rust
pub struct ParseError {
    pub message: String,
    pub span: Option<Span>,
}
```

Returned for fatal parse errors and also used as elements of `ParseOutput.errors` for recovered
errors. Implements `Display` as `"line:col: message"` when a span is present, or just `"message"`
without.

### `LexError`

```rust
pub struct LexError {
    pub message: String,
    pub span: Span,
}
```

Returned by `lexer::tokenize` on lexer-level failures (unterminated string, invalid radix digit,
bad number literal, depth limit exceeded). The parser converts a `LexError` to a `ParseError` and
returns `Err(...)` immediately — no partial AST is produced for lexer failures.

---

## Token Types

The lexer produces `Spanned<Token>` values. Every token carries a `Span` with start/end `Position`
(offset + line + column) and a reference to the `SourceFile`.

| Token | Source | Notes |
|---|---|---|
| `OpenBracket` | `[` | Nesting depth checked; lexer enforces `MAX_LEX_DEPTH` |
| `CloseBracket` | `]` | Sets `last_was_nonwhitespace = true` |
| `Colon` | `:` | Key-value separator; sets `last_was_nonwhitespace = false` |
| `Semicolon` | `;` | Newline alias — treated identically to `Newline` in all parse positions |
| `Dot` | `.` | Always a Dot token — no whitespace sensitivity. Sets `after_access_dot = true` |
| `Pipe` | `\|` | Pipe operator |
| `At` | `@` | Annotation separator with preceding whitespace (or after a non-value token) |
| `ImmediateAt` | `@` | Annotation separator immediately after a value-ending token (no whitespace) |
| `Ellipsis` | `...` | Variadic/rest marker |
| `DocSeparator` | `---` | Document separator; only when NOT followed by a bare-word char |
| `Newline` | `\n`, `\r\n`, bare `\r` | Emitted for blank-line tracking; resets `last_was_nonwhitespace` |
| `Comment(String)` | `# …` to EOL | Does not update `last_was_nonwhitespace`; preserved for formatter |
| `Int(i64)` | decimal, hex `0x`, binary `0b`, octal `0o`/leading-zero | Sets `last_was_nonwhitespace = true` |
| `U64Lit(u64)` | integer with `u` suffix (`42u`, `0xFFu`) | Sets `last_was_nonwhitespace = true` |
| `Float(f64)` | decimal with `.` or `e`/`E` exponent | Sets `last_was_nonwhitespace = true` |
| `Identifier(String)` | bare word | Sets `last_was_nonwhitespace = true` |
| `StringLiteral { prefix, delimiter, content }` | `"…"`, `"""…"""`, `i"…"`, `i"""…"""` | Raw content only; sets `last_was_nonwhitespace = true` |
| `EscapedRef(String)` | `$name` | Disambiguator in head/key positions; sets `last_was_nonwhitespace = true` |
| `Let` | `let` | Reserved keyword; sets `last_was_nonwhitespace = false` |
| `Case` | `case` | Reserved keyword; sets `last_was_nonwhitespace = false` |

`StringLiteral.content` is always raw — no escape processing, no `$name` interpolation, no
indentation stripping. These are processed later by the evaluator/lowering pass.

`true` and `false` are plain `Identifier` tokens, not reserved keywords. `Boolean` is a
user-defined type.

`%name` tokens (capability references like `%cwd`, `%libdir`) lex as plain `Identifier` tokens —
`%` is a bare-word character with no special treatment.

---

## Whitespace Sensitivity

Only `@` is whitespace-sensitive in tinct. The `[` token is always `OpenBracket` regardless of
surrounding whitespace — bracket-access syntax was removed in sprint `access-pipeline-phase2`.

`ImmediateAt` vs `At` is determined by two lexer flags:

- `had_whitespace_before: bool` — set by `skip_whitespace_except_newline()` when horizontal
  whitespace (space or tab, but not newlines) is skipped before the current token.
- `last_was_nonwhitespace: bool` — set to `true` after any value-ending token (`Identifier`,
  `CloseBracket`, `StringLiteral`, `Int`, `Float`, `U64Lit`, `EscapedRef`), reset to `false` after
  whitespace, newlines, `OpenBracket`, `Colon`, `Pipe`, and keywords (`Let`, `Case`). Comments
  leave `last_was_nonwhitespace` unchanged.

When `@` is seen: if `!had_whitespace_before && last_was_nonwhitespace`, emit `ImmediateAt`;
otherwise emit `At`. This means all of the following produce `ImmediateAt`: `x@Int`,
`]@Seq`, `"s"@String`, `42@Int`, `42u@U64`, `3.14@Float`, `$var@Type`, `obj.field@Type`.

Dot access is not whitespace-sensitive: `a .b` and `a.b` produce the same token stream
(`Identifier Dot Identifier`).

---

## Lexer: Identifier Character Sets

Tinct identifier lexing uses a **denylist** (not an allowlist) for extensibility. Two relevant
predicates:

- `is_var_ident_char(c)`: excludes `' '`, `'\t'`, `'\r'`, `'\n'`, `'['`, `']'`, `':'`, `';'`,
  `'#'`, `'"'`, `'@'`, `'.'`, `'|'`. Everything else is a valid identifier character, including
  `%`, `!`, `?`, `-`, `_`, `$` (inside a `$name` ref), digits, Unicode, etc.
- `is_access_field_char(c, is_first)`: currently delegates to `is_var_ident_char(c)`, so access
  field names use the same rules as general identifiers.

After a `Dot` token, the lexer sets `after_access_dot = true`. The next bare word is then lexed
in "access field" mode (using `is_access_field_char`). In practice this produces the same result as
normal identifier lexing since both predicates are identical.

---

## AST Output: `SurfaceProgram`

```
SurfaceProgram
└── documents: Vec<Spanned<Arc<SurfaceDocument>>>

SurfaceDocument
├── header: IndexMap<String, Arc<SurfaceNode>>   ← from "--- key: val" separator line
└── items: Vec<SurfaceItem>

SurfaceItem
├── Expr(Arc<SurfaceNode>)
└── Decl(Spanned<SurfaceDeclaration>)

SurfaceNode
├── expr: SurfaceExpression
├── span: Span
├── type_guard: TypeAnnotation    ← OnceLock; written by type checker, read by lowerer
└── provenance: Provenance        ← OnceLock; written by macro expander
```

Every node carries a `Span` pointing into the original source. Spans are preserved through all
downstream passes. The parser sets `type_guard` and `provenance` to fresh empty `OnceLock` defaults;
later passes fill them in.

`SurfaceNode::new(expr, span)` is the canonical constructor — it fills `type_guard` and `provenance`
with their defaults. Use the `surface_node!(expr, span)` macro for struct-literal contexts.

### Inline OnceLock Annotations on Nodes

Several AST node variants carry additional `OnceLock`-based fields that are written by later passes
and read by still-later ones. The parser produces all nodes with these fields in their empty default
state:

| Field | Type | Carrier | Written by | Read by |
|---|---|---|---|---|
| `type_guard` | `TypeAnnotation` | `SurfaceNode` | Type checker | Lowerer |
| `provenance` | `Provenance` | `SurfaceNode` | Macro expander | Debugger/formatter |
| `resolution` | `Resolution` | `VarRef`, leading-dot `Field` | Resolver | Lowerer |
| `call_dispatch` | `CallDispatch` | `VarRef` (typeclass method calls) | Type checker | Lowerer |
| `field_slot` | `SlotAnnotation` | `Field` | Type checker | Lowerer |
| `resolved_annotation_type` | `TypeAnnotation` | `SurfaceParam` | Type checker | Lowerer |
| `resolved_type` | `TypeAnnotation` | `TypeAssert` | Type checker | Lowerer |
| `pin_resolution` | `Resolution` | `Pattern::Pin` | Resolver | Evaluator |
| `to_match_binding` | `MatchableBinding` | `Pattern::Predicate` | Type checker | Lowerer/evaluator |

Clone semantics: `Resolution`, `TypeAnnotation`, and `SlotAnnotation` **reset** to empty on clone
(cloned nodes in new scopes must be re-annotated). `MatchableBinding` **preserves** its value
through clone because the instance binding name is global and scope-independent.

---

## Key Grammar Rules

### Bracket Form Classification

When `OpenBracket` is seen, the parser peeks at the next significant (non-whitespace, non-comment)
token to classify the form:

| First token after `[` | Horizontal colon next? | Form |
|---|---|---|
| `call` identifier | no | Explicit call `[call f args…]` |
| `fn` identifier | no | Function definition `[fn [let params…] body]` |
| `type` identifier | no | Type alias `[type …]` |
| `quote` identifier | no | Quasiquote `[quote expr]` |
| `unquote` identifier | no | Unquote `[unquote expr]` (only valid inside `[quote …]`) |
| `unquote-splice` identifier | no | Unquote-splice `[unquote-splice expr]` (only valid in list position inside `[quote …]`) |
| `syntax-class` identifier | no | Syntax class declaration |
| `match` identifier | no | Pattern match `[match scrutinee arm: body …]` |
| `class` identifier | no | Type class declaration |
| `instance` identifier | no | Type class instance |
| `pattern` identifier | no | Pattern declaration `[pattern bindings]` |
| `Let` keyword | no | Binding declaration `[let x@T y z: default]` |
| `Case` keyword | no | Case arm `[case [let bindings] pattern body]` |
| any keyword above | yes | Dict (keyword followed by `:` is a dict entry, not a special form) |
| `At` / `ImmediateAt` | — | Dict (floating annotation form `[@Type expr]`) |
| Identifier, no horizontal colon, no `ImmediateAt` next | — | Implied call `[f args…]` |
| Identifier with horizontal colon next | — | Dict (first token is a key) |
| Identifier with `ImmediateAt` next | — | Dict (annotated first element is data, not a call head) |
| `EscapedRef`, literals, or anything else | — | Dict |

"Horizontal colon" means a `Colon` on the same line (comments skipped, but newlines and semicolons
are NOT skipped). This prevents `[call\n: x]` from misclassifying as a dict entry — `call` across
a newline from `:` is still treated as a call.

Inside `[let …]`, nested `[` is always classified as a sub-`LetDecl` (for multi-payload
destructuring), except when the next token after `[` is `let`, in which case it falls through to
standard classification. This is the one context-sensitive rule in the parser.

### Pipe

`a | b` is parsed as `Pipe(a, b)`. Multi-stage pipes `a | b | c` are left-associative:
`Pipe(Pipe(a, b), c)`. `Pipe` is a surface-only node — the lowering pass rewrites it to `Call`
before evaluation.

### Dot Access

`expr.field` is parsed as `Field { expr: Some(expr), field: Ident(field) }`. An integer after a
dot (`a.0`) produces `Field { expr: Some(a), field: Int(0) }`. Leading-dot `.field` (no
left-hand expression) produces `Field { expr: None, field: Ident(field) }` — semantics: skip the
current letrec scope frame and look up `field` in the parent scope.

Whitespace before `.` is permitted and produces the same result as no whitespace.

`..` (two consecutive dots) emits two `Dot` tokens — range syntax was removed. `...` is `Ellipsis`.

### Annotations

`name@Type` and `expr@Type` attach a `Spanned<Annotation>` to the preceding node via an
`AnnotationCollect` stack frame. `@` directly adjacent to a value-ending token is `ImmediateAt`;
`@` after whitespace is `At`. Both trigger annotation collection — the distinction is used only
to determine whether the annotation is "attached" (follows a value) or "floating" (appears before
one, as in `[@Type expr]`).

Annotation forms:
- `@Name` → `Annotation::Simple("Name")`
- `@Name@Inner` → `Annotation::Annotated("Name", inner)` (chained)
- `@[key: val …]` → `Annotation::PropertyDict(entries)` — inside `[@Type expr]` brackets,
  whitespace is allowed (the annotation value is non-atomic)

The `parse_annotation_direct` function handles annotations appearing in `---` separator lines
(document header key-value pairs). This is a separate parsing path from the `AnnotationCollect`
frame mechanism and does not share code with it. Unification is tracked in T-1617.

### Document Separator

`---` on its own line (not followed by a bare-word character) ends one `SurfaceDocument` and begins
the next. `----` is not a separator — the `!bare_word_char` lookahead in the lexer prevents it
from matching.

Key-value pairs on the same line as `---` (e.g., `--- caps: [...]`, `--- stage: "type"`) populate
the next document's `header` dict. Header values are parsed by `parse_annotation_direct`.

### `case` vs `let` keyword distinction

`let` and `case` are proper `Token::Let` and `Token::Case` — reserved keywords emitted by the lexer
as distinct token variants rather than `Identifier`. All other special-form names (`fn`, `call`,
`type`, `match`, `class`, `instance`, `quote`, `unquote`, `unquote-splice`, `syntax-class`,
`pattern`) are plain `Identifier` tokens disambiguated at the parser level by name matching.

---

## Stack Frames

The parser maintains a `Vec<StackFrame>`. An `OpenBracket` pushes one frame; a `CloseBracket` pops
one and constructs an AST node. All frames carry `span_start: Position` for span construction.

```rust
enum StackFrame {
    Dict { entries, pending_key, seen_keys, span_start, floating_annotation },
    Call { func, implied, args, pending_key, span_start },
    Fn { params, body, return_ann, span_start, params_consumed },
    TypeAlias { params, type_exprs, pending_key, span_start },
    Quote { expr, span_start },
    Unquote { expr, span_start },
    UnquoteSplice { expr, span_start },
    SyntaxClass { name, pattern, message, pending_key, span_start },
    Match { scrutinee, arms, pending_pattern_expr, pending_pattern, span_start },
    ClassDecl { name, params, superclasses, methods, pending_key, structural_metadata, span_start },
    InstanceDecl { class_name, arms, pending_arm_pattern, pending_key, current_arm_methods, span_start },
    PatternDecl { bindings, span_start },
    LetDecl { bindings, pending_key, pending_rhs, span_start },
    CaseDecl { let_bindings, pattern, body, span_start },
    Pipe { lhs, span_start },
    AnnotationCollect { target, value, span_start },
}
```

`AnnotationCollect` is the main annotation mechanism. Its `target` is one of:
- `Attached(Arc<SurfaceNode>)` — annotation on a preceding expression (`x@Type`)
- `Floating` — annotation to apply to the next expression in the parent frame (`[@Type …]`)
- `FnReturn` — return-type annotation on `[fn@Type …]`

`Pipe` frames are not bracket-delimited — they are pushed when a `Pipe` token is seen and popped
when the RHS expression completes (driven by `drain_annotation_frames` / `push_value` logic, not
by `CloseBracket`).

---

## Error Recovery

Two recovery helpers exist:

- `recover_from_bracket_error`: Called when an error occurs while processing tokens inside an
  existing frame. Pops the frame, builds a partial expression (preserving valid entries from
  `Dict` and `Call` frames), appends an `SurfaceExpression::Error` node, pushes the partial
  expression to the parent, and skips to the matching `]`.
- `recover_from_failed_open`: Called when an error occurs when opening a bracket (no frame was
  pushed — e.g., depth limit hit at the `[` token itself). Does not pop anything. Pushes
  `SurfaceExpression::Error` to the current parent and skips to the matching `]`.

`skip_to_closing_bracket` scans forward tracking nesting depth to find the matching `]`.

---

## Quote Depth Tracking

The parser maintains `quote_depth: u32`. It is incremented when entering a `[quote …]` form and
decremented when entering `[unquote …]` or `[unquote-splice …]`. `[unquote …]` and
`[unquote-splice …]` are parse errors when `quote_depth == 0`. `[unquote-splice …]` is also a
parse error when the parent frame is a `Quote` frame (it must appear in a list position, not at
the top of a quote body).

---

## Parser Helpers

The following functions assist the main parse loop:

| Function | Purpose |
|---|---|
| `peek_next_significant` | Skip whitespace, newlines, semicolons, and comments to find the next token |
| `peek_next_horizontal` | Skip only comments (not newlines/semicolons) — for keyword-colon lookahead |
| `skip_whitespace_tokens` | Advance past whitespace/newlines/comments, collecting them into the comment maps |
| `key_to_string` | Extract a comparable string from a key expression for parse-time duplicate detection (literal keys only) |
| `skip_to_closing_bracket` | Scan forward to the matching `]` for error recovery |
| `recover_from_bracket_error` | Error recovery when a parse error occurs inside a pushed frame |
| `recover_from_failed_open` | Error recovery when a parse error occurs before a frame is pushed |
| `parse_annotation_direct` | Separate annotation parser for `---` document header key-value pairs |

---

## Downstream Interaction

The parser produces a `SurfaceProgram`. Downstream passes consume it in order:

1. **`desugar`** (`src/desugar.rs`) — `$_` shorthand rewriting and other surface-level desugaring. Mutates the `SurfaceProgram` in place.
2. **`resolve`** (`src/resolve.rs`) — assigns de Bruijn coordinates to `VarRef` and leading-dot `Field` nodes by writing into their inline `Resolution` OnceLock fields.
3. **Type checker** (`src/typecheck.rs`) — infers and checks types; writes `TypeAnnotation`, `CallDispatch`, and `SlotAnnotation` OnceLock fields on nodes.
4. **Lowerer** (`src/lower.rs`) — converts `SurfaceExpression` to `CoreExpr` using the resolver and type checker annotations. Reads all inline OnceLock fields.
5. **Evaluator** (`src/eval.rs` etc.) — evaluates `CoreExpr` in the scope arena.
6. **Formatter** (`src/formatter.rs`) — uses `ParseOutput.leading_comments`, `trailing_comments`, and `blank_before` to reconstruct canonical source layout.

The parser has no knowledge of the type system, evaluator, or name resolution. It does not call
into any of those subsystems. It calls only `lexer::tokenize`.

---

## Invariants

1. **Depth limit enforced at both stages.** The lexer rejects `[` at nesting depth ≥ `MAX_LEX_DEPTH`
   (256) with a `LexError`. The parser rejects `OpenBracket` at stack depth ≥ `MAX_PARSE_DEPTH`
   (256) with a `ParseError`.
2. **All nodes carry spans.** No `SurfaceNode` is produced without a source span. Rust-synthesized
   nodes use `rust_span!()` which embeds the Rust source file and line number.
3. **Error nodes are localized.** A `SurfaceExpression::Error(span)` node appears only where a
   bracket-internal parse error occurred. The surrounding expression is otherwise complete.
4. **`ParseOutput.program` is always present.** Even when `ParseOutput.errors` is non-empty, the
   program is a complete (possibly error-containing) AST.
5. **The parser does not interpret semantics.** String escape sequences (`\n`, `\t`, etc.),
   interpolation (`$name`, `${expr}` inside `i"…"`), triple-quoted indentation stripping, pipe
   rewrites, and `$_` desugaring all happen in later passes. The parser produces the AST verbatim.
6. **Duplicate key detection is parse-time for literal keys only.** Dict entries with string,
   integer, float, or unsigned-integer keys are checked for duplicates at parse time using
   `seen_keys`. Computed keys (`Field`, `Call`, `VarRef`) are checked at evaluation time.
7. **OnceLock fields are empty at parse time.** All inline `Resolution`, `TypeAnnotation`,
   `CallDispatch`, `SlotAnnotation`, `Provenance`, and `MatchableBinding` fields are initialized
   to their empty defaults by the parser. No pass other than the designated writer sets them.
