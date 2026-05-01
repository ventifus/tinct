//! Iterative parser for tinct.
//!
//! Hand-written recursive-descent parser that replaced the pest-based parser in sprint parser-core-c3.
//! Implements all language constructs: call/fn/type-alias/type-assert forms, keyed entries,
//! bracket access, range access, dot access chains, document separators, comment collection,
//! and fn param list parsing (simple, annotated, variadic).
//!
//! The parser enforces a maximum nesting depth of 256 brackets to prevent stack overflow.
//! Unlike the previous pest parser (which recursed on Rust's call stack), this implementation
//! uses an explicit stack for bracket nesting, making it safe for deeply nested inputs.

use std::collections::BTreeMap;

use crate::ast::*;
use crate::lexer::{self, Token};

/// Error returned when parsing fails, including message and optional source location.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub span: Option<Span>,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(span) = &self.span {
            write!(
                f,
                "{}:{}: {}",
                span.start.line, span.start.column, self.message
            )
        } else {
            write!(f, "{}", self.message)
        }
    }
}

impl std::error::Error for ParseError {}

/// Maximum nesting depth for bracket expressions (enforced before allocation).
const MAX_PARSE_DEPTH: usize = 256;

/// Helper: peek at the next significant (non-whitespace, non-newline, non-comment) token.
fn peek_next_significant<'a>(
    tokens: &'a [Spanned<Token>],
    current_index: usize,
) -> Option<(&'a Token, usize)> {
    let mut idx = current_index + 1;
    while idx < tokens.len() {
        match &tokens[idx].node {
            Token::Newline | Token::Semicolon | Token::Comment(_) => {
                idx += 1;
            }
            token => return Some((token, idx)),
        }
    }
    None
}

/// Helper: peek at the next horizontally adjacent token (skip comments and semicolons, but NOT newlines).
/// Used for keyword-colon lookahead where `[call\n: x]` must NOT be classified as a dict entry.
fn peek_next_horizontal<'a>(
    tokens: &'a [Spanned<Token>],
    current_index: usize,
) -> Option<(&'a Token, usize)> {
    let mut idx = current_index + 1;
    while idx < tokens.len() {
        match &tokens[idx].node {
            Token::Semicolon | Token::Comment(_) => {
                idx += 1;
            }
            token => return Some((token, idx)),
        }
    }
    None
}

/// Extract a comparable string from a key expression for duplicate detection.
/// Returns None for complex expressions where comparison isn't meaningful.
///
/// Parse-time duplicate detection is literal-keys-only; computed keys (DotAccess,
/// BracketAccess, Call) return None here and are checked at eval-time.
fn key_to_string(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Str(s) => Some(s.clone()),
        Expr::Int(n) => Some(n.to_string()),
        Expr::Float(n) => Some(n.to_string()),
        Expr::Bool(b) => Some(b.to_string()),
        Expr::VarRef(name) => Some(format!("${name}")),
        _ => None,
    }
}

/// Helper: count how many whitespace/newline/semicolon tokens to skip from the current position.
/// Also collects comment tokens into the leading_comments map (keyed by the next non-whitespace token's offset).
fn skip_whitespace_tokens(
    tokens: &[Spanned<Token>],
    current_index: usize,
    leading_comments: &mut BTreeMap<usize, Vec<String>>,
) -> usize {
    let mut count = 0;
    let mut idx = current_index;
    let mut collected_comments = Vec::new();

    while idx < tokens.len() {
        match &tokens[idx].node {
            Token::Comment(text) => {
                collected_comments.push(text.clone());
                count += 1;
                idx += 1;
            }
            Token::Newline | Token::Semicolon => {
                count += 1;
                idx += 1;
            }
            _ => {
                // Found non-whitespace token — attach collected comments to it
                if !collected_comments.is_empty() && idx < tokens.len() {
                    let next_offset = tokens[idx].span.start.offset;
                    leading_comments
                        .entry(next_offset)
                        .or_insert_with(Vec::new)
                        .extend(collected_comments);
                }
                break;
            }
        }
    }
    count
}

/// Adjust a single `Position` from sub-source coordinates to absolute coordinates.
///
/// When `parse_annotation` re-parses a bracket sub-string via `parse2`, the resulting spans
/// have offsets relative to the start of the sub-string (i.e., offset 0 = start of `[`).
/// This function shifts a position back into the original file's coordinate space.
///
/// `base` is the absolute `Position` of the first character of the sub-source (the `[`).
/// In sub-source coordinates that character is at offset=0, line=1, column=1.
fn adjust_position(pos: Position, base: Position) -> Position {
    Position {
        offset: pos.offset + base.offset,
        line: pos.line + base.line - 1,
        // Column is relative to its own line. Only the first sub-source line shares a line
        // with content that came before `[`, so we add base.column-1 only for that line.
        column: if pos.line == 1 {
            pos.column + base.column - 1
        } else {
            pos.column
        },
    }
}

/// Adjust a `Span` from sub-source coordinates to absolute coordinates.
fn adjust_span(span: Span, base: Position) -> Span {
    Span {
        start: adjust_position(span.start, base),
        end: adjust_position(span.end, base),
    }
}

/// Recursively adjust all spans in a `Spanned<Expr>` from sub-source to absolute coordinates.
fn adjust_spanned_expr(se: Spanned<Expr>, base: Position) -> Spanned<Expr> {
    Spanned {
        span: adjust_span(se.span, base),
        node: adjust_expr(se.node, base),
    }
}

/// Recursively adjust all spans in an `Expr`.
fn adjust_expr(expr: Expr, base: Position) -> Expr {
    match expr {
        // Leaf nodes — no nested spans
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Str(_)
        | Expr::VarRef(_)
        | Expr::Rest(_) => expr,

        Expr::DotAccess { expr, field } => Expr::DotAccess {
            expr: Box::new(adjust_spanned_expr(*expr, base)),
            field,
        },
        Expr::BracketAccess { expr, key } => Expr::BracketAccess {
            expr: Box::new(adjust_spanned_expr(*expr, base)),
            key: Box::new(adjust_spanned_expr(*key, base)),
        },
        Expr::RangeAccess { expr, start, end } => Expr::RangeAccess {
            expr: Box::new(adjust_spanned_expr(*expr, base)),
            start: start.map(|s| Box::new(adjust_spanned_expr(*s, base))),
            end: end.map(|e| Box::new(adjust_spanned_expr(*e, base))),
        },
        Expr::Dict(entries) => Expr::Dict(adjust_entries(entries, base)),
        Expr::Call {
            func,
            args,
            named_args,
        } => Expr::Call {
            func: Box::new(adjust_spanned_expr(*func, base)),
            args: args
                .into_iter()
                .map(|a| adjust_spanned_expr(a, base))
                .collect(),
            named_args: named_args
                .into_iter()
                .map(|na| Spanned {
                    span: adjust_span(na.span, base),
                    node: NamedArg {
                        name: na.node.name,
                        value: adjust_spanned_expr(na.node.value, base),
                    },
                })
                .collect(),
        },
        Expr::Fn {
            return_ann,
            params,
            body,
            desugared,
        } => Expr::Fn {
            return_ann: return_ann.map(|ra| Spanned {
                span: adjust_span(ra.span, base),
                node: adjust_annotation(ra.node, base),
            }),
            params: params
                .into_iter()
                .map(|p| Spanned {
                    span: adjust_span(p.span, base),
                    node: Param {
                        name: p.node.name,
                        annotation: p.node.annotation.map(|ann| Spanned {
                            span: adjust_span(ann.span, base),
                            node: adjust_annotation(ann.node, base),
                        }),
                        variadic: p.node.variadic,
                    },
                })
                .collect(),
            body: Box::new(adjust_spanned_expr(*body, base)),
            desugared,
        },
        Expr::TypeAlias(inner) => Expr::TypeAlias(Box::new(adjust_spanned_expr(*inner, base))),
        Expr::TypeAssert {
            annotation,
            expr,
            resolved_type,
        } => Expr::TypeAssert {
            annotation: Spanned {
                span: adjust_span(annotation.span, base),
                node: adjust_annotation(annotation.node, base),
            },
            expr: Box::new(adjust_spanned_expr(*expr, base)),
            resolved_type,
        },
        Expr::Annotated { name, annotation } => Expr::Annotated {
            name,
            annotation: Spanned {
                span: adjust_span(annotation.span, base),
                node: adjust_annotation(annotation.node, base),
            },
        },
    }
}

/// Recursively adjust all spans in an `Annotation`.
fn adjust_annotation(ann: Annotation, base: Position) -> Annotation {
    match ann {
        Annotation::Simple(_) => ann,
        Annotation::PropertyDict(entries) => {
            Annotation::PropertyDict(adjust_entries(entries, base))
        }
    }
}

/// Adjust all spans in a list of `Spanned<Entry>` from sub-source to absolute coordinates.
fn adjust_entries(entries: Vec<Spanned<Entry>>, base: Position) -> Vec<Spanned<Entry>> {
    entries
        .into_iter()
        .map(|se| Spanned {
            span: adjust_span(se.span, base),
            node: Entry {
                key: se.node.key.map(|k| adjust_spanned_expr(k, base)),
                value: adjust_spanned_expr(se.node.value, base),
            },
        })
        .collect()
}

