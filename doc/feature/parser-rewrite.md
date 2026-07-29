# Iterative Parser and AST-Based Formatter

## Overview

The iterative parser eliminates a set of structural problems in the pest-based
parser: stack overflow risk on deeply-nested input, opaque error messages, and
a formatter that operated on token streams with AST-level heuristics.

Key improvements:

1. **Security**: crafted deeply-nested input cannot reach SIGSEGV — depth is
   bounded by heap (`Vec<StackFrame>` length), never by the native stack.
2. **Error quality**: the parser tracks state explicitly, enabling messages like
   "unclosed bracket opened at line 5:3" and "expected value after `:` at line
   7:12". Bracket-level recovery enables multiple errors per parse.
3. **Formatter exactness**: the AST-based formatter eliminates all heuristics.
   `is_fn_params` is an AST node type check. Comment placement is driven by
   comment-attachment maps, not span arithmetic.
4. **Dependency simplification**: removing the pest dependency simplifies the
   build and eliminates call-limit tuning and version drift.

All phases implemented: `parser-lexer`, `parser-core`, `formatter-ast`
(completed 2026-05-05).

## Design

### Lexer (`src/lexer.rs`)

Keywords (`call`, `fn`, `type`) remain `Token::Identifier`. They are contextual,
not lexical: `[call: foo]` is a valid dict with key `"call"`; `[x call y]` has
`"call"` as a positional value. Keyword-hood is a parse-time, positional
property that the lexer does not determine.

**Note**: The lexer originally introduced a `Token::BracketAccess` variant for
whitespace-sensitive bracket syntax (`a[0]` vs `a [0]`). This feature was
evaluated and removed during the `access-pipeline-phase2` sprint. Bracket access
syntax is no longer part of the language.

### Iterative Parser (`src/parser.rs`)

Pest's implicit call stack is replaced with an explicit `Vec<StackFrame>`. The
parser holds a current expression buffer and a frame stack; bracket nesting
pushes and pops frames.

```rust
// Note: fields simplified — see src/parser.rs for full definition
enum StackFrame {
    Dict {
        entries: Vec<Spanned<SurfaceEntry>>,
        pending_key: Option<Arc<SurfaceNode>>,
        seen_keys: HashSet<String>,
        floating_annotation: Option<Spanned<Annotation>>,
        span_start: Span,
    },
    Call {
        func: Option<Arc<SurfaceNode>>,
        implied: bool,
        args: Vec<CallArg>,
        pending_key: Option<(String, Span, Option<Spanned<Annotation>>)>,
        span_start: Span,
    },
    Fn {
        params: Vec<Spanned<SurfaceParam>>,
        body: Vec<Arc<SurfaceNode>>,
        return_ann: Option<Spanned<Annotation>>,
        params_consumed: bool,
        span_start: Span,
    },
    TypeAlias {
        params: Vec<(String, Option<Spanned<Annotation>>)>,
        type_exprs: Vec<Arc<SurfaceNode>>,
        span_start: Span,
    },
    AnnotationCollect { target: AnnotationTarget, value: Option<Arc<SurfaceNode>>, span_start: Position },
}
```

On `Token::OpenBracket`: peek at the next non-whitespace token to classify the
form (keyword detection: `Identifier("call")`, `Identifier("fn")`,
`Identifier("type")`), then push the appropriate frame. `[@...]` is no longer a
special case — `@` after `[` starts an `AnnotationCollect` frame using the
floating annotation mechanism on the enclosing `Dict` frame.

On `Token::CloseBracket`: pop the frame, construct the corresponding AST node
(`Expr::Dict`, `Expr::Call`, `Expr::Fn`, `Expr::TypeAlias`,
`Expr::TypeAssert`), and append to the enclosing frame's expression buffer. When
a `Dict` frame closes with a floating annotation set and exactly one auto-indexed
entry, the entry value is wrapped in `TypeAssert` and pushed directly (not
wrapped in `Dict`).

Between brackets: parse atoms (literals, EscapedRefs, Identifiers), access chains
(dot), and annotations — all non-recursive, driven by the current token type.

