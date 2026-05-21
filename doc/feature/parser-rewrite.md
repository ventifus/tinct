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
enum StackFrame {
    Dict           { entries: Vec<Entry>, span_start: usize },
    Call           { args: Vec<CallArg>, span_start: usize },
    Fn             { params: Vec<Param>, body_start: Option<usize>, span_start: usize },
    TypeAlias      { name: String, params: Vec<String>, span_start: usize },
    TypeAssert     { annotation: Spanned<TypeExpr>, span_start: usize },
}
```

On `Token::OpenBracket`: peek at the next non-whitespace token to classify the
form (keyword detection: `Identifier("call")`, `Identifier("fn")`,
`Identifier("type")`, `Token::At` for TypeAssert), then push the appropriate
frame.

On `Token::CloseBracket`: pop the frame, construct the corresponding AST node
(`Expr::Dict`, `Expr::Call`, `Expr::Fn`, `Expr::TypeAlias`,
`Expr::TypeAssert`), and append to the enclosing frame's expression buffer.

Between brackets: parse atoms (literals, EscapedRefs, Identifiers), access chains
(dot), and annotations — all non-recursive, driven by the current token type.

**Annotated bare words** (`word@annotation`, compound-atomic in pest) are
handled at the lexer level: when `@` follows an `Identifier` token with no
whitespace gap (detected via the `had_whitespace_before` flag), the lexer emits
`Token::ImmediateAt` instead of `Token::At`. The parser treats `ImmediateAt` as
the annotation separator in annotated bare-word context.

`MAX_DEPTH` is checked on `stack.len()` before each push. This fires before any
allocation, replacing the post-hoc depth check in the previous builder.

Static constraints are enforced inline:

- Duplicate key detection during `Dict` frame entry collection
- Variadic param rules during `Fn` frame param collection

Error messages carry bracket context:

```
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
    pub file: Spanned<File>,
    pub source: String,                                    // original input — for span-based source lookups
    pub leading_comments: BTreeMap<usize, Vec<String>>,   // keyed by span.start.offset
    pub trailing_comments: BTreeMap<usize, String>,        // keyed by span.start.offset
}
```

Leading comments (appearing before an AST node) are keyed by the
`span.start.offset` of the node they precede. Trailing comments (appearing on
the same line after a value) are keyed by the `span.start.offset` of the node
they follow. The formatter looks up both maps for each node it emits.

`source` stores the original input string. The formatter uses it for two
purposes: (1) rendering `Expr::Error(Span)` nodes verbatim by slicing
`source[span.start.offset..span.end.offset]`, and (2) recovering the bare-word
vs quoted-string distinction for `Expr::Str` nodes (see §AST-Based Formatter).

`Spanned<T>` is completely unchanged — no new fields, no broken `PartialEq`,
no memory overhead in evaluator or type-checker paths. The evaluator and type
checker receive `Spanned<File>` as before; only the formatter consumes `source`
and the comment maps.

`parse(source: &str) -> Result<ParseOutput, ParseError>`. The iterative parser
replaces pest entirely. The corpus test suite serves as the regression suite.

### AST-Based Formatter (rewrite of `src/formatter.rs`)

The rewritten formatter walks `Spanned<File>` from `ParseOutput` and emits
canonical tinct source. It does not consume the token stream.

Key properties:

- **Exact structure**: bracket form is known from AST node type, not keyword
  scanning.
- **Exact comment placement**: leading and trailing comments from
  `ParseOutput.leading_comments` and `ParseOutput.trailing_comments` are
  looked up by `span.start.offset` and emitted at the correct positions.
- **No heuristics**: `is_fn_params`, `has_whitespace_between`, keyword string
  comparisons are all eliminated.
- **Single-line / multi-line decision**: driven by rendered width of the AST
  subtree, same policy as before but computed from node structure.
- **String form preservation**: both `Token::BareWord` and
  `Token::QuotedString` collapse to `Expr::Str` during parsing — the AST does
  not distinguish them. The formatter recovers the original form via a
  span-based source lookup: for each `Expr::Str` node, it checks
  `ParseOutput.source.as_bytes()[span.start.offset]`. If that byte is `b'"'`,
  the string was quoted and is emitted with `"..."` delimiters and proper
  escaping; otherwise it was a bare word and is emitted verbatim without
  quotes. This span-peek is isolated to the `Expr::Str` arm of the formatter's
  expression walker. It is removed when unified syntax Phase 2
  (`doc/whatif/new-syntax.md`) adopts bare-word references. The `Expr` enum is
  not modified for this purpose — no `Expr::BareWord` variant is introduced.

The rewritten formatter requires a successful parse. Files with syntax errors
cannot be formatted — the formatter returns an error. Error nodes
(`Expr::Error(Span)`) are rendered by emitting the original source text for the
span verbatim, preserving partial formatting capability.

Semicolons are normalized: the AST represents the canonical form, which uses
only whitespace and newlines, never semicolons.

## Implementation

### Lexer (`src/lexer.rs`)

Emits `Token::ImmediateAt` for `@` with no whitespace after an `Identifier`
token. Whitespace-sensitive token handling updated at all match sites.

**Note**: `Token::BracketAccess` was evaluated and removed during the
`access-pipeline-phase2` sprint. Bracket access syntax is no longer part of the
language.

### Parser (`src/parser.rs`)

Complete replacement of pest-based parsing with `Vec<StackFrame>` main loop,
lexer token consumption, `ParseOutput` comment maps, static constraint
enforcement, and bracket-level error recovery. Pest and the grammar file are
removed. The corpus test suite is the regression suite.

### Formatter (`src/formatter.rs`)

Complete rewrite as AST walker over `ParseOutput`. Comment maps drive placement;
AST node types drive structure. Behavior is observably equivalent for valid
files. For files with syntax errors, the formatter returns an error.

### Parse Output (`src/parser.rs`)

`parse()` returns `Result<ParseOutput, ParseError>` where `ParseOutput` carries
`Spanned<File>`, the original `source: String`, and two `BTreeMap<usize, _>`
comment tables. `Spanned<T>` is entirely unchanged. Callers of `parse()` unwrap
`ParseOutput.file` for the AST; the formatter additionally consumes `source`
and the comment maps. The evaluator and type checker are unaffected.

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