/// Parse an annotation starting from the given token index (which should be At or ImmediateAt).
/// Returns (Annotation, next_index) on success.
///
/// Supports both simple annotations (`@Number`) and property dict annotations (`@[type: Number default: 0]`).
/// Property dict annotations are parsed by extracting the bracket sub-string from `input` and
/// re-parsing it as a standalone expression via `parse2`, then converting the resulting
/// `Expr::Dict` into `Annotation::PropertyDict`.
fn parse_annotation(
    tokens: &[Spanned<Token>],
    start_index: usize,
    leading_comments: &mut BTreeMap<usize, Vec<String>>,
    input: &str,
) -> Result<(Spanned<Annotation>, usize), ParseError> {
    let mut i = start_index;

    // Skip the @ token
    match &tokens[i].node {
        Token::At | Token::ImmediateAt => {
            i += 1;
        }
        _ => {
            return Err(ParseError {
                message: "expected @ or @@ to start annotation".to_string(),
                span: Some(tokens[i].span),
            });
        }
    }

    // Skip whitespace after @
    i += skip_whitespace_tokens(tokens, i, leading_comments);

    if i >= tokens.len() {
        return Err(ParseError {
            message: "unexpected end of input after @".to_string(),
            span: None,
        });
    }

    let ann_token = &tokens[i];

    match &ann_token.node {
        Token::BareWord(name) => {
            // Simple annotation: @Number, @a, etc.
            let annotation = Annotation::Simple(name.clone());
            Ok((Spanned::new(annotation, ann_token.span), i + 1))
        }
        Token::OpenBracket => {
            // Property dict annotation: @[key: value ...]
            // Find the matching CloseBracket by tracking nesting depth.
            let bracket_start = i;
            let bracket_start_span = tokens[bracket_start].span;
            let mut depth: usize = 0;
            let mut end_i = bracket_start;
            let mut found = false;
            for j in bracket_start..tokens.len() {
                match &tokens[j].node {
                    Token::OpenBracket | Token::BracketAccess => depth += 1,
                    Token::CloseBracket => {
                        depth -= 1;
                        if depth == 0 {
                            end_i = j;
                            found = true;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if !found {
                return Err(ParseError {
                    message: "unclosed bracket in property dict annotation".to_string(),
                    span: Some(bracket_start_span),
                });
            }

            let ann_span = Span {
                start: bracket_start_span.start,
                end: tokens[end_i].span.end,
            };

            // Extract the source sub-string for this bracket expression.
            // Token spans use byte offsets into `input`.
            let byte_start = bracket_start_span.start.offset;
            let byte_end = tokens[end_i].span.end.offset;
            let sub_source = &input[byte_start..byte_end];

            // Re-parse the sub-string as a standalone expression.
            let sub_output = parse2(sub_source).map_err(|e| ParseError {
                message: format!("error in property dict annotation: {}", e.message),
                span: Some(ann_span),
            })?;

            // Extract the first expression from the first document.
            let first_expr = sub_output
                .file
                .node
                .documents
                .into_iter()
                .next()
                .and_then(|doc| doc.node.expressions.into_iter().next());

            match first_expr {
                Some(spanned_expr) => match spanned_expr.node {
                    Expr::Dict(entries) => {
                        // The sub-parse produced spans relative to sub_source (offset 0 = `[`).
                        // Adjust all entry spans back to the original file's coordinate space.
                        let base = bracket_start_span.start;
                        let adjusted = adjust_entries(entries, base);
                        Ok((
                            Spanned::new(Annotation::PropertyDict(adjusted), ann_span),
                            end_i + 1,
                        ))
                    }
                    other => Err(ParseError {
                        message: format!(
                            "property dict annotation must be a dict expression, got: {other}"
                        ),
                        span: Some(ann_span),
                    }),
                },
                None => {
                    // Empty bracket: @[] — treat as empty PropertyDict
                    Ok((
                        Spanned::new(Annotation::PropertyDict(vec![]), ann_span),
                        end_i + 1,
                    ))
                }
            }
        }
        _ => Err(ParseError {
            message: format!(
                "expected annotation name or bracket dict after @, found {:?}",
                ann_token.node
            ),
            span: Some(ann_token.span),
        }),
    }
}

/// Parse a function parameter list: `[param1 param2@Type ...rest]`
/// Expects the current token to be OpenBracket. Advances index past CloseBracket.
/// Returns (Vec<Param>, next_index) on success.
fn parse_param_list(
    tokens: &[Spanned<Token>],
    i: &mut usize,
    leading_comments: &mut BTreeMap<usize, Vec<String>>,
    input: &str,
) -> Result<Vec<Spanned<Param>>, ParseError> {
    let _param_list_start = *i;

    // Consume OpenBracket
    if !matches!(&tokens[*i].node, Token::OpenBracket) {
        return Err(ParseError {
            message: "expected '[' to start param list".to_string(),
            span: Some(tokens[*i].span),
        });
    }
    *i += 1;

    let mut params = Vec::new();
    let mut saw_variadic = false;

    loop {
        *i += skip_whitespace_tokens(tokens, *i, leading_comments);

        if *i >= tokens.len() {
            return Err(ParseError {
                message: "unexpected end of input in param list".to_string(),
                span: None,
            });
        }

        match &tokens[*i].node {
            Token::CloseBracket => {
                *i += 1; // Consume CloseBracket
                break;
            }
            Token::Ellipsis => {
                // Variadic param: ...name
                if saw_variadic {
                    return Err(ParseError {
                        message: "multiple variadic parameters".to_string(),
                        span: Some(tokens[*i].span),
                    });
                }
                saw_variadic = true;
                let ellipsis_span = tokens[*i].span;
                *i += 1;

                *i += skip_whitespace_tokens(tokens, *i, leading_comments);

                if *i >= tokens.len() {
                    return Err(ParseError {
                        message: "expected parameter name after '...'".to_string(),
                        span: Some(ellipsis_span),
                    });
                }

                match &tokens[*i].node {
                    Token::BareWord(name) => {
                        let param_span = Span {
                            start: ellipsis_span.start,
                            end: tokens[*i].span.end,
                        };
                        params.push(Spanned::new(
                            Param {
                                name: name.clone(),
                                annotation: None,
                                variadic: true,
                            },
                            param_span,
                        ));
                        *i += 1;

                        // Check for illegal annotation on variadic param
                        if *i < tokens.len() && matches!(&tokens[*i].node, Token::ImmediateAt) {
                            return Err(ParseError {
                                message: "annotations on variadic parameters are not allowed"
                                    .to_string(),
                                span: Some(tokens[*i].span),
                            });
                        }
                    }
                    _ => {
                        return Err(ParseError {
                            message: "expected parameter name after '...'".to_string(),
                            span: Some(tokens[*i].span),
                        });
                    }
                }
            }
            Token::BareWord(name) => {
                if saw_variadic {
                    return Err(ParseError {
                        message: "parameter after variadic parameter".to_string(),
                        span: Some(tokens[*i].span),
                    });
                }

                let param_start_span = tokens[*i].span;
                let param_name = name.clone();
                *i += 1;

                // Check for annotation: param@Type or param@[type: ...]
                let annotation =
                    if *i < tokens.len() && matches!(&tokens[*i].node, Token::ImmediateAt) {
                        let (ann, next_i) = parse_annotation(tokens, *i, leading_comments, input)?;
                        *i = next_i;
                        Some(ann)
                    } else {
                        None
                    };

                let param_span = if let Some(ref ann) = annotation {
                    Span {
                        start: param_start_span.start,
                        end: ann.span.end,
                    }
                } else {
                    param_start_span
                };

                params.push(Spanned::new(
                    Param {
                        name: param_name,
                        annotation,
                        variadic: false,
                    },
                    param_span,
                ));
            }
            _ => {
                return Err(ParseError {
                    message: format!("unexpected token in param list: {:?}", tokens[*i].node),
                    span: Some(tokens[*i].span),
                });
            }
        }
    }

    Ok(params)
}

/// Stack frame types for the iterative parser.
///
/// Each variant corresponds to a bracket-form being parsed. The parser pushes
/// a frame on `Token::OpenBracket` or `Token::BracketAccess`, collects entries/args/params
/// during iteration, then pops and constructs the AST node on `Token::CloseBracket`.
#[derive(Debug, Clone)]
enum StackFrame {
    /// Dictionary literal: `[key: value ...]`
    Dict {
        entries: Vec<Entry>,
        /// Pending key from a BareWord/QuotedString/VarRef before a colon
        pending_key: Option<Spanned<Expr>>,
        /// Track seen keys for duplicate detection (literal keys only)
        seen_keys: std::collections::HashSet<String>,
        span_start: usize,
    },
    /// Function call: `[call $func arg1 arg2 name: val]`
    Call {
        args: Vec<CallArg>,
        /// Pending key for named args (BareWord before colon)
        pending_key: Option<(String, Span)>,
        span_start: usize,
    },
    /// Function definition: `[fn [params] body]` or `[fn@Type [params] body]`
    Fn {
        /// Parameter list — parsed from `[fn [x y] body]` syntax
        params: Vec<Spanned<Param>>,
        body: Option<Spanned<Expr>>,
        return_ann: Option<Spanned<Annotation>>,
        span_start: usize,
    },
    /// Type alias: `[type expr]`
    TypeAlias {
        type_expr: Option<Spanned<Expr>>,
        span_start: usize,
    },
    /// Type assertion: `[@Annotation expr]`
    TypeAssert {
        annotation: Option<Spanned<Annotation>>,
        expr: Option<Spanned<Expr>>,
        span_start: usize,
    },
    /// Bracket access key: `$a[key_expr]` where `key_expr` may contain nested brackets
    /// Also handles range access: `$a[2..5]`
    BracketAccessKey {
        target: Spanned<Expr>,
        /// The first expression before `..` (if range) or the full key (if not range)
        key_expr: Option<Spanned<Expr>>,
        /// Set to true if we've seen a `..` token, making this a range access
        is_range: bool,
        /// The expression after `..` (only for range access)
        range_end: Option<Spanned<Expr>>,
        span_start: usize,
    },
}

/// Intermediate representation for call arguments (positional or named).
///
/// During call parsing, arguments are collected in order. The evaluator later
/// enforces the C-PRIORITY binding order (see `doc/04-functions.md §Call Convention`).
#[derive(Debug, Clone, PartialEq)]
enum CallArg {
    Positional(Spanned<Expr>),
    Named(String, Spanned<Expr>),
}

/// Parse output: AST plus comment side-channels for the formatter.
///
/// `leading_comments` are keyed by the `span.start.offset` of the node they precede.
/// `trailing_comments` are keyed by the `span.start.offset` of the node they follow.
///
/// The evaluator and type checker consume only `file`; the formatter uses all three fields.
///
/// External pipeline consumers that don't need comments should access `.file` directly.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseOutput {
    pub file: Spanned<File>,
    pub leading_comments: BTreeMap<usize, Vec<String>>,
    pub trailing_comments: BTreeMap<usize, String>,
}

/// Parse tinct source text using the iterative parser.
///
/// This is the main entry point for Phase 2c-1 (complete feature set). The parser handles:
/// - Basic literals: `Int`, `Float`, `BoolLit`, `QuotedString`, `BareWord`, `VarRef`
/// - Dicts: `[]`, `[42]`, `[a: 1 b: 2]`, keyed and auto-indexed entries
/// - Call forms: `[call $f arg1 arg2 name: val]`
/// - Fn forms: `[fn [x y@Int ...rest] body]`, `[fn@Type [params] body]` with full param parsing
/// - Type-alias: `[type expr]`
/// - Type-assert: `[@Annotation expr]`
/// - Bracket access: `$a[0]`, `$a[$key]`
/// - Dot access chains: `$a.b.c`, `$a.b[0]`
/// - Range access: `$a[2..5]`, `$a[..5]`, `$a[2..]`, `$a[..]`
/// - Document separators: `---` between document sections
/// - Comment collection: leading and trailing comments attached by span offset
///
/// Not yet implemented (deferred to parser-core-c2):
/// - Annotated bare words as dict values (`word@SimpleType`)
/// - Corpus parity tests with pest parser
///
/// NOTE: When parser-core-c2 lands, `parse()` in parser.rs will be replaced by this function.
/// All pipeline entry points (eval_source, typecheck_source, REPL, LSP) will unwrap `.file`
/// from `ParseOutput`.
pub fn parse2(input: &str) -> Result<ParseOutput, ParseError> {
    // Tokenize the input via the lexer
    let tokens = lexer::tokenize(input).map_err(|e| ParseError {
        message: e.message,
        span: Some(e.span),
    })?;

    // Stack of frames tracking bracket nesting
    let mut stack: Vec<StackFrame> = Vec::new();

    // Current document being built (one or more expressions)
    let mut current_document_expressions: Vec<Spanned<Expr>> = Vec::new();

    // All documents in the file
    let mut documents: Vec<Spanned<Document>> = Vec::new();

    // Comment maps
    let mut leading_comments: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut trailing_comments: BTreeMap<usize, String> = BTreeMap::new();

    // Track the span of the last significant token for trailing comment detection
    let mut last_significant_span: Option<Span> = None;

    // Convert to index-based iteration for peeking
    let token_vec = tokens;
    let mut i = 0;

    while i < token_vec.len() {
        let spanned_token = &token_vec[i];
        let token = &spanned_token.node;
        let span = spanned_token.span;

        match token {
            Token::OpenBracket => {
                // Check depth before pushing
                if stack.len() >= MAX_PARSE_DEPTH {
                    return Err(ParseError {
                        message: format!(
                            "maximum nesting depth exceeded (limit: {MAX_PARSE_DEPTH})"
                        ),
                        span: Some(span),
                    });
                }

                // Peek at next non-whitespace/non-newline token for form classification
                let next_token = peek_next_significant(&token_vec, i);

                match next_token {
                    Some((Token::BareWord(s), keyword_idx))
                        if s == "call"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Call form: [call $func args...]
                        // (Not a call form if the keyword is followed by colon: [call: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::Call {
                            args: Vec::new(),
                            pending_key: None,
                            span_start: span.start.offset,
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "call" token
                        i += skip_whitespace_tokens(&token_vec, i, &mut leading_comments);
                        i += 1;
                        continue;
                    }
                    Some((Token::BareWord(s), keyword_idx))
                        if s == "fn"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Fn form: [fn [params] body] or [fn@RetType [params] body]
                        // (Not a fn form if the keyword is followed by colon: [fn: x] is a dict.)
                        // (depth already checked above)

                        i += 1; // Consume the OpenBracket
                        i += skip_whitespace_tokens(&token_vec, i, &mut leading_comments);
                        i += 1; // Consume the "fn" token

                        // Check for return annotation: fn@RetType
                        let return_ann = if i < token_vec.len()
                            && matches!(&token_vec[i].node, Token::ImmediateAt)
                        {
                            let (ann, next_i) =
                                parse_annotation(&token_vec, i, &mut leading_comments, input)?;
                            i = next_i;
                            Some(ann)
                        } else {
                            None
                        };

                        // Parse param list if present: [fn [params] body]
                        i += skip_whitespace_tokens(&token_vec, i, &mut leading_comments);
                        let params = if i < token_vec.len()
                            && matches!(&token_vec[i].node, Token::OpenBracket)
                        {
                            parse_param_list(&token_vec, &mut i, &mut leading_comments, input)?
                        } else {
                            Vec::new()
                        };

                        stack.push(StackFrame::Fn {
                            params,
                            body: None,
                            return_ann,
                            span_start: span.start.offset,
                        });
                        continue;
                    }
                    Some((Token::BareWord(s), keyword_idx))
                        if s == "type"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Type-alias form: [type expr]
                        // (Not a type form if the keyword is followed by colon: [type: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::TypeAlias {
                            type_expr: None,
                            span_start: span.start.offset,
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "type" token
                        i += skip_whitespace_tokens(&token_vec, i, &mut leading_comments);
                        i += 1;
                        continue;
                    }
                    Some((Token::At, _)) | Some((Token::ImmediateAt, _)) => {
                        // Type-assert form: [@Annotation expr]
                        // (depth already checked above)
                        stack.push(StackFrame::TypeAssert {
                            annotation: None,
                            expr: None,
                            span_start: span.start.offset,
                        });
                        i += 1; // Consume the OpenBracket
                        continue;
                    }
                    _ => {
                        // Default: dict literal
                        // (depth already checked above)
                        stack.push(StackFrame::Dict {
                            entries: Vec::new(),
                            pending_key: None,
                            seen_keys: std::collections::HashSet::new(),
                            span_start: span.start.offset,
                        });
                        i += 1;
                        continue;
                    }
                }
            }

            Token::BracketAccess => {
                // Check depth before pushing
                if stack.len() >= MAX_PARSE_DEPTH {
                    return Err(ParseError {
                        message: format!(
                            "maximum nesting depth exceeded (limit: {MAX_PARSE_DEPTH})"
                        ),
                        span: Some(span),
                    });
                }

                // Pop the target expression from the current context (document or frame)
                let target = if stack.is_empty() {
                    if current_document_expressions.is_empty() {
                        return Err(ParseError {
                            message: "bracket access requires a target expression before '['"
                                .to_string(),
                            span: Some(span),
                        });
                    }
                    current_document_expressions.pop().unwrap()
                } else {
                    pop_last_value_from_frame(&mut stack, span)?
                };

                stack.push(StackFrame::BracketAccessKey {
                    target,
                    key_expr: None,
                    is_range: false,
                    range_end: None,
                    span_start: span.start.offset,
                });
                i += 1;
                continue;
            }

            Token::CloseBracket => {
                // Pop the frame and construct the AST node
                let frame = stack.pop().ok_or_else(|| ParseError {
                    message: "unmatched closing bracket".to_string(),
                    span: Some(span),
                })?;

                let dict_span = |span_start: usize| Span {
                    start: Position {
                        offset: span_start,
                        line: 1, // TODO(parser-core-c): proper line tracking from lexer tokens
                        column: 1,
                    },
                    end: span.end,
                };

                match frame {
                    StackFrame::Dict {
                        entries,
                        pending_key,
                        seen_keys: _,
                        span_start,
                    } => {
                        // If there's a pending key, that's an error — key without value
                        if let Some(key_expr) = pending_key {
                            return Err(ParseError {
                                message: "key without value: expected `:` and value".to_string(),
                                span: Some(key_expr.span),
                            });
                        }

                        // Construct the dict expression
                        let dict_expr = Expr::Dict(
                            entries
                                .into_iter()
                                .map(|e| {
                                    let entry_span = if let Some(ref key) = e.key {
                                        Span {
                                            start: key.span.start,
                                            end: e.value.span.end,
                                        }
                                    } else {
                                        e.value.span
                                    };
                                    Spanned::new(e, entry_span)
                                })
                                .collect(),
                        );

                        let spanned_dict = Spanned::new(dict_expr, dict_span(span_start));

                        // Push to parent or document (via push_value to handle pending_key)
                        push_value(&mut stack, &mut current_document_expressions, spanned_dict)?;
                    }

                    StackFrame::Call {
                        args,
                        pending_key,
                        span_start,
                    } => {
                        // If there's a pending key, that's an error — named arg without value
                        if let Some((key, key_span)) = pending_key {
                            return Err(ParseError {
                                message: format!("named argument `{}` without value", key),
                                span: Some(key_span),
                            });
                        }

                        // First arg is the function, rest are arguments
                        if args.is_empty() {
                            return Err(ParseError {
                                message: "call form requires at least a function expression"
                                    .to_string(),
                                span: Some(span),
                            });
                        }

                        let func = match &args[0] {
                            CallArg::Positional(expr) => expr.clone(),
                            CallArg::Named(name, _) => {
                                return Err(ParseError {
                                    message: format!(
                                        "call function cannot be a named argument (got `{name}:`)",
                                    ),
                                    span: Some(span),
                                });
                            }
                        };

                        let mut positional_args = Vec::new();
                        let mut named_args = Vec::new();

                        for arg in args.into_iter().skip(1) {
                            match arg {
                                CallArg::Positional(expr) => positional_args.push(expr),
                                CallArg::Named(name, expr) => {
                                    named_args.push(Spanned::new(
                                        NamedArg {
                                            name,
                                            value: expr.clone(),
                                        },
                                        expr.span,
                                    ));
                                }
                            }
                        }

                        let call_expr = Expr::Call {
                            func: Box::new(func),
                            args: positional_args,
                            named_args,
                        };

                        let spanned_call = Spanned::new(call_expr, dict_span(span_start));
                        push_value(&mut stack, &mut current_document_expressions, spanned_call)?;
                    }

                    StackFrame::Fn {
                        params,
                        body,
                        return_ann,
                        span_start,
                    } => {
                        // Fn form: [fn [params] body]
                        let body = body.ok_or_else(|| ParseError {
                            message: "fn form requires a body expression".to_string(),
                            span: Some(span),
                        })?;

                        let fn_expr = Expr::Fn {
                            return_ann,
                            params,
                            body: Box::new(body),
                            desugared: false,
                        };

                        let spanned_fn = Spanned::new(fn_expr, dict_span(span_start));
                        push_value(&mut stack, &mut current_document_expressions, spanned_fn)?;
                    }

                    StackFrame::TypeAlias {
                        type_expr,
                        span_start,
                    } => {
                        let type_expr = type_expr.ok_or_else(|| ParseError {
                            message: "type-alias form requires a type expression".to_string(),
                            span: Some(span),
                        })?;

                        let alias_expr = Expr::TypeAlias(Box::new(type_expr));
                        let spanned_alias = Spanned::new(alias_expr, dict_span(span_start));
                        push_value(&mut stack, &mut current_document_expressions, spanned_alias)?;
                    }

                    StackFrame::TypeAssert {
                        annotation,
                        expr,
                        span_start,
                    } => {
                        let annotation = annotation.ok_or_else(|| ParseError {
                            message: "type-assert form requires an annotation".to_string(),
                            span: Some(span),
                        })?;
                        let expr = expr.ok_or_else(|| ParseError {
                            message: "type-assert form requires an expression".to_string(),
                            span: Some(span),
                        })?;

                        use std::cell::RefCell;
                        let type_assert_expr = Expr::TypeAssert {
                            annotation,
                            expr: Box::new(expr),
                            resolved_type: RefCell::new(None),
                        };

                        let spanned_type_assert =
                            Spanned::new(type_assert_expr, dict_span(span_start));
                        push_value(
                            &mut stack,
                            &mut current_document_expressions,
                            spanned_type_assert,
                        )?;
                    }

                    StackFrame::BracketAccessKey {
                        target,
                        key_expr,
                        is_range,
                        range_end,
                        span_start,
                    } => {
                        if is_range {
                            // Range access: $a[start..end]
                            let range_access_expr = Expr::RangeAccess {
                                expr: Box::new(target),
                                start: key_expr.map(Box::new),
                                end: range_end.map(Box::new),
                            };

                            let spanned_access =
                                Spanned::new(range_access_expr, dict_span(span_start));
                            push_value(
                                &mut stack,
                                &mut current_document_expressions,
                                spanned_access,
                            )?;
                        } else {
                            // Regular bracket access: $a[key]
                            let key = key_expr.ok_or_else(|| ParseError {
                                message: "bracket access requires a key expression".to_string(),
                                span: Some(span),
                            })?;

                            let bracket_access_expr = Expr::BracketAccess {
                                expr: Box::new(target),
                                key: Box::new(key),
                            };

                            let spanned_access =
                                Spanned::new(bracket_access_expr, dict_span(span_start));
                            push_value(
                                &mut stack,
                                &mut current_document_expressions,
                                spanned_access,
                            )?;
                        }
                    }
                }

                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::Colon => {
                // Key-value separator
                match stack.last_mut() {
                    Some(StackFrame::Dict {
                        ref mut pending_key,
                        ..
                    }) => {
                        if pending_key.is_none() {
                            return Err(ParseError {
                                message: "`:` without a key (expected key before `:`)".to_string(),
                                span: Some(span),
                            });
                        }
                        // Pending key is set; next expression will be the value
                    }
                    Some(StackFrame::Call {
                        ref mut pending_key,
                        ..
                    }) => {
                        if pending_key.is_none() {
                            return Err(ParseError {
                                message: "`:` without a name (expected bare word before `:` for named arg)".to_string(),
                                span: Some(span),
                            });
                        }
                        // Pending key is set; next expression will be the value
                    }
                    _ => {
                        return Err(ParseError {
                            message: "`:` can only appear in dict or call forms".to_string(),
                            span: Some(span),
                        });
                    }
                }
                i += 1;
                continue;
            }

            // Literals: collect as values, but detect colon-ahead for dict key position.
            Token::Int(n) => {
                let expr = Spanned::new(Expr::Int(*n), span);
                // Check if this integer is a potential dict key (e.g. [0: $x])
                if let Some((Token::Colon, _)) = peek_next_significant(&token_vec, i) {
                    if let Some(StackFrame::Dict {
                        ref mut pending_key,
                        ..
                    }) = stack.last_mut()
                    {
                        *pending_key = Some(expr.clone());
                        last_significant_span = Some(span);
                        i += 1;
                        continue;
                    }
                }
                push_value(&mut stack, &mut current_document_expressions, expr)?;
                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::Float(f) => {
                let expr = Spanned::new(Expr::Float(*f), span);
                push_value(&mut stack, &mut current_document_expressions, expr)?;
                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::BoolLit(b) => {
                let expr = Spanned::new(Expr::Bool(*b), span);
                push_value(&mut stack, &mut current_document_expressions, expr)?;
                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::QuotedString(s) => {
                let expr = Spanned::new(Expr::Str(s.clone()), span);
                // Check if this quoted string is a potential dict key (e.g. ["key": value])
                if let Some((Token::Colon, _)) = peek_next_significant(&token_vec, i) {
                    if let Some(StackFrame::Dict {
                        ref mut pending_key,
                        ..
                    }) = stack.last_mut()
                    {
                        *pending_key = Some(expr.clone());
                        last_significant_span = Some(span);
                        i += 1;
                        continue;
                    }
                }
                push_value(&mut stack, &mut current_document_expressions, expr)?;
                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::BareWord(s) => {
                // Check for annotation: word@Type
                if i + 1 < token_vec.len() && matches!(&token_vec[i + 1].node, Token::ImmediateAt) {
                    // Annotated bare word
                    let name = s.clone();
                    let name_span = span;
                    i += 1; // Move to ImmediateAt token
                    let (annotation, next_i) =
                        parse_annotation(&token_vec, i, &mut leading_comments, input)?;
                    i = next_i;
                    let full_span = Span {
                        start: name_span.start,
                        end: annotation.span.end,
                    };
                    let expr = Spanned::new(Expr::Annotated { name, annotation }, full_span);
                    push_value(&mut stack, &mut current_document_expressions, expr)?;
                    last_significant_span = Some(full_span);
                    continue;
                }

                let expr = Spanned::new(Expr::Str(s.clone()), span);
                // Check if this is a potential key (next token is colon)
                if let Some((Token::Colon, _)) = peek_next_significant(&token_vec, i) {
                    // This bare word is a key candidate
                    match stack.last_mut() {
                        Some(StackFrame::Dict {
                            ref mut pending_key,
                            ..
                        }) => {
                            *pending_key = Some(expr.clone());
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        Some(StackFrame::Call {
                            ref mut pending_key,
                            args: _,
                            ..
                        }) => {
                            // Named arg key — store the string name
                            *pending_key = Some((s.clone(), span));
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        _ => {
                            // Not in dict/call context; treat as normal value
                            push_value(&mut stack, &mut current_document_expressions, expr)?;
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                    }
                } else {
                    // Not followed by colon; regular value
                    push_value(&mut stack, &mut current_document_expressions, expr)?;
                    last_significant_span = Some(span);
                    i += 1;
                    continue;
                }
            }

            Token::VarRef(name) => {
                let expr = Spanned::new(Expr::VarRef(name.clone()), span);
                // Check if this VarRef is a potential dict key (followed by colon)
                if let Some((Token::Colon, _)) = peek_next_significant(&token_vec, i) {
                    if let Some(StackFrame::Dict {
                        ref mut pending_key,
                        ..
                    }) = stack.last_mut()
                    {
                        *pending_key = Some(expr.clone());
                        last_significant_span = Some(span);
                        i += 1;
                        continue;
                    }
                }
                push_value(&mut stack, &mut current_document_expressions, expr)?;
                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            // Other tokens: deferred to later sprints or ignored
            Token::Comment(comment_text) => {
                // Determine if this is a trailing or leading comment based on line position
                // Trailing: comment on the same line as the previous significant token
                // Leading: comment on a different line (or no previous token)
                if let Some(prev_span) = last_significant_span {
                    if prev_span.start.line == span.start.line {
                        // Same line as previous token → trailing comment
                        trailing_comments.insert(prev_span.start.offset, comment_text.clone());
                    } else {
                        // Different line → leading comment for next token
                        if let Some((_, next_idx)) = peek_next_significant(&token_vec, i) {
                            let next_offset = token_vec[next_idx].span.start.offset;
                            leading_comments
                                .entry(next_offset)
                                .or_insert_with(Vec::new)
                                .push(comment_text.clone());
                        }
                    }
                } else {
                    // No previous token → leading comment for next token
                    if let Some((_, next_idx)) = peek_next_significant(&token_vec, i) {
                        let next_offset = token_vec[next_idx].span.start.offset;
                        leading_comments
                            .entry(next_offset)
                            .or_insert_with(Vec::new)
                            .push(comment_text.clone());
                    }
                }

                i += 1;
                continue;
            }

            Token::Newline | Token::Semicolon => {
                // Whitespace/separators — ignored
                i += 1;
                continue;
            }

            Token::DocSeparator => {
                // Document separator: finalize current document and start a new one
                if !stack.is_empty() {
                    return Err(ParseError {
                        message: "document separator cannot appear inside bracket expressions"
                            .to_string(),
                        span: Some(span),
                    });
                }

                // Finalize current document (even if empty)
                let exprs = std::mem::take(&mut current_document_expressions);
                let doc_span = if exprs.is_empty() {
                    // Empty document: use separator position
                    span
                } else {
                    Span {
                        start: exprs.first().unwrap().span.start,
                        end: exprs.last().unwrap().span.end,
                    }
                };
                documents.push(Spanned::new(Document { expressions: exprs }, doc_span));

                i += 1;
                continue;
            }

            Token::Dot => {
                // Dot access: pop the preceding expression and create DotAccess
                // Pop from the current context (document or frame)
                let target = if stack.is_empty() {
                    if current_document_expressions.is_empty() {
                        return Err(ParseError {
                            message: "dot access requires a target expression before '.'"
                                .to_string(),
                            span: Some(span),
                        });
                    }
                    current_document_expressions.pop().unwrap()
                } else {
                    // Inside a frame — pop the last value from the current frame
                    pop_last_value_from_frame(&mut stack, span)?
                };

                i += 1; // Consume the Dot

                // Skip whitespace
                i += skip_whitespace_tokens(&token_vec, i, &mut leading_comments);

                if i >= token_vec.len() {
                    return Err(ParseError {
                        message: "expected field name after '.'".to_string(),
                        span: Some(span),
                    });
                }

                // Next token must be a BareWord for the field name
                match &token_vec[i].node {
                    Token::BareWord(field) => {
                        let field_name = field.clone();
                        let dot_access_span = Span {
                            start: target.span.start,
                            end: token_vec[i].span.end,
                        };

                        let dot_access_expr = Expr::DotAccess {
                            expr: Box::new(target),
                            field: field_name,
                        };

                        let spanned_access = Spanned::new(dot_access_expr, dot_access_span);

                        if stack.is_empty() {
                            current_document_expressions.push(spanned_access);
                        } else {
                            push_value(
                                &mut stack,
                                &mut current_document_expressions,
                                spanned_access,
                            )?;
                        }

                        i += 1;
                        continue;
                    }
                    _ => {
                        return Err(ParseError {
                            message: format!(
                                "expected field name (bare word) after '.', found {:?}",
                                token_vec[i].node
                            ),
                            span: Some(token_vec[i].span),
                        });
                    }
                }
            }

            Token::Range => {
                // Range operator: must be inside a BracketAccessKey frame
                match stack.last_mut() {
                    Some(StackFrame::BracketAccessKey {
                        ref mut is_range, ..
                    }) => {
                        *is_range = true;
                        i += 1;
                        continue;
                    }
                    _ => {
                        return Err(ParseError {
                            message:
                                "range operator '..' can only appear in bracket access context"
                                    .to_string(),
                            span: Some(span),
                        });
                    }
                }
            }

            Token::At | Token::ImmediateAt => {
                // Check context: if we're in a TypeAssert frame and don't have annotation yet, parse it
                match stack.last_mut() {
                    Some(StackFrame::TypeAssert {
                        ref mut annotation, ..
                    }) if annotation.is_none() => {
                        // Parse the annotation
                        let (ann, next_i) =
                            parse_annotation(&token_vec, i, &mut leading_comments, input)?;
                        *annotation = Some(ann);
                        i = next_i;
                        continue;
                    }
                    _ => {
                        return Err(ParseError {
                            message: "@ annotations outside type-assert or param contexts not yet supported".to_string(),
                            span: Some(span),
                        });
                    }
                }
            }

            Token::Ellipsis => {
                // Rest/open-row marker: `...` or `...name` inside a dict expression.
                // Only valid inside a Dict frame (type expression context).
                // Produces Expr::Rest(None) for anonymous open row, Expr::Rest(Some(name)) for named.
                if let Some(StackFrame::Dict { .. }) = stack.last() {
                    let ellipsis_span = span;
                    i += 1; // Consume ellipsis
                    i += skip_whitespace_tokens(&token_vec, i, &mut leading_comments);
                    // Check for optional name after ...
                    let (rest_name, rest_end) = if i < token_vec.len() {
                        match &token_vec[i].node {
                            Token::BareWord(name) => {
                                let n = name.clone();
                                let end_span = token_vec[i].span;
                                (
                                    Some(n),
                                    Span {
                                        start: ellipsis_span.start,
                                        end: end_span.end,
                                    },
                                )
                            }
                            _ => (None, ellipsis_span),
                        }
                    } else {
                        (None, ellipsis_span)
                    };
                    let name_advance = if rest_name.is_some() { 1 } else { 0 };
                    let rest_expr = Spanned::new(Expr::Rest(rest_name), rest_end);
                    push_value(&mut stack, &mut current_document_expressions, rest_expr)?;
                    last_significant_span = Some(rest_end);
                    i += name_advance;
                    continue;
                } else {
                    return Err(ParseError {
                        message: "variadic/rest markers not yet supported outside dict context"
                            .to_string(),
                        span: Some(span),
                    });
                }
            }
        }
    }

    // Check for unclosed brackets
    if !stack.is_empty() {
        // Get the innermost unclosed bracket's position
        let innermost_frame = stack.last().unwrap();
        let bracket_offset = match innermost_frame {
            StackFrame::Dict { span_start, .. } => *span_start,
            StackFrame::Call { span_start, .. } => *span_start,
            StackFrame::Fn { span_start, .. } => *span_start,
            StackFrame::TypeAlias { span_start, .. } => *span_start,
            StackFrame::TypeAssert { span_start, .. } => *span_start,
            StackFrame::BracketAccessKey { span_start, .. } => *span_start,
        };

        // Convert offset to line/column
        // Build line starts table from input.
        // Recognizes LF (\n), CRLF (\r\n), and bare CR (\r) as line endings.
        // CRLF counts as one line ending (the \r is consumed with the \n that follows).
        let mut line_starts = vec![0usize];
        let input_bytes = input.as_bytes();
        let mut bi = 0;
        while bi < input_bytes.len() {
            match input_bytes[bi] {
                b'\r' if bi + 1 < input_bytes.len() && input_bytes[bi + 1] == b'\n' => {
                    // CRLF: one line ending, advance past both bytes
                    bi += 2;
                    line_starts.push(bi);
                }
                b'\r' => {
                    // Bare CR (Mac Classic): one line ending
                    bi += 1;
                    line_starts.push(bi);
                }
                b'\n' => {
                    // LF
                    bi += 1;
                    line_starts.push(bi);
                }
                _ => {
                    bi += 1;
                }
            }
        }

        // Binary search to find the line for bracket_offset
        let line_index = match line_starts.binary_search(&bracket_offset) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let start_pos = Position {
            offset: bracket_offset,
            line: line_index + 1,
            column: bracket_offset - line_starts[line_index] + 1,
        };

        let unclosed_span = Span {
            start: start_pos,
            end: Position {
                offset: bracket_offset + 1,
                line: start_pos.line,
                column: start_pos.column + 1,
            },
        };

        let count = stack.len();
        let message = if count == 1 {
            "unclosed bracket".to_string()
        } else {
            format!("{} unclosed brackets", count)
        };

        return Err(ParseError {
            message,
            span: Some(unclosed_span),
        });
    }

    // Build the final file
    // If no expressions, create one empty document
    if current_document_expressions.is_empty() && documents.is_empty() {
        let doc = Document {
            expressions: vec![],
        };
        let doc_span = Span {
            start: Position {
                offset: 0,
                line: 1,
                column: 1,
            },
            end: Position {
                offset: input.len(),
                line: 1,
                column: 1,
            },
        };
        documents.push(Spanned::new(doc, doc_span));
    } else if !current_document_expressions.is_empty() {
        // Finalize current document
        let doc = Document {
            expressions: current_document_expressions,
        };
        let doc_span = Span {
            start: Position {
                offset: 0,
                line: 1,
                column: 1,
            },
            end: Position {
                offset: input.len(),
                line: 1,
                column: 1,
            },
        };
        documents.push(Spanned::new(doc, doc_span));
    }

    let file = File { documents };
    let file_span = Span {
        start: Position {
            offset: 0,
            line: 1,
            column: 1,
        },
        end: Position {
            offset: input.len(),
            line: 1,
            column: 1,
        },
    };

    Ok(ParseOutput {
        file: Spanned::new(file, file_span),
        leading_comments,
        trailing_comments,
    })
}

/// Helper: pop the last value from the current frame for postfix operator transformation.
/// This is used by dot access and other postfix operators that need to retroactively
/// transform the previously-pushed expression.
///
/// Note: For Dict frames, this pops the entire entry and returns just the value. The caller
/// must re-push the transformed value, which will create a new entry (either keyed or auto-indexed
/// depending on whether there was a pending_key).
fn pop_last_value_from_frame(
    stack: &mut Vec<StackFrame>,
    span: Span,
) -> Result<Spanned<Expr>, ParseError> {
    match stack.last_mut() {
        Some(StackFrame::Dict {
            ref mut entries,
            ref mut pending_key,
            ref mut seen_keys,
            ..
        }) => {
            // Check if there's a pending key first - if so, we haven't pushed the value yet
            // and there's nothing to pop
            if pending_key.is_some() {
                return Err(ParseError {
                    message: "dot access requires a target before '.'".to_string(),
                    span: Some(span),
                });
            }
            if entries.is_empty() {
                return Err(ParseError {
                    message: "dot access requires a target before '.'".to_string(),
                    span: Some(span),
                });
            }
            let last_entry = entries.pop().unwrap();
            // Restore the key as pending_key so the transformed value will be re-associated
            // with the same key. Also remove it from seen_keys so that when push_value
            // re-inserts the completed entry it doesn't trigger a false duplicate key error.
            if let Some(ref key_expr) = last_entry.key {
                if let Some(key_str) = key_to_string(&key_expr.node) {
                    seen_keys.remove(&key_str);
                }
            }
            *pending_key = last_entry.key;
            Ok(last_entry.value)
        }
        Some(StackFrame::Call { ref mut args, .. }) => {
            if args.is_empty() {
                return Err(ParseError {
                    message: "dot access requires a target before '.'".to_string(),
                    span: Some(span),
                });
            }
            match args.pop().unwrap() {
                CallArg::Positional(expr) => Ok(expr),
                CallArg::Named(name, _expr) => Err(ParseError {
                    message: format!("dot access cannot operate on named argument '{}'", name),
                    span: Some(span),
                }),
            }
        }
        Some(StackFrame::Fn { ref mut body, .. }) => {
            if let Some(b) = body.take() {
                Ok(b)
            } else {
                Err(ParseError {
                    message: "dot access requires a target before '.'".to_string(),
                    span: Some(span),
                })
            }
        }
        Some(StackFrame::BracketAccessKey {
            ref mut key_expr,
            ref mut range_end,
            ref is_range,
            ..
        }) => {
            if *is_range {
                if let Some(end) = range_end.take() {
                    Ok(end)
                } else if let Some(start) = key_expr.take() {
                    Ok(start)
                } else {
                    Err(ParseError {
                        message: "dot access requires a target before '.'".to_string(),
                        span: Some(span),
                    })
                }
            } else {
                if let Some(key) = key_expr.take() {
                    Ok(key)
                } else {
                    Err(ParseError {
                        message: "dot access requires a target before '.'".to_string(),
                        span: Some(span),
                    })
                }
            }
        }
        Some(StackFrame::TypeAlias {
            ref mut type_expr, ..
        }) => {
            if let Some(expr) = type_expr.take() {
                Ok(expr)
            } else {
                Err(ParseError {
                    message: "dot access requires a target before '.'".to_string(),
                    span: Some(span),
                })
            }
        }
        Some(StackFrame::TypeAssert { ref mut expr, .. }) => {
            if let Some(e) = expr.take() {
                Ok(e)
            } else {
                Err(ParseError {
                    message: "dot access requires a target before '.'".to_string(),
                    span: Some(span),
                })
            }
        }
        None => Err(ParseError {
            message: "dot access requires a target before '.'".to_string(),
            span: Some(span),
        }),
    }
}

/// Helper: push an expression to the parent frame or current document.
fn push_expr_to_parent(
    stack: &mut Vec<StackFrame>,
    current_document_expressions: &mut Vec<Spanned<Expr>>,
    expr: Spanned<Expr>,
) -> Result<(), ParseError> {
    if stack.is_empty() {
        current_document_expressions.push(expr);
        Ok(())
    } else {
        match stack.last_mut() {
            Some(StackFrame::Dict {
                ref mut entries, ..
            }) => {
                entries.push(Entry {
                    key: None,
                    value: expr,
                });
                Ok(())
            }
            Some(StackFrame::Call { ref mut args, .. }) => {
                args.push(CallArg::Positional(expr));
                Ok(())
            }
            Some(StackFrame::Fn { ref mut body, .. }) => {
                if body.is_some() {
                    return Err(ParseError {
                        message: "fn form can only have one body expression".to_string(),
                        span: Some(expr.span),
                    });
                }
                *body = Some(expr);
                Ok(())
            }
            Some(StackFrame::TypeAlias {
                ref mut type_expr, ..
            }) => {
                if type_expr.is_some() {
                    return Err(ParseError {
                        message: "type-alias form can only have one type expression".to_string(),
                        span: Some(expr.span),
                    });
                }
                *type_expr = Some(expr);
                Ok(())
            }
            Some(StackFrame::TypeAssert {
                expr: ref mut type_assert_expr,
                ..
            }) => {
                if type_assert_expr.is_some() {
                    return Err(ParseError {
                        message: "type-assert form can only have one expression".to_string(),
                        span: Some(expr.span),
                    });
                }
                *type_assert_expr = Some(expr);
                Ok(())
            }
            Some(StackFrame::BracketAccessKey {
                ref mut key_expr,
                ref is_range,
                ref mut range_end,
                ..
            }) => {
                if *is_range {
                    // We've seen .., so this expression is the end of the range
                    if range_end.is_some() {
                        return Err(ParseError {
                            message: "range access can only have one expression after '..'"
                                .to_string(),
                            span: Some(expr.span),
                        });
                    }
                    *range_end = Some(expr);
                } else {
                    // No .. yet, so this is the key or range start
                    if key_expr.is_some() {
                        return Err(ParseError {
                            message:
                                "bracket access can only have one key expression before '..' or ']'"
                                    .to_string(),
                            span: Some(expr.span),
                        });
                    }
                    *key_expr = Some(expr);
                }
                Ok(())
            }
            None => unreachable!("stack.is_empty() was false but last_mut returned None"),
        }
    }
}

/// Helper: push a value expression, handling keyed entries in dict/call contexts.
fn push_value(
    stack: &mut Vec<StackFrame>,
    current_document_expressions: &mut Vec<Spanned<Expr>>,
    expr: Spanned<Expr>,
) -> Result<(), ParseError> {
    if stack.is_empty() {
        current_document_expressions.push(expr);
        return Ok(());
    }

    match stack.last_mut() {
        Some(StackFrame::Dict {
            ref mut entries,
            ref mut pending_key,
            ref mut seen_keys,
            ..
        }) => {
            if let Some(key) = pending_key.take() {
                // Check for duplicate key (literal keys only)
                if let Some(key_str) = key_to_string(&key.node) {
                    if seen_keys.contains(&key_str) {
                        return Err(ParseError {
                            message: format!("duplicate key \"{}\"", key_str),
                            span: Some(key.span),
                        });
                    }
                    seen_keys.insert(key_str);
                }
                // This value completes a keyed entry
                entries.push(Entry {
                    key: Some(key),
                    value: expr,
                });
            } else {
                // Auto-indexed entry
                entries.push(Entry {
                    key: None,
                    value: expr,
                });
            }
            Ok(())
        }
        Some(StackFrame::Call {
            ref mut args,
            ref mut pending_key,
            ..
        }) => {
            if let Some((name, _)) = pending_key.take() {
                // This value completes a named argument
                args.push(CallArg::Named(name, expr));
            } else {
                // Positional argument
                args.push(CallArg::Positional(expr));
            }
            Ok(())
        }
        _ => {
            // All other frames: delegate to push_expr_to_parent
            push_expr_to_parent(stack, current_document_expressions, expr)
        }
    }
}

/// Parse tinct source text and return the AST.
///
/// This is a compatibility wrapper around `parse2()` that extracts just the AST file,
/// discarding comment information. Most pipeline entry points (eval_source, typecheck_source,
/// REPL, LSP) use this function.
///
/// For advanced use cases (like the formatter) that need comment preservation, use `parse2()`
/// directly and access the `ParseOutput.leading_comments` and `ParseOutput.trailing_comments` maps.
pub fn parse(input: &str) -> Result<Spanned<File>, ParseError> {
    let output = parse2(input)?;
    Ok(output.file)
}

/// Parse a single tinct expression.
///
/// This is a convenience wrapper that parses the input and returns the first expression
/// from the first document. If the input is empty or has no expressions, returns an error.
///
/// Primarily used for testing and corpus validation where single-expression inputs are common.
pub fn parse_expression(input: &str) -> Result<Spanned<Expr>, ParseError> {
    let file = parse(input)?;

    if file.node.documents.is_empty() {
        return Err(ParseError {
            message: "no documents in input".to_string(),
            span: None,
        });
    }

    let first_doc = &file.node.documents[0];
    if first_doc.node.expressions.is_empty() {
        return Err(ParseError {
            message: "no expressions in first document".to_string(),
            span: Some(first_doc.span),
        });
    }

    Ok(first_doc.node.expressions[0].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: parse successfully and return the first expression from the first document.
    fn parse_expr(input: &str) -> Spanned<Expr> {
        let output = parse2(input).expect("parse failed");
        output.file.node.documents[0].node.expressions[0].clone()
    }

    #[test]
    fn test_empty_dict() {
        let output = parse2("[]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        assert_eq!(doc.expressions.len(), 1);
        match &doc.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 0);
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_dict_one_value() {
        let output = parse2("[42]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        assert_eq!(doc.expressions.len(), 1);
        match &doc.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(entries[0].node.key.is_none()); // auto-indexed
                assert!(matches!(&entries[0].node.value.node, Expr::Int(42)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_keyed_entry() {
        let output = parse2("[a: 1 b: 2]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 2);
                // First entry: a: 1
                assert!(entries[0].node.key.is_some());
                match &entries[0].node.key.as_ref().unwrap().node {
                    Expr::Str(s) => assert_eq!(s, "a"),
                    other => panic!("expected key 'a', got {other:?}"),
                }
                assert!(matches!(&entries[0].node.value.node, Expr::Int(1)));
                // Second entry: b: 2
                assert!(entries[1].node.key.is_some());
                match &entries[1].node.key.as_ref().unwrap().node {
                    Expr::Str(s) => assert_eq!(s, "b"),
                    other => panic!("expected key 'b', got {other:?}"),
                }
                assert!(matches!(&entries[1].node.value.node, Expr::Int(2)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_call_simple() {
        let output = parse2("[call $f 1 2]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Call {
                func,
                args,
                named_args,
            } => {
                match &func.node {
                    Expr::VarRef(name) => assert_eq!(name, "f"),
                    other => panic!("expected VarRef for func, got {other:?}"),
                }
                assert_eq!(args.len(), 2);
                assert!(matches!(&args[0].node, Expr::Int(1)));
                assert!(matches!(&args[1].node, Expr::Int(2)));
                assert_eq!(named_args.len(), 0);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn test_call_named_args() {
        let output = parse2("[call $f x: 1]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Call {
                func,
                args,
                named_args,
            } => {
                match &func.node {
                    Expr::VarRef(name) => assert_eq!(name, "f"),
                    other => panic!("expected VarRef for func, got {other:?}"),
                }
                assert_eq!(args.len(), 0);
                assert_eq!(named_args.len(), 1);
                assert_eq!(named_args[0].node.name, "x");
                assert!(matches!(&named_args[0].node.value.node, Expr::Int(1)));
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_simple() {
        let output = parse2("[fn 42]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Fn {
                params,
                body,
                return_ann,
                desugared,
            } => {
                assert_eq!(params.len(), 0);
                assert!(matches!(&body.node, Expr::Int(42)));
                assert!(return_ann.is_none());
                assert_eq!(*desugared, false);
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_type_alias() {
        let output = parse2("[type 42]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::TypeAlias(inner) => {
                assert!(matches!(&inner.node, Expr::Int(42)));
            }
            other => panic!("expected TypeAlias, got {other:?}"),
        }
    }

    #[test]
    fn test_literal_int() {
        let expr = parse_expr("42");
        assert!(matches!(expr.node, Expr::Int(42)));
    }

    #[test]
    fn test_depth_limit() {
        let mut input = String::new();
        for _ in 0..MAX_PARSE_DEPTH {
            input.push('[');
        }
        input.push('['); // One more to exceed
        for _ in 0..=MAX_PARSE_DEPTH {
            input.push(']');
        }

        let err = parse2(&input).unwrap_err();
        assert!(err.message.contains("maximum nesting depth exceeded"));
    }

    #[test]
    fn test_type_assert_simple() {
        let output = parse2("[@Number 42]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::TypeAssert {
                annotation, expr, ..
            } => {
                match &annotation.node {
                    Annotation::Simple(name) => assert_eq!(name, "Number"),
                    other => panic!("expected Simple annotation, got {other:?}"),
                }
                assert!(matches!(&expr.node, Expr::Int(42)));
            }
            other => panic!("expected TypeAssert, got {other:?}"),
        }
    }

    #[test]
    fn test_bracket_access_literal_key() {
        let output = parse2("$a[0]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::BracketAccess { expr, key } => {
                match &expr.node {
                    Expr::VarRef(name) => assert_eq!(name, "a"),
                    other => panic!("expected VarRef, got {other:?}"),
                }
                assert!(matches!(&key.node, Expr::Int(0)));
            }
            other => panic!("expected BracketAccess, got {other:?}"),
        }
    }

    #[test]
    fn test_bracket_access_variable_key() {
        let output = parse2("$a[$key]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::BracketAccess { expr, key } => {
                match &expr.node {
                    Expr::VarRef(name) => assert_eq!(name, "a"),
                    other => panic!("expected VarRef, got {other:?}"),
                }
                match &key.node {
                    Expr::VarRef(name) => assert_eq!(name, "key"),
                    other => panic!("expected VarRef key, got {other:?}"),
                }
            }
            other => panic!("expected BracketAccess, got {other:?}"),
        }
    }

    // --- Error path tests ---

    #[test]
    fn test_call_empty() {
        let err = parse2("[call]").unwrap_err();
        assert!(
            err.message.contains("call form requires"),
            "expected error about call form requiring a function, got: {}",
            err.message
        );
    }

    #[test]
    fn test_call_func_as_named_arg() {
        // [call f: $x] — first arg is Named("f", ...) which is forbidden as func
        let err = parse2("[call f: $x]").unwrap_err();
        assert!(
            err.message.contains("named argument"),
            "expected error about named argument, got: {}",
            err.message
        );
    }

    #[test]
    fn test_dict_pending_key_no_value() {
        // [a:] — key with no value before closing bracket
        let err = parse2("[a:]").unwrap_err();
        assert!(
            err.message.contains("key without value"),
            "expected 'key without value' error, got: {}",
            err.message
        );
    }

    #[test]
    fn test_call_pending_named_arg_no_value() {
        // [call $f x:] — named arg x with no value before closing bracket
        let err = parse2("[call $f x:]").unwrap_err();
        assert!(
            err.message.contains("without value"),
            "expected 'without value' error for named arg, got: {}",
            err.message
        );
    }

    #[test]
    fn test_type_alias_empty() {
        let err = parse2("[type]").unwrap_err();
        assert!(
            err.message.contains("type-alias form requires"),
            "expected error about type-alias requiring a type expression, got: {}",
            err.message
        );
    }

    #[test]
    fn test_type_assert_no_annotation() {
        // [@] — type-assert with @; parse_annotation sees CloseBracket after @ → error
        let err = parse2("[@]").unwrap_err();
        assert!(
            err.message
                .contains("expected annotation name or bracket dict after @"),
            "expected error about invalid annotation token, got: {}",
            err.message
        );
    }

    #[test]
    fn test_type_assert_no_expr() {
        // [@Number] — annotation parsed, but no expression
        let err = parse2("[@Number]").unwrap_err();
        assert!(
            err.message
                .contains("type-assert form requires an expression"),
            "expected error about missing expression, got: {}",
            err.message
        );
    }

    #[test]
    fn test_bracket_access_empty() {
        // $a[] — bracket access with empty key
        let err = parse2("$a[]").unwrap_err();
        assert!(
            err.message
                .contains("bracket access requires a key expression"),
            "expected error about empty key, got: {}",
            err.message
        );
    }

    #[test]
    fn test_colon_outside_dict_call() {
        // [fn :] — because "fn" is followed by ":", it's classified as a dict (not Fn form).
        // Within the dict, "fn" is set as pending_key, then "]" arrives with no value → error.
        // The `:` can only appear in non-dict/call contexts error fires when colon appears
        // with no pending context, e.g. inside a TypeAlias frame: [type :]
        let err = parse2("[fn :]").unwrap_err();
        assert!(
            err.message.contains("key without value") || err.message.contains("`:` without a key"),
            "expected key-related error for [fn :], got: {}",
            err.message
        );
        // Also test the true "colon outside dict/call" case: colon in a TypeAlias frame
        let err2 = parse2("[type x :]").unwrap_err();
        // [type x :] — "type" is not followed by colon so Fn frame... wait "type" is followed
        // by space then "x", so TypeAlias frame is pushed. "x" is not followed by colon
        // (it's followed by space then ":"). So "x" is pushed as type_expr. Then ":" appears
        // with TypeAlias frame on stack → "`:` can only appear in dict or call forms".
        assert!(
            err2.message
                .contains("`:` can only appear in dict or call forms"),
            "expected error about colon in wrong context for [type x :], got: {}",
            err2.message
        );
    }

    #[test]
    fn test_colon_without_key_in_dict() {
        // [:] — colon with no preceding key in a dict
        let err = parse2("[:]").unwrap_err();
        assert!(
            err.message.contains("`:` without a key"),
            "expected error about colon without key, got: {}",
            err.message
        );
    }

    #[test]
    fn test_fn_multiple_bodies() {
        // [fn 1 2] — two body expressions in an fn form
        let err = parse2("[fn 1 2]").unwrap_err();
        assert!(
            err.message
                .contains("fn form can only have one body expression"),
            "expected error about multiple body expressions, got: {}",
            err.message
        );
    }

    #[test]
    fn test_type_alias_multiple_exprs() {
        // [type 1 2] — two expressions in a type-alias form
        let err = parse2("[type 1 2]").unwrap_err();
        assert!(
            err.message
                .contains("type-alias form can only have one type expression"),
            "expected error about multiple expressions, got: {}",
            err.message
        );
    }

    #[test]
    fn test_type_assert_multiple_exprs() {
        // [@Number 1 2] — two expressions in a type-assert form
        let err = parse2("[@Number 1 2]").unwrap_err();
        assert!(
            err.message
                .contains("type-assert form can only have one expression"),
            "expected error about multiple expressions, got: {}",
            err.message
        );
    }

    #[test]
    fn test_fn_empty() {
        // [fn] — fn with no body
        let err = parse2("[fn]").unwrap_err();
        assert!(
            err.message.contains("fn form requires a body expression"),
            "expected error about fn requiring a body, got: {}",
            err.message
        );
    }

    // --- Edge case / positive tests ---

    #[test]
    fn test_keyword_as_dict_key() {
        // [call: 1] — "call" followed by colon → dict, not a call form (Fix 2)
        let output = parse2("[call: 1]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                let key = entries[0].node.key.as_ref().expect("expected keyed entry");
                match &key.node {
                    Expr::Str(s) => assert_eq!(s, "call"),
                    other => panic!("expected key 'call', got {other:?}"),
                }
                assert!(matches!(&entries[0].node.value.node, Expr::Int(1)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_all_keywords_as_dict_keys() {
        // [call: 1 fn: 2 type: 3] — all three keywords as dict keys
        let output = parse2("[call: 1 fn: 2 type: 3]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 3);
                let expected_keys = ["call", "fn", "type"];
                let expected_values = [1i64, 2, 3];
                for (i, (key, val)) in expected_keys.iter().zip(expected_values.iter()).enumerate()
                {
                    let entry_key = entries[i].node.key.as_ref().expect("expected keyed entry");
                    match &entry_key.node {
                        Expr::Str(s) => assert_eq!(s.as_str(), *key),
                        other => panic!("expected key '{key}', got {other:?}"),
                    }
                    match &entries[i].node.value.node {
                        Expr::Int(n) => assert_eq!(*n, *val),
                        other => panic!("expected Int({val}), got {other:?}"),
                    }
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_whitespace_in_form_classification() {
        // "[ call $f]" — leading whitespace before keyword; peek skips it → still a Call form
        let output = parse2("[ call $f]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Call {
                func,
                args,
                named_args,
            } => {
                match &func.node {
                    Expr::VarRef(name) => assert_eq!(name, "f"),
                    other => panic!("expected VarRef for func, got {other:?}"),
                }
                assert_eq!(args.len(), 0);
                assert_eq!(named_args.len(), 0);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn test_keyed_entry_with_bracket_value() {
        // [a: [1]] — dict with keyed entry whose value is a nested dict (Fix 1)
        let output = parse2("[a: [1]]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                let key = entries[0].node.key.as_ref().expect("expected keyed entry");
                match &key.node {
                    Expr::Str(s) => assert_eq!(s, "a"),
                    other => panic!("expected key 'a', got {other:?}"),
                }
                // Value should be a Dict containing Int(1)
                match &entries[0].node.value.node {
                    Expr::Dict(inner_entries) => {
                        assert_eq!(inner_entries.len(), 1);
                        assert!(matches!(&inner_entries[0].node.value.node, Expr::Int(1)));
                    }
                    other => panic!("expected inner Dict, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_call_named_arg_bracket_value() {
        // [call $f x: [1]] — call with named arg whose value is a nested dict (Fix 1)
        let output = parse2("[call $f x: [1]]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Call {
                func,
                args,
                named_args,
            } => {
                match &func.node {
                    Expr::VarRef(name) => assert_eq!(name, "f"),
                    other => panic!("expected VarRef for func, got {other:?}"),
                }
                assert_eq!(args.len(), 0);
                assert_eq!(named_args.len(), 1);
                assert_eq!(named_args[0].node.name, "x");
                match &named_args[0].node.value.node {
                    Expr::Dict(inner_entries) => {
                        assert_eq!(inner_entries.len(), 1);
                        assert!(matches!(&inner_entries[0].node.value.node, Expr::Int(1)));
                    }
                    other => panic!("expected inner Dict for named arg value, got {other:?}"),
                }
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn test_call_only_named_args() {
        // [call $f x: 1 y: 2] — call with func and two named args, no positional
        let output = parse2("[call $f x: 1 y: 2]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Call {
                func,
                args,
                named_args,
            } => {
                match &func.node {
                    Expr::VarRef(name) => assert_eq!(name, "f"),
                    other => panic!("expected VarRef for func, got {other:?}"),
                }
                assert_eq!(args.len(), 0);
                assert_eq!(named_args.len(), 2);
                assert_eq!(named_args[0].node.name, "x");
                assert!(matches!(&named_args[0].node.value.node, Expr::Int(1)));
                assert_eq!(named_args[1].node.name, "y");
                assert!(matches!(&named_args[1].node.value.node, Expr::Int(2)));
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn test_unmatched_closing_bracket() {
        let err = parse2("]").unwrap_err();
        assert!(
            err.message.contains("unmatched closing bracket"),
            "expected 'unmatched closing bracket' error, got: {}",
            err.message
        );
    }

    #[test]
    fn test_unclosed_bracket() {
        let err = parse2("[").unwrap_err();
        assert!(
            err.message.contains("unclosed bracket"),
            "expected 'unclosed bracket' error, got: {}",
            err.message
        );
    }

    #[test]
    fn test_call_colon_without_key() {
        // [call $f :] — colon inside Call frame with pending_key=None (no preceding bare word)
        let err = parse2("[call $f :]").unwrap_err();
        assert!(
            err.message.contains("without a name"),
            "expected error about colon without a name in call frame, got: {}",
            err.message
        );
    }

    #[test]
    fn test_bracket_access_inside_dict() {
        // [a: $x[0]] — bracket access works as dict value (BracketAccess pops from frame)
        let output = parse2("[a: $x[0]]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.key.as_ref().unwrap().node {
                    Expr::Str(s) => assert_eq!(s, "a"),
                    other => panic!("expected key 'a', got {other:?}"),
                }
                match &entries[0].node.value.node {
                    Expr::BracketAccess { expr, key } => {
                        match &expr.node {
                            Expr::VarRef(name) => assert_eq!(name, "x"),
                            other => panic!("expected VarRef('x'), got {other:?}"),
                        }
                        assert!(matches!(&key.node, Expr::Int(0)));
                    }
                    other => panic!("expected BracketAccess as value, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_annotation_invalid_token() {
        // [@123] — parse_annotation receives Int(123) after @, not BareWord or OpenBracket
        let err = parse2("[@123]").unwrap_err();
        assert!(
            err.message
                .contains("expected annotation name or bracket dict after @"),
            "expected error about invalid annotation token, got: {}",
            err.message
        );
    }

    #[test]
    fn test_nested_bracket_access() {
        // $a[0][1] — second BracketAccess wraps the result of the first
        let output = parse2("$a[0][1]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::BracketAccess {
                expr: outer_expr,
                key: outer_key,
            } => {
                // outer key is Int(1)
                assert!(
                    matches!(&outer_key.node, Expr::Int(1)),
                    "expected outer key Int(1), got {:?}",
                    outer_key.node
                );
                // outer target is BracketAccess($a, 0)
                match &outer_expr.node {
                    Expr::BracketAccess {
                        expr: inner_expr,
                        key: inner_key,
                    } => {
                        match &inner_expr.node {
                            Expr::VarRef(name) => assert_eq!(name, "a"),
                            other => panic!("expected VarRef('a') as inner target, got {other:?}"),
                        }
                        assert!(
                            matches!(&inner_key.node, Expr::Int(0)),
                            "expected inner key Int(0), got {:?}",
                            inner_key.node
                        );
                    }
                    other => panic!("expected inner BracketAccess, got {other:?}"),
                }
            }
            other => panic!("expected outer BracketAccess, got {other:?}"),
        }
    }

    #[test]
    fn test_mixed_keyed_and_auto_indexed() {
        // [a: 1 2 b: 3] — keyed, auto-indexed, keyed entries
        let output = parse2("[a: 1 2 b: 3]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(
                    entries.len(),
                    3,
                    "expected 3 entries, got {}",
                    entries.len()
                );
                // Entry 0: key=Some("a"), value=1
                let key0 = entries[0]
                    .node
                    .key
                    .as_ref()
                    .expect("entry 0 should have key");
                match &key0.node {
                    Expr::Str(s) => assert_eq!(s, "a"),
                    other => panic!("expected key 'a', got {other:?}"),
                }
                assert!(matches!(&entries[0].node.value.node, Expr::Int(1)));
                // Entry 1: key=None (auto-indexed), value=2
                assert!(
                    entries[1].node.key.is_none(),
                    "entry 1 should be auto-indexed (no key)"
                );
                assert!(matches!(&entries[1].node.value.node, Expr::Int(2)));
                // Entry 2: key=Some("b"), value=3
                let key2 = entries[2]
                    .node
                    .key
                    .as_ref()
                    .expect("entry 2 should have key");
                match &key2.node {
                    Expr::Str(s) => assert_eq!(s, "b"),
                    other => panic!("expected key 'b', got {other:?}"),
                }
                assert!(matches!(&entries[2].node.value.node, Expr::Int(3)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // --- New tests for parser-core-c features ---

    #[test]
    fn test_fn_params_simple() {
        let output = parse2("[fn [x y] $x]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Fn {
                params,
                body,
                return_ann,
                desugared,
            } => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0].node.name, "x");
                assert!(params[0].node.annotation.is_none());
                assert!(!params[0].node.variadic);
                assert_eq!(params[1].node.name, "y");
                assert!(params[1].node.annotation.is_none());
                assert!(!params[1].node.variadic);
                assert!(matches!(&body.node, Expr::VarRef(name) if name == "x"));
                assert!(return_ann.is_none());
                assert!(!desugared);
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_params_annotated() {
        let output = parse2("[fn [x@Int] $x]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Fn { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "x");
                assert!(params[0].node.annotation.is_some());
                match &params[0].node.annotation.as_ref().unwrap().node {
                    Annotation::Simple(name) => assert_eq!(name, "Int"),
                    other => panic!("expected Simple annotation, got {other:?}"),
                }
                assert!(!params[0].node.variadic);
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_return_annotation() {
        let output = parse2("[fn@Number [x] $x]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Fn {
                return_ann, params, ..
            } => {
                assert!(return_ann.is_some());
                match &return_ann.as_ref().unwrap().node {
                    Annotation::Simple(name) => assert_eq!(name, "Number"),
                    other => panic!("expected Simple annotation, got {other:?}"),
                }
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "x");
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_variadic() {
        let output = parse2("[fn [...args] $args]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Fn { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "args");
                assert!(params[0].node.variadic);
                assert!(params[0].node.annotation.is_none());
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_dot_access_simple() {
        let output = parse2("$a.b").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::DotAccess { expr, field } => {
                match &expr.node {
                    Expr::VarRef(name) => assert_eq!(name, "a"),
                    other => panic!("expected VarRef, got {other:?}"),
                }
                assert_eq!(field, "b");
            }
            other => panic!("expected DotAccess, got {other:?}"),
        }
    }

    #[test]
    fn test_dot_access_chain() {
        let output = parse2("$a.b.c").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::DotAccess {
                expr: outer_expr,
                field: outer_field,
            } => {
                assert_eq!(outer_field, "c");
                match &outer_expr.node {
                    Expr::DotAccess {
                        expr: inner_expr,
                        field: inner_field,
                    } => {
                        assert_eq!(inner_field, "b");
                        match &inner_expr.node {
                            Expr::VarRef(name) => assert_eq!(name, "a"),
                            other => panic!("expected VarRef at base, got {other:?}"),
                        }
                    }
                    other => panic!("expected inner DotAccess, got {other:?}"),
                }
            }
            other => panic!("expected outer DotAccess, got {other:?}"),
        }
    }

    #[test]
    fn test_dot_access_inside_call() {
        // [call $fn $a.b]
        let output = parse2("[call $fn $a.b]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Call {
                func,
                args,
                named_args,
            } => {
                match &func.node {
                    Expr::VarRef(name) => assert_eq!(name, "fn"),
                    other => panic!("expected VarRef for func, got {other:?}"),
                }
                assert_eq!(args.len(), 1);
                match &args[0].node {
                    Expr::DotAccess { expr, field } => {
                        assert_eq!(field, "b");
                        match &expr.node {
                            Expr::VarRef(name) => assert_eq!(name, "a"),
                            other => panic!("expected VarRef, got {other:?}"),
                        }
                    }
                    other => panic!("expected DotAccess, got {other:?}"),
                }
                assert_eq!(named_args.len(), 0);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn test_dot_access_inside_dict() {
        // [x: $y.z]
        let output = parse2("[x: $y.z]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(entries[0].node.key.is_some());
                match &entries[0].node.key.as_ref().unwrap().node {
                    Expr::Str(s) => assert_eq!(s, "x"),
                    other => panic!("expected key 'x', got {other:?}"),
                }
                match &entries[0].node.value.node {
                    Expr::DotAccess { expr, field } => {
                        assert_eq!(field, "z");
                        match &expr.node {
                            Expr::VarRef(name) => assert_eq!(name, "y"),
                            other => panic!("expected VarRef, got {other:?}"),
                        }
                    }
                    other => panic!("expected DotAccess, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_range_access_both() {
        let output = parse2("$a[2..5]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::RangeAccess { expr, start, end } => {
                match &expr.node {
                    Expr::VarRef(name) => assert_eq!(name, "a"),
                    other => panic!("expected VarRef, got {other:?}"),
                }
                assert!(start.is_some());
                match &start.as_ref().unwrap().node {
                    Expr::Int(n) => assert_eq!(*n, 2),
                    other => panic!("expected Int(2) for start, got {other:?}"),
                }
                assert!(end.is_some());
                match &end.as_ref().unwrap().node {
                    Expr::Int(n) => assert_eq!(*n, 5),
                    other => panic!("expected Int(5) for end, got {other:?}"),
                }
            }
            other => panic!("expected RangeAccess, got {other:?}"),
        }
    }

    #[test]
    fn test_range_access_unbounded() {
        let output = parse2("$a[..]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::RangeAccess { expr, start, end } => {
                match &expr.node {
                    Expr::VarRef(name) => assert_eq!(name, "a"),
                    other => panic!("expected VarRef, got {other:?}"),
                }
                assert!(start.is_none());
                assert!(end.is_none());
            }
            other => panic!("expected RangeAccess, got {other:?}"),
        }
    }

    #[test]
    fn test_range_access_start_only() {
        let output = parse2("$a[2..]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::RangeAccess { expr, start, end } => {
                match &expr.node {
                    Expr::VarRef(name) => assert_eq!(name, "a"),
                    other => panic!("expected VarRef, got {other:?}"),
                }
                assert!(start.is_some());
                match &start.as_ref().unwrap().node {
                    Expr::Int(n) => assert_eq!(*n, 2),
                    other => panic!("expected Int(2) for start, got {other:?}"),
                }
                assert!(end.is_none());
            }
            other => panic!("expected RangeAccess, got {other:?}"),
        }
    }

    #[test]
    fn test_range_access_end_only() {
        let output = parse2("$a[..5]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::RangeAccess { expr, start, end } => {
                match &expr.node {
                    Expr::VarRef(name) => assert_eq!(name, "a"),
                    other => panic!("expected VarRef, got {other:?}"),
                }
                assert!(start.is_none());
                assert!(end.is_some());
                match &end.as_ref().unwrap().node {
                    Expr::Int(n) => assert_eq!(*n, 5),
                    other => panic!("expected Int(5) for end, got {other:?}"),
                }
            }
            other => panic!("expected RangeAccess, got {other:?}"),
        }
    }

    #[test]
    fn test_doc_separator() {
        let output = parse2("[a: 1]\n---\n[b: 2]").expect("parse failed");
        assert_eq!(output.file.node.documents.len(), 2);

        // First document
        let doc1 = &output.file.node.documents[0].node;
        assert_eq!(doc1.expressions.len(), 1);
        match &doc1.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.key.as_ref().unwrap().node {
                    Expr::Str(s) => assert_eq!(s, "a"),
                    other => panic!("expected key 'a', got {other:?}"),
                }
            }
            other => panic!("expected Dict in doc1, got {other:?}"),
        }

        // Second document
        let doc2 = &output.file.node.documents[1].node;
        assert_eq!(doc2.expressions.len(), 1);
        match &doc2.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.key.as_ref().unwrap().node {
                    Expr::Str(s) => assert_eq!(s, "b"),
                    other => panic!("expected key 'b', got {other:?}"),
                }
            }
            other => panic!("expected Dict in doc2, got {other:?}"),
        }
    }

    #[test]
    fn test_comment_leading() {
        let output = parse2("# comment\n[a: 1]").expect("parse failed");
        assert!(!output.leading_comments.is_empty());
        // Comments are attached by offset of next significant token
        // We just verify that we have at least one comment collected
        let has_comment = output
            .leading_comments
            .values()
            .any(|v| v.iter().any(|c| c.contains("comment")));
        assert!(
            has_comment,
            "expected to find 'comment' in leading_comments"
        );
    }

    #[test]
    fn test_fn_param_variadic_not_last() {
        // [...args x] — variadic param not last
        let err = parse2("[fn [...args x] $x]").unwrap_err();
        assert!(
            err.message.contains("parameter after variadic"),
            "expected error about param after variadic, got: {}",
            err.message
        );
    }

    #[test]
    fn test_fn_multiple_variadic() {
        // [...args ...rest] — multiple variadic params
        let err = parse2("[fn [...args ...rest] $x]").unwrap_err();
        assert!(
            err.message.contains("multiple variadic"),
            "expected error about multiple variadic params, got: {}",
            err.message
        );
    }

    #[test]
    fn test_fn_variadic_with_annotation_errors() {
        // [...args@Int] — annotation on variadic param
        let err = parse2("[fn [...args@Int] $args]").unwrap_err();
        assert!(
            err.message
                .contains("annotations on variadic parameters are not allowed"),
            "expected error about variadic annotation, got: {}",
            err.message
        );
    }

    #[test]
    fn test_range_outside_bracket_access() {
        // .. outside brackets is a bare word per lexer rules (Range only emitted inside brackets)
        let output = parse2("1..5").expect("should parse (.. is bare word outside brackets)");
        let doc = &output.file.node.documents[0].node;
        assert_eq!(doc.expressions.len(), 2); // Int(1) and BareWord("..5")
    }

    #[test]
    fn test_doc_separator_inside_bracket() {
        // --- inside a bracket expression
        let err = parse2("[---]").unwrap_err();
        assert!(
            err.message
                .contains("document separator cannot appear inside"),
            "expected error about doc separator inside bracket, got: {}",
            err.message
        );
    }

    // --- Tests added for review findings (parser-core-c1) ---

    #[test]
    fn test_whitespace_prevents_dot_access() {
        // "$a .b" has whitespace before dot; lexer emits Dot as non-access-context bare word ".b"
        let output = parse2("$a .b").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        assert_eq!(
            doc.expressions.len(),
            2,
            "expected 2 expressions (VarRef 'a' + BareWord '.b'), got {}",
            doc.expressions.len()
        );
        match &doc.expressions[0].node {
            Expr::VarRef(name) => assert_eq!(name, "a"),
            other => panic!("expected VarRef('a') as first expr, got {other:?}"),
        }
        // Second expression: the ".b" bare word (not a DotAccess)
        assert!(
            !matches!(&doc.expressions[1].node, Expr::DotAccess { .. }),
            "second expression should not be DotAccess — whitespace prevents dot access"
        );
    }

    #[test]
    fn test_whitespace_prevents_bracket_access() {
        // "$a [0]" has whitespace before "["; lexer emits OpenBracket (not BracketAccess)
        let output = parse2("$a [0]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        assert_eq!(
            doc.expressions.len(),
            2,
            "expected 2 expressions (VarRef 'a' + Dict containing Int(0)), got {}",
            doc.expressions.len()
        );
        match &doc.expressions[0].node {
            Expr::VarRef(name) => assert_eq!(name, "a"),
            other => panic!("expected VarRef('a') as first expr, got {other:?}"),
        }
        match &doc.expressions[1].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::Int(0)));
            }
            other => panic!("expected Dict([Int(0)]) as second expr, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_params_mixed() {
        // [fn [x y@Int ...rest] $x] — simple + annotated + variadic
        let output = parse2("[fn [x y@Int ...rest] $x]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Fn { params, body, .. } => {
                assert_eq!(params.len(), 3);
                // param 0: simple "x"
                assert_eq!(params[0].node.name, "x");
                assert!(params[0].node.annotation.is_none());
                assert!(!params[0].node.variadic);
                // param 1: annotated "y@Int"
                assert_eq!(params[1].node.name, "y");
                assert!(params[1].node.annotation.is_some());
                match &params[1].node.annotation.as_ref().unwrap().node {
                    Annotation::Simple(name) => assert_eq!(name, "Int"),
                    other => panic!("expected Simple(Int) annotation, got {other:?}"),
                }
                assert!(!params[1].node.variadic);
                // param 2: variadic "...rest"
                assert_eq!(params[2].node.name, "rest");
                assert!(params[2].node.variadic);
                assert!(params[2].node.annotation.is_none());
                // body
                assert!(matches!(&body.node, Expr::VarRef(name) if name == "x"));
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_both_annotations() {
        // [fn@Number [x@Int] $x] — return annotation + annotated param
        let output = parse2("[fn@Number [x@Int] $x]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Fn {
                params,
                return_ann,
                body,
                ..
            } => {
                // Return annotation
                assert!(return_ann.is_some());
                match &return_ann.as_ref().unwrap().node {
                    Annotation::Simple(name) => assert_eq!(name, "Number"),
                    other => panic!("expected Simple(Number) return annotation, got {other:?}"),
                }
                // Param
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].node.name, "x");
                assert!(params[0].node.annotation.is_some());
                match &params[0].node.annotation.as_ref().unwrap().node {
                    Annotation::Simple(name) => assert_eq!(name, "Int"),
                    other => panic!("expected Simple(Int) param annotation, got {other:?}"),
                }
                assert!(!params[0].node.variadic);
                // Body
                assert!(matches!(&body.node, Expr::VarRef(name) if name == "x"));
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_dot_access_on_dict_literal() {
        // "[x: 1].x" — dot access immediately after closing bracket (no whitespace)
        // The lexer emits Dot (not BareWord) after ']' since CloseBracket is in is_access_context.
        let output = parse2("[x: 1].x").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        assert_eq!(
            doc.expressions.len(),
            1,
            "expected 1 expression (DotAccess)"
        );
        match &doc.expressions[0].node {
            Expr::DotAccess { expr, field } => {
                assert_eq!(field, "x");
                match &expr.node {
                    Expr::Dict(entries) => {
                        assert_eq!(entries.len(), 1);
                        match &entries[0].node.key.as_ref().unwrap().node {
                            Expr::Str(s) => assert_eq!(s, "x"),
                            other => panic!("expected key 'x', got {other:?}"),
                        }
                    }
                    other => panic!("expected Dict as DotAccess target, got {other:?}"),
                }
            }
            other => panic!("expected DotAccess, got {other:?}"),
        }
    }

    #[test]
    fn test_comment_trailing() {
        // "[a: 1] # trailing comment" — comment on same line as dict → trailing
        let output = parse2("[a: 1] # trailing comment").expect("parse failed");
        assert!(
            !output.trailing_comments.is_empty(),
            "expected at least one trailing comment"
        );
        let has_comment = output
            .trailing_comments
            .values()
            .any(|c| c.contains("trailing comment"));
        assert!(
            has_comment,
            "expected to find 'trailing comment' in trailing_comments, got: {:?}",
            output.trailing_comments
        );
    }

    #[test]
    fn test_range_in_nested_context() {
        // "[x: $y[2..5]]" — range access as dict value
        let output = parse2("[x: $y[2..5]]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.key.as_ref().unwrap().node {
                    Expr::Str(s) => assert_eq!(s, "x"),
                    other => panic!("expected key 'x', got {other:?}"),
                }
                match &entries[0].node.value.node {
                    Expr::RangeAccess { expr, start, end } => {
                        match &expr.node {
                            Expr::VarRef(name) => assert_eq!(name, "y"),
                            other => panic!("expected VarRef('y'), got {other:?}"),
                        }
                        assert!(start.is_some());
                        match &start.as_ref().unwrap().node {
                            Expr::Int(n) => assert_eq!(*n, 2),
                            other => panic!("expected Int(2) for start, got {other:?}"),
                        }
                        assert!(end.is_some());
                        match &end.as_ref().unwrap().node {
                            Expr::Int(n) => assert_eq!(*n, 5),
                            other => panic!("expected Int(5) for end, got {other:?}"),
                        }
                    }
                    other => panic!("expected RangeAccess as dict value, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_multiple_doc_separators() {
        // Three documents separated by two ---
        let output = parse2("[a: 1]\n---\n[b: 2]\n---\n[c: 3]").expect("parse failed");
        assert_eq!(
            output.file.node.documents.len(),
            3,
            "expected 3 documents, got {}",
            output.file.node.documents.len()
        );

        // Document 1: [a: 1]
        let doc1 = &output.file.node.documents[0].node;
        assert_eq!(doc1.expressions.len(), 1);
        match &doc1.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.key.as_ref().unwrap().node {
                    Expr::Str(s) => assert_eq!(s, "a"),
                    other => panic!("expected key 'a' in doc1, got {other:?}"),
                }
            }
            other => panic!("expected Dict in doc1, got {other:?}"),
        }

        // Document 2: [b: 2]
        let doc2 = &output.file.node.documents[1].node;
        assert_eq!(doc2.expressions.len(), 1);
        match &doc2.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.key.as_ref().unwrap().node {
                    Expr::Str(s) => assert_eq!(s, "b"),
                    other => panic!("expected key 'b' in doc2, got {other:?}"),
                }
            }
            other => panic!("expected Dict in doc2, got {other:?}"),
        }

        // Document 3: [c: 3]
        let doc3 = &output.file.node.documents[2].node;
        assert_eq!(doc3.expressions.len(), 1);
        match &doc3.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                match &entries[0].node.key.as_ref().unwrap().node {
                    Expr::Str(s) => assert_eq!(s, "c"),
                    other => panic!("expected key 'c' in doc3, got {other:?}"),
                }
            }
            other => panic!("expected Dict in doc3, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_empty_params() {
        // [fn [] 42] — fn with explicit empty param list, body Int(42)
        let output = parse2("[fn [] 42]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Fn {
                params,
                body,
                return_ann,
                desugared,
            } => {
                assert_eq!(params.len(), 0, "expected empty param list");
                assert!(matches!(&body.node, Expr::Int(42)));
                assert!(return_ann.is_none());
                assert!(!desugared);
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_param_span() {
        // [fn [x@Int] $x] — verify param[0] span covers "x@Int"
        // "[fn [x@Int] $x]"
        //  0123456789...
        //  offset 5 = 'x', offset 6 = '@', offset 7..9 = "Int"
        let output = parse2("[fn [x@Int] $x]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Fn { params, .. } => {
                assert_eq!(params.len(), 1);
                let param_span = params[0].span;
                assert_eq!(
                    param_span.start.offset, 5,
                    "expected param span to start at offset 5 ('x'), got {}",
                    param_span.start.offset
                );
                assert!(
                    param_span.end.offset > 9,
                    "expected param span end > 9 (includes '@Int'), got {}",
                    param_span.end.offset
                );
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_duplicate_key() {
        let err = parse2("[a: 1  a: 2]").unwrap_err();
        assert!(err.message.contains("duplicate key"));
        assert!(err.message.contains("\"a\""));
    }

    #[test]
    fn test_empty_document_explicit() {
        // --- is the LLT document separator
        let output = parse2("---\n[a: 1]").expect("parse failed");
        assert_eq!(output.file.node.documents.len(), 2);
        assert_eq!(output.file.node.documents[0].node.expressions.len(), 0);
        assert_eq!(output.file.node.documents[1].node.expressions.len(), 1);
    }

    #[test]
    fn test_annotated_bare_word() {
        let expr = parse_expr("word@Int");
        match &expr.node {
            Expr::Annotated { name, annotation } => {
                assert_eq!(name, "word");
                match &annotation.node {
                    Annotation::Simple(s) => assert_eq!(s, "Int"),
                    other => panic!("expected Simple annotation, got {other:?}"),
                }
            }
            other => panic!("expected Annotated, got {other:?}"),
        }
    }

    #[test]
    fn test_comment_collection() {
        let output = parse2("# my comment\n[a: 1]").expect("parse failed");
        assert!(
            !output.leading_comments.is_empty(),
            "expected leading_comments to be non-empty"
        );
        let has_comment = output
            .leading_comments
            .values()
            .any(|comments| comments.iter().any(|c| c.contains("my comment")));
        assert!(
            has_comment,
            "expected to find 'my comment' in leading_comments, got: {:?}",
            output.leading_comments
        );
    }

    /// Regression test for f1e38a2: bracket-access value inside a keyed dict entry caused a
    /// false "duplicate key" error. `[key: $current-key value: $xs[$current-key]]` was
    /// incorrectly rejected because `pop_last_value_from_frame` restored the popped entry's key
    /// as `pending_key` without removing it from `seen_keys`, so when `push_value` re-inserted
    /// the completed bracket-access entry it found the key already in `seen_keys`.
    #[test]
    fn test_bracket_access_value_in_keyed_dict_no_false_duplicate() {
        // Two distinct keys: "key" and "value". The value for "value" is a bracket-access expr.
        // This must parse without a "duplicate key" error.
        let output = parse2("[key: $k value: $xs[$k]]").expect(
            "parse failed: bracket-access value in keyed dict incorrectly rejected as duplicate key",
        );
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 2, "expected 2 entries");
                // First entry: key: $k
                match &entries[0].node.key.as_ref().unwrap().node {
                    Expr::Str(s) => assert_eq!(s, "key"),
                    other => panic!("expected key 'key', got {other:?}"),
                }
                match &entries[0].node.value.node {
                    Expr::VarRef(name) => assert_eq!(name, "k"),
                    other => panic!("expected VarRef('k') as value, got {other:?}"),
                }
                // Second entry: value: $xs[$k]
                match &entries[1].node.key.as_ref().unwrap().node {
                    Expr::Str(s) => assert_eq!(s, "value"),
                    other => panic!("expected key 'value', got {other:?}"),
                }
                match &entries[1].node.value.node {
                    Expr::BracketAccess { expr, key } => {
                        match &expr.node {
                            Expr::VarRef(name) => assert_eq!(name, "xs"),
                            other => {
                                panic!("expected VarRef('xs') as bracket target, got {other:?}")
                            }
                        }
                        match &key.node {
                            Expr::VarRef(name) => assert_eq!(name, "k"),
                            other => panic!("expected VarRef('k') as bracket key, got {other:?}"),
                        }
                    }
                    other => panic!("expected BracketAccess as value for 'value:', got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    /// Regression test: VarRef as dict key followed by bracket-access value.
    /// `[$k: $xs[$idx]]` — VarRef key "k" whose value is a bracket-access expression.
    /// Must not produce a duplicate key error.
    #[test]
    fn test_varref_key_with_bracket_access_value() {
        let output = parse2("[$k: $xs[$idx]]")
            .expect("parse failed: VarRef key with bracket-access value incorrectly rejected");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                // Key is VarRef("k")
                match &entries[0].node.key.as_ref().unwrap().node {
                    Expr::VarRef(name) => assert_eq!(name, "k"),
                    other => panic!("expected VarRef key 'k', got {other:?}"),
                }
                // Value is BracketAccess
                match &entries[0].node.value.node {
                    Expr::BracketAccess { .. } => {}
                    other => panic!("expected BracketAccess value, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }
}