**Annotations** (`word@annotation`, `]@Type`, `"s"@String`, `42@Int`) are
handled via `Token::ImmediateAt`. The lexer emits `ImmediateAt` instead of `At`
when `@` appears with no whitespace gap after any value-ending token: `CloseBracket`,
`StringLiteral`, `Int`, `Float`, `U64Lit`, `EscapedRef`, or `Identifier`. The
parser responds by popping the last value from the current frame and pushing an
`AnnotationCollect(Attached(popped))` frame. Plain `@` (with whitespace, or at
the start of a bracket form) starts an `AnnotationCollect(Floating)` frame whose
annotation is attached to the enclosing `Dict` as a floating annotation.

`MAX_DEPTH` is checked on `stack.len()` before each push. This fires before any
allocation, replacing the post-hoc depth check in the previous builder.

Static constraints are enforced inline:

- Duplicate key detection during `Dict` frame entry collection
- Variadic param rules during `Fn` frame param collection

Error messages carry bracket context:

```text
error: unclosed bracket
  --> input.llt:5:3
   |
 5 | [x: [y: z]
   |      ^ this bracket is not closed
```

**Comment attachment**: the iterative parser collects `Token::Comment` tokens
and returns them alongside the AST in a `ParseOutput` struct:

```rust
pub struct ParseOutput {
    pub leading_comments: BTreeMap<usize, Vec<String>>,   // keyed by span.start.offset
    pub trailing_comments: BTreeMap<usize, String>,        // keyed by span.start.offset
    pub blank_before: BTreeMap<usize, bool>,               // keyed by span.start.offset
    pub errors: Vec<ParseError>,                           // recovered errors from bracket-level recovery
    pub program: SurfaceProgram,                           // the parsed Surface AST
}
```

Leading comments (appearing before an AST node) are keyed by the
`span.start.offset` of the node they precede. Trailing comments (appearing on
the same line after a value) are keyed by the `span.start.offset` of the node
they follow. The formatter looks up both maps for each node it emits.

The original input string is not stored in `ParseOutput`. The formatter passes
it directly as `AstToDictOpts.source` (an `Option<&str>`) to
`surface_program_to_dict` in `surface_convert.rs` (see `formatter.rs:84`).
`AstToDictOpts.source` enables the bare-word vs quoted-string distinction for
`SurfaceExpression::StringLiteral` keys in dict entries.

The evaluator and type checker consume only `program`; the formatter additionally
uses the comment maps and passes the original `&str` input through
`AstToDictOpts`.

`parse(source: &str) -> Result<ParseOutput, ParseError>`. The iterative parser
replaces pest entirely. The corpus test suite serves as the regression suite.

### AST-Based Formatter (`src/formatter.rs`)

The formatter is tinct-hosted: `src/formatter.rs` is a thin Rust wrapper that
orchestrates three steps.

1. **Parse** — the input source is parsed to a `ParseOutput` via `parse()`.
2. **Convert to dict** — `surface_program_to_dict` (from `src/surface_convert.rs`)
   converts the `SurfaceProgram` AST to a tinct `Value::Dict` representation.
   This conversion is driven by `AstToDictOpts`, which carries:
   - `source: Option<&str>` — the original input string, passed directly from
     the `format_source_tinct` call site (not from `ParseOutput`). Used by
     `surface_convert.rs` to distinguish bare-word dict keys from quoted string
     keys by inspecting the character at each key's span offset.
   - `comments: Option<CommentMaps>` — borrows `ParseOutput.leading_comments`,
     `ParseOutput.trailing_comments`, and `ParseOutput.blank_before` to embed
     comment and blank-line metadata into Entry and Document nodes of the dict.
3. **Evaluate the tinct formatter** — the dict is passed as pipeline input (`%`)
   to a tinct-hosted formatter script (`stdlib/cli/fmt/pretty.llt` or similar).
   The script performs all formatting decisions — single-line vs multi-line
   layout, indentation, spacing — and returns the formatted source as a string.

All layout decisions (width thresholds, comment placement, indentation) are
implemented in tinct, not in Rust. The Rust layer handles only parsing,
AST-to-dict conversion, and tinct evaluation.

The formatter requires a successful parse. Files with syntax errors cannot be
formatted — the formatter returns an error.

## Implementation

### Lexer (`src/lexer.rs`)

