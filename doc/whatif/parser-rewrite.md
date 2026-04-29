# What If: Iterative Parser and AST-Based Formatter for tinct

**State:** Accepted — 2026-04-28

What would it take to replace tinct's pest-based parser with a hand-written
iterative parser and rewrite the formatter to operate on the AST?

## Current State

tinct parses source text using a pest PEG grammar (`src/grammar.pest`, 230
lines) and a post-parse AST builder (`src/parser.rs`, ~900 lines). The
formatter (`src/formatter.rs`) operates directly on the lexer's token stream —
not the AST — to avoid comment loss and a dependency on the parser.

### Parser

The pest grammar handles four concerns simultaneously: bracket-form
disambiguation (dict vs. call vs. fn vs. type-alias vs. type-assert), literal
precedence (float > int > bool > quoted > annotated bare > bare word),
whitespace-sensitive access chains (`$a.b` vs. `$a .b`, `$a[0]` vs.
`$a [0]`), and static constraints (duplicate key detection, variadic param
rules). These are checked by the builders after pest succeeds; the grammar
itself is permissive.

Whitespace sensitivity is handled by pest's compound-atomic rules (`${...}`),
which suppress implicit whitespace between sub-rules. This is effective but
opaque: the distinction between access and non-access is a grammar-level
implementation detail with no token-level counterpart.

Recursion depth is bounded by two layered checks. `pest::set_call_limit` is
set to 500,000 rule invocations to prevent unbounded recursion. After pest
succeeds, `MAX_PARSE_DEPTH = 256` is checked during AST construction. But the
order matters: adversarial deeply-nested input can exhaust the 64 MB worker
stack (SIGSEGV) before the call limit fires, because pest's own recursion is
not bounded by the same guard. The security finding in TODO.md (line 444)
notes this gap.

Error messages come from pest's backtracking failure reports ("expected X"),
with no structural context. The first syntax error is fatal — no recovery, no
second error.

### Formatter

The formatter (`src/formatter.rs`) tokenizes source via `src/lexer.rs` and
walks the flat token stream, making bracket-level formatting decisions without
AST context. This design was chosen explicitly because pest silently discards
comment tokens, making AST-based formatting impossible without rethinking
comment handling. See `doc/12-tooling.md §Formatter`.

The consequence is a set of heuristics that substitute for missing AST
structure:

- **`is_fn_params`** — scans backwards from a `[` for an `Identifier("fn")`
  token to decide whether to format the bracket as a param list. Fragile:
  a comment containing "fn" before a bracket can misfire (TODO line 1107,
  `src/formatter.rs:418-450`).

- **Whitespace-sensitive bracket detection** — four separate call sites use
  `has_whitespace_between(a, b)` (span offset comparison) to decide whether
  `[` is a bracket access or a new expression, duplicating logic that belongs
  in the lexer.

- **Keyword detection** — keywords are detected via string comparison
  (`Identifier(s) if s == "call" || s == "fn" || s == "type"`), which is
  correct but noise — the AST would already encode the distinction.

### What's Missing

1. **Stack safety at parse time** — the explicit depth bound fires post-hoc
   during AST construction, after pest has already consumed stack.