Emits `Token::ImmediateAt` for `@` with no whitespace after any value-ending token (identifier, `]`, string literal, number, float, u64 literal). The `last_was_nonwhitespace` flag is set by all value-producing tokens and cleared by structural delimiters and keywords. Access-field identifiers (after `.`) also set the flag, enabling `obj.field@Type`. Whitespace-sensitive token handling updated at all match sites.

**Note**: `Token::BracketAccess` was evaluated and removed during the
`access-pipeline-phase2` sprint. Bracket access syntax is no longer part of the
language.

### Parser (`src/parser.rs`)

Complete replacement of pest-based parsing with `Vec<StackFrame>` main loop,
lexer token consumption, `ParseOutput` comment maps, static constraint
enforcement, and bracket-level error recovery. Pest and the grammar file are
removed. The corpus test suite is the regression suite.

### Formatter (`src/formatter.rs`)

Thin Rust wrapper: parses the input, converts the AST to a dict via
`surface_program_to_dict`, then evaluates the tinct-hosted formatter script with
the dict as pipeline input. All layout logic lives in tinct. For files with
syntax errors, the formatter returns an error without invoking the tinct script.

### Parse Output (`src/parser.rs`)

`parse()` returns `Result<ParseOutput, ParseError>` where `ParseOutput` carries
the `SurfaceProgram` AST, three `BTreeMap<usize, _>` metadata tables
(`leading_comments`, `trailing_comments`, `blank_before`), and a `Vec<ParseError>`
of recovered errors. The evaluator and type checker consume only `program`; the
formatter additionally borrows the comment maps via `AstToDictOpts::comments`.
The original source string is not stored in `ParseOutput` — callers that need it
(such as the formatter) pass it directly.

The `(level, slot)` de Bruijn annotation from `doc/feature/arena-patterns.md`
annotates `VarRef` nodes in the AST as a separate post-parse pass. The comment
maps are an independent side channel; both coexist with no conflict.

### Pest Dependency (`Cargo.toml`)

Removed. `pest` and `pest_derive` are no longer build dependencies.

## References

- **pest** (2018). PEG parser library for Rust. — Previous tinct parser
  implementation. Compound-atomic rules (`${}`) are the mechanism for
  whitespace-sensitive access chains in the former `src/grammar.pest`.
- Nystrom, R. (2021). *Crafting Interpreters*. Genever Benning. — Reference
  implementation for hand-written recursive descent and Pratt parsers;
  Chapter 16 covers bytecode compilers, Chapter 17 closures. The iterative
  parser design draws on the pattern of making control flow explicit.
- Pratt, V.R. (1973). "Top down operator precedence." In *POPL '73*, pp.
  41–51. ACM. — Top-down operator precedence parsing; tinct's bracket grammar
  does not use Pratt's algorithm directly.
- Wadler, P. (1998). "A prettier printer." In *The Fun of Programming*,
  Cornish, J. & Gibbons, J. (eds.), Palgrave. — The combinator algebra
  underlying modern AST-based formatters (Prettier, rustfmt's intermediate
  IR). The rewritten formatter's single-line / multi-line decision follows
  Wadler's `fits` predicate applied to rendered subtree width.
- Nickel language (2022). LALRPOP-based grammar (`grammar.lalrpop`) with
  hand-written extensions (`src/parser/`) for a lazy configuration language
  with a similar bracket-heavy syntax. Reference for parsing complex bracket
  grammars in Rust; uses LALR rather than a hand-written iterative approach.
- Nix evaluator (C++). `src/libexpr/parser.y`, `src/libexpr/lexer.l` —
  Flex/Bison-based parser for the canonical lazy configuration language.
  Iterative evaluation with explicit frame types (the `iterative-eval`
  parallel); hand-written lexer with `last_token` tracking for
  whitespace-sensitive disambiguation.
- Go `go/ast` package (2009–present). — AST representation that includes
  `CommentGroup` nodes attached to declarations and statements. Reference
  for the leading/trailing comment side-channel approach used in `ParseOutput`.
- Matklad (2018). "Resilient LL Parsing Tutorial." Blog post. — Technique
  for error-resilient hand-written parsers that produce partial ASTs on
  syntax errors. Foundation for bracket-level error recovery.