2. **Precise error messages** — no bracket-context tracking ("unclosed bracket
   opened at line 5:3", "expected value after `:` at line 7:12").
3. **Error recovery** — first error is fatal; no multi-error reporting.
4. **Comment preservation in the AST** — pest discards comments, blocking
   AST-based formatting.
5. **An exact formatter** — heuristic substitutes for structure.

## Why This Rewrite Matters for tinct

1. **Security**: crafted deeply-nested input can reach SIGSEGV before the
   depth guard fires. An iterative parser eliminates this structurally — depth
   is bounded by heap (`Vec<StackFrame>` length), never by the native stack.
2. **Error quality**: hand-written parsers track parser state explicitly,
   enabling messages like "unclosed bracket opened at line 5:3" and
   "expected value after `:` at line 7:12". Bracket-level recovery enables
   multiple errors per parse.
3. **Formatter exactness**: an AST-based formatter eliminates all heuristics.
   `is_fn_params` becomes an AST node type check. Whitespace-sensitive
   bracket detection becomes a token type check. Comment placement is driven
   by comment-attachment in the AST, not span arithmetic.
4. **Lexer as shared infrastructure**: making whitespace-sensitive bracket
   detection a lexer concern (`BracketAccess` token) benefits every consumer
   — parser, formatter, and any future LSP token type queries.
5. **Dependency simplification**: removing the pest dependency simplifies
   the build and eliminates a source of call-limit tuning and version drift.

## Design

### Lexer (`src/lexer.rs`)

Add one new token variant: `Token::BracketAccess`. The lexer emits it instead
of `Token::OpenBracket` when `[` follows a value-producing token with no
intervening whitespace:

```
$a[0]   → EscapedRef("a"), BracketAccess, Int(0), CloseBracket
$a [0]  → EscapedRef("a"), OpenBracket, Int(0), CloseBracket
```

Detection uses the existing `last_significant_token` tracking already in
`src/lexer.rs:120-129`. A `[` is `BracketAccess` when `last_significant_token`
is a value-ending token (CloseBracket, EscapedRef, Identifier, QuotedString,
Int, Float, BoolLit) and there is no whitespace gap between the previous
token's end offset and the current `[`'s start offset.

Keywords (`call`, `fn`, `type`) remain `Token::Identifier`. They are contextual,
not lexical: `[call: foo]` is a valid dict with key `"call"`; `[x call y]`
has `"call"` as a positional value. Keyword-hood is a parse-time, positional
property that the lexer cannot determine.

The formatter's four `has_whitespace_between` call sites around
`Token::OpenBracket` collapse to a single `Token::BracketAccess` match once
the rewritten formatter operates on this token stream. This simplification is
a direct benefit of placing whitespace sensitivity in its natural home.

### Iterative Parser (`src/parser2.rs`)

Replace pest's implicit call stack with an explicit `Vec<StackFrame>`. The
parser holds a current expression buffer and a frame stack; bracket nesting
pushes and pops frames.

```rust
enum StackFrame {
    Dict           { entries: Vec<Entry>, span_start: usize },
    Call           { args: Vec<CallArg>, span_start: usize },
    Fn             { params: Vec<Param>, body_start: Option<usize>, span_start: usize },
    TypeAlias      { name: String, params: Vec<String>, span_start: usize },
    TypeAssert     { annotation: Spanned<TypeExpr>, span_start: usize },
    BracketAccessKey { span_start: usize },
}
```

`BracketAccessKey` handles bracket access keys that contain nested bracket
expressions — e.g. `$a[some_expr]` where `some_expr` itself contains `[]`.
When `Token::BracketAccess` is encountered during atom parsing, a
`BracketAccessKey` frame is pushed; `Token::CloseBracket` pops it and
produces the key expression for the enclosing access chain.

On `Token::OpenBracket`: peek at the next non-whitespace token to classify the
form (keyword detection: `Identifier("call")`, `Identifier("fn")`,
`Identifier("type")`, `Token::At` for TypeAssert), then push the appropriate
frame.

On `Token::CloseBracket`: pop the frame, construct the corresponding AST node
(`Expr::Dict`, `Expr::Call`, `Expr::Fn`, `Expr::TypeAlias`,
`Expr::TypeAssert`), and append to the enclosing frame's expression buffer.

Between brackets: parse atoms (literals, EscapedRefs, Identifiers), access chains
(dot and bracket), and annotations — all non-recursive, driven by the current
token type.

**Annotated bare words** (`word@annotation`, compound-atomic in pest) are
handled at the lexer level: when `@` follows an `Identifier` token with no
whitespace gap (detected via span offset comparison, the same mechanism as
`BracketAccess`), the lexer emits `Token::ImmediateAt` instead of
`Token::At`. The parser treats `ImmediateAt` as the annotation separator in
annotated bare-word context.

`MAX_DEPTH` is checked on `stack.len()` before each push. This fires before
any allocation, replacing the post-hoc depth check in the current builder.

Static constraints are enforced inline rather than post-hoc:
- Duplicate key detection during `Dict` frame entry collection
- Variadic param rules during `Fn` frame param collection

Error messages carry bracket context:

```
error: unclosed bracket
  --> input.llt:5:3
   |
 5 | [x: [y: $z]
   |      ^ this bracket is not closed
```

**Comment attachment**: the iterative parser collects `Token::Comment` tokens
and returns them alongside the AST in a `ParseOutput` struct:

```rust
pub struct ParseOutput {
    pub file: Spanned<File>,
    pub leading_comments: BTreeMap<usize, Vec<String>>,  // keyed by span.start.offset
    pub trailing_comments: BTreeMap<usize, String>,       // keyed by span.start.offset
}
```

Leading comments (appearing before an AST node) are keyed by the
`span.start.offset` of the node they precede. Trailing comments (appearing
on the same line after a value) are keyed by the `span.start.offset` of the
node they follow. The formatter looks up both maps for each node it emits.

`Spanned<T>` is completely unchanged — no new fields, no broken `PartialEq`,
no memory overhead in evaluator or type-checker paths. The evaluator and type
checker receive `Spanned<File>` as before; only the formatter consumes the
comment maps.

`parse(source: &str) -> Result<ParseOutput, ParseError>`. There is no parser
selection parameter — the iterative parser replaces pest entirely (see
§Phased Adoption). The corpus test suite serves as the regression suite: if
all corpus tests pass with the new parser, it is correct.

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
  subtree, same policy as today but computed from node structure.

Trade-off: the rewritten formatter requires a successful parse. Files with
syntax errors cannot be formatted — the formatter returns an error. This is
the correct trade-off: the formatter now signals "this file has a syntax error"
rather than silently producing output for structurally invalid input. The
iterative parser's error recovery (Phase 4) partially restores this: a file
with recoverable syntax errors produces a partial AST that can be partially
formatted.

Semicolons continue to be normalized: the AST represents the canonical form,
which uses only whitespace and newlines, never semicolons.

## What Would Change

### Lexer (`src/lexer.rs`)

**Current:** emits `Token::OpenBracket` for both `$a[0]` and `$a [0]`.
No `ImmediateAt` token exists.

**Proposed:** emits `Token::BracketAccess` for no-whitespace `[` after a
value-ending token; `Token::OpenBracket` otherwise. Emits `Token::ImmediateAt`
for `@` with no whitespace after an `Identifier` token.

**Impact:** Minor — two new token variants, updated in all match sites.

### Parser (`src/parser.rs`, replaced)

**Current:** pest-based, `src/grammar.pest` + `src/parser.rs` builders.

**Proposed:** complete replacement with `Vec<StackFrame>` main loop, lexer
token consumption, `ParseOutput` comment maps, static constraint enforcement,
bracket-level error recovery (Phase 4). Pest and the grammar file are removed.

**Impact:** Major — full replacement. The corpus test suite is the regression
suite; all tests must pass before the switch lands.

### Formatter (`src/formatter.rs`)

**Current:** token-stream walker with four `has_whitespace_between` call sites,
`is_fn_params` heuristic, keyword string comparisons.

**Proposed:** complete rewrite as AST walker over `ParseOutput`. Comment maps
drive placement; AST node types drive structure.

**Impact:** Major — full rewrite. Behavior is observably equivalent for valid
files. For files with syntax errors, the formatter returns an error. Error
nodes (`Expr::Error(Span)`) are rendered by emitting the original source text
for the span verbatim, preserving partial formatting capability.

### Parse Output (`src/parser.rs`)

**Current:** `parse()` returns `Result<Spanned<File>, ParseError>`.

**Proposed:** `parse()` returns `Result<ParseOutput, ParseError>` where
`ParseOutput` carries `Spanned<File>` alongside two `BTreeMap<usize, _>`
comment tables. `Spanned<T>` is entirely unchanged — no new fields, no impact
on `PartialEq`, no overhead in evaluator or type-checker code paths.

**Impact:** Minor — callers of `parse()` unwrap `ParseOutput.file` for the
AST; the formatter additionally consumes the comment maps. The evaluator and
type checker are unaffected.

**Compatibility constraint**: the `(level, slot)` de Bruijn annotation planned
in `doc/whatif/arena-patterns.md` Phase 1 annotates `VarRef` nodes in the AST
as a separate post-parse pass. The comment maps are an independent side channel;
both coexist with no conflict.

### Pest Dependency (`Cargo.toml`)

**Current:** `pest` and `pest_derive` are build dependencies.

**Proposed:** removed in Phase 2 (immediate replacement).

**Impact:** Minor — build simplification, reduced dependency surface.

## Phased Adoption

### Phase 1: Lexer — `BracketAccess` and `ImmediateAt` Tokens

Add `Token::BracketAccess` and `Token::ImmediateAt` to `src/lexer.rs`. Update
the existing formatter to use `BracketAccess` (replacing the four
`has_whitespace_between` call sites). The pest parser does not consume lexer
tokens, so it is unaffected. The iterative parser in Phase 2 depends on both
new tokens.

This phase is independently useful: the formatter loses four ad-hoc span
comparisons and the lexer accurately reflects source structure.

### Phase 2: Parser Replacement

Replace `src/parser.rs` and `src/grammar.pest` with the iterative parser.
Remove `pest` and `pest_derive` from `Cargo.toml`. The new `parse()` returns
`ParseOutput { file, leading_comments, trailing_comments }`. All corpus tests
must pass before this lands — they are the regression suite.

Benchmark parse time on large inputs. Remove the 64 MB worker thread stack
workaround (also targeted by `iterative-eval`; coordinate the removal to avoid
a double-landing).

### Phase 3: AST-Based Formatter

Rewrite `src/formatter.rs` to walk `ParseOutput`. Depends on Phase 2 (the
new parser must produce the `ParseOutput` comment maps). The existing
`format_source()` API signature is preserved; the implementation changes.

Error nodes (`Expr::Error(Span)`) are rendered by emitting the original source
text for the span verbatim.

Verify: all existing formatter corpus tests (48 cases) pass with identical
output for valid inputs.

### Phase 4: Error Recovery

Extend the iterative parser with bracket-level error recovery. On a syntax
error inside a bracket form, skip tokens until the matching `]`, emit an
`Expr::Error(Span)` node, and continue parsing. Multiple errors are collected
and reported together.

### Prerequisites

- Phase 1 has no prerequisites — it is a standalone lexer change.
- Phase 2 requires Phase 1 (`BracketAccess`, `ImmediateAt` tokens).
- Phase 3 requires Phase 2 (`ParseOutput` comment maps from the new parser).
- Phase 4 requires Phase 2 (error recovery extends the iterative parser loop).

### Relationship to `iterative-eval`

The `iterative-eval` sprint (see TODO.md) replaces the recursive evaluator
with a CEK machine using an explicit continuation stack — the same structural
pattern applied to the evaluator rather than the parser. The two sprints are
independent: the parser produces `ParseOutput`; the evaluator consumes
`Spanned<File>`. They do not share types or implementation.

The one concrete compatibility point: `doc/whatif/arena-patterns.md` Phase 1
plans a variable resolution pass that annotates `VarRef` nodes in the AST
with `(level, slot)` de Bruijn pairs for flat environment lookup. This is a
separate post-parse pass; the `ParseOutput` comment maps are an independent
side channel. Both coexist with no conflict.

### Trigger

- When the security finding for stack overflow on deeply-nested input (TODO
  line 444) is prioritized for remediation.
- When error message quality becomes a blocker for user adoption or debugging.
- When the formatter `is_fn_params` heuristic (TODO line 1107) causes a
  real misformat that can't be fixed without AST context.
- When the `iterative-eval` sprint begins — both sprints remove the 64 MB
  stack workaround, and coordinating the removal avoids a double-landing.

## References

- **pest** (2018). PEG parser library for Rust. — Current tinct parser
  implementation. Compound-atomic rules (`${}`) are the mechanism for
  whitespace-sensitive access chains in `src/grammar.pest`.
- Nystrom, R. (2021). *Crafting Interpreters*. Genever Benning. — Reference
  implementation for hand-written recursive descent and Pratt parsers;
  Chapter 16 covers bytecode compilers, Chapter 17 closures. The iterative
  parser design draws on the pattern of making control flow explicit.
- Pratt, V.R. (1973). "Top down operator precedence." In *POPL '73*, pp.
  41–51. ACM. — Top-down operator precedence parsing; the `BracketAccess`
  token distinction maps to Pratt's notion of token context (nud vs. led
  positions). tinct's bracket grammar does not use Pratt's algorithm directly
  but the token-context principle is the same.
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
  syntax errors. Foundation for Phase 4 bracket-level error recovery.
