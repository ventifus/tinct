//! Iterative parser for tinct — will replace the pest-based parser.
//!
//! This is Phase 2b of the parser rewrite: form classification and access chain handling.
//! Implements call/fn/type-alias forms, keyed entries, bracket access, and annotated bare words.
//!
//! See `doc/whatif/parser-rewrite.md` for the complete design.

use std::collections::BTreeMap;

use crate::ast::*;
use crate::lexer::{self, Token};
use crate::parser::ParseError;

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

/// Helper: count how many whitespace/newline/semicolon tokens to skip from the current position.
fn skip_whitespace_tokens(tokens: &[Spanned<Token>], current_index: usize) -> usize {
    let mut count = 0;
    let mut idx = current_index;
    while idx < tokens.len() {
        match &tokens[idx].node {
            Token::Newline | Token::Semicolon | Token::Comment(_) => {
                count += 1;
                idx += 1;
            }
            _ => break,
        }
    }
    count
}

/// Parse an annotation starting from the given token index (which should be At or ImmediateAt).
/// Returns (Annotation, next_index) on success.
///
/// Annotations are always flat (BareWord or property dict), so no recursion depth check is needed.
/// If property dict annotations are implemented in a future sprint, they will be parsed via the
/// main loop's depth-checked bracket handling, not here.
fn parse_annotation(
    tokens: &[Spanned<Token>],
    start_index: usize,
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
    i += skip_whitespace_tokens(tokens, i);

    if i >= tokens.len() {
        return Err(ParseError {
            message: "unexpected end of input after @".to_string(),
            span: None,
        });
    }

    let ann_token = &tokens[i];

    match &ann_token.node {
        Token::BareWord(name) => {
            // Simple annotation
            let annotation = Annotation::Simple(name.clone());
            Ok((Spanned::new(annotation, ann_token.span), i + 1))
        }
        Token::OpenBracket => {
            // Property dict annotation: @[key: value ...]
            // We need to parse this as a dict and convert it
            // For now, return an error as this requires full dict parsing
            Err(ParseError {
                message: "property dict annotations (@[...]) not yet implemented".to_string(),
                span: Some(ann_token.span),
            })
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
        /// Parameter list — always empty until param list parsing is implemented (parser-core-c).
        /// This field exists for future use when parsing `[fn [x y] body]` syntax.
        #[allow(dead_code)]
        params: Vec<Param>,
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
    BracketAccessKey {
        target: Spanned<Expr>,
        key_expr: Option<Spanned<Expr>>,
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
/// This is the main entry point for Phase 2b. The parser handles:
/// - Basic literals: `Int`, `Float`, `BoolLit`, `QuotedString`, `BareWord`, `VarRef`
/// - Dicts: `[]`, `[42]`, `[a: 1 b: 2]`, keyed and auto-indexed entries
/// - Call forms: `[call $f arg1 arg2 name: val]`
/// - Fn forms: `[fn [params] body]`, `[fn@Type [params] body]` (simplified: no params parsing yet)
/// - Type-alias: `[type expr]`
/// - Type-assert: `[@Annotation expr]` (deferred)
/// - Bracket access: `$a[0]`, `$a[$key]` (deferred)
/// - Annotated bare words: `word@Annotation` (deferred)
///
/// Not yet implemented (deferred to later sprints):
/// - Dot access chains (`.`)
/// - Range operators (`..`)
/// - Document separators (`---`)
/// - Comment collection for formatter
/// - Fn param list parsing
/// - Annotations (simple and property dict)
///
/// NOTE: When parser-core-c lands, `parse()` in parser.rs will be replaced by this function.
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

    // Comment maps (empty for now; filled in by future sprints)
    let leading_comments: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let trailing_comments: BTreeMap<usize, String> = BTreeMap::new();

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
                                peek_next_significant(&token_vec, keyword_idx),
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
                        i += skip_whitespace_tokens(&token_vec, i);
                        i += 1;
                        continue;
                    }
                    Some((Token::BareWord(s), keyword_idx))
                        if s == "fn"
                            && !matches!(
                                peek_next_significant(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Fn form: [fn [params] body]
                        // (Not a fn form if the keyword is followed by colon: [fn: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::Fn {
                            params: Vec::new(),
                            body: None,
                            return_ann: None,
                            span_start: span.start.offset,
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "fn" token
                        i += skip_whitespace_tokens(&token_vec, i);
                        i += 1;
                        continue;
                    }
                    Some((Token::BareWord(s), keyword_idx))
                        if s == "type"
                            && !matches!(
                                peek_next_significant(&token_vec, keyword_idx),
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
                        i += skip_whitespace_tokens(&token_vec, i);
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

                // BracketAccess at document level: pop from current_document_expressions
                // TODO(parser-core-c): Handle BracketAccess inside Dict/Call frames by tracking
                // the last pushed expression per frame
                let target = if !current_document_expressions.is_empty() {
                    current_document_expressions.pop().unwrap()
                } else {
                    return Err(ParseError {
                        message: "bracket access inside dict/call contexts not yet supported"
                            .to_string(),
                        span: Some(span),
                    });
                };

                stack.push(StackFrame::BracketAccessKey {
                    target,
                    key_expr: None,
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
                        params: _,
                        body,
                        return_ann,
                        span_start,
                    } => {
                        // Fn form: [fn [params] body]
                        // For now, simplified: body is required
                        let body = body.ok_or_else(|| ParseError {
                            message: "fn form requires a body expression".to_string(),
                            span: Some(span),
                        })?;

                        let fn_expr = Expr::Fn {
                            return_ann,
                            params: Vec::new(), // Param list parsing deferred to parser-core-c
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
                        span_start,
                    } => {
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

            // Literals: collect as values
            Token::Int(n) => {
                let expr = Spanned::new(Expr::Int(*n), span);
                push_value(&mut stack, &mut current_document_expressions, expr)?;
                i += 1;
                continue;
            }

            Token::Float(f) => {
                let expr = Spanned::new(Expr::Float(*f), span);
                push_value(&mut stack, &mut current_document_expressions, expr)?;
                i += 1;
                continue;
            }

            Token::BoolLit(b) => {
                let expr = Spanned::new(Expr::Bool(*b), span);
                push_value(&mut stack, &mut current_document_expressions, expr)?;
                i += 1;
                continue;
            }

            Token::QuotedString(s) => {
                let expr = Spanned::new(Expr::Str(s.clone()), span);
                push_value(&mut stack, &mut current_document_expressions, expr)?;
                i += 1;
                continue;
            }

            Token::BareWord(s) => {
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
                            i += 1;
                            continue;
                        }
                        _ => {
                            // Not in dict/call context; treat as normal value
                            push_value(&mut stack, &mut current_document_expressions, expr)?;
                            i += 1;
                            continue;
                        }
                    }
                } else {
                    // Not followed by colon; regular value
                    push_value(&mut stack, &mut current_document_expressions, expr)?;
                    i += 1;
                    continue;
                }
            }

            Token::VarRef(name) => {
                let expr = Spanned::new(Expr::VarRef(name.clone()), span);
                push_value(&mut stack, &mut current_document_expressions, expr)?;
                i += 1;
                continue;
            }

            // Other tokens: deferred to later sprints or ignored
            Token::Comment(_) => {
                // Comment collection for formatter comes later
                i += 1;
                continue;
            }

            Token::Newline | Token::Semicolon => {
                // Whitespace/separators — ignored
                i += 1;
                continue;
            }

            Token::DocSeparator => {
                return Err(ParseError {
                    message: "document separators not yet supported".to_string(),
                    span: Some(span),
                });
            }

            Token::Dot => {
                return Err(ParseError {
                    message: "dot access not yet supported".to_string(),
                    span: Some(span),
                });
            }

            Token::Range => {
                return Err(ParseError {
                    message: "range operator not yet supported".to_string(),
                    span: Some(span),
                });
            }

            Token::At | Token::ImmediateAt => {
                // Check context: if we're in a TypeAssert frame and don't have annotation yet, parse it
                match stack.last_mut() {
                    Some(StackFrame::TypeAssert {
                        ref mut annotation, ..
                    }) if annotation.is_none() => {
                        // Parse the annotation
                        let (ann, next_i) = parse_annotation(&token_vec, i)?;
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
                return Err(ParseError {
                    message: "variadic/rest markers not yet supported".to_string(),
                    span: Some(span),
                });
            }
        }
    }

    // Check for unclosed brackets
    if !stack.is_empty() {
        return Err(ParseError {
            message: format!("{} unclosed bracket(s)", stack.len()),
            span: None,
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
                ref mut key_expr, ..
            }) => {
                if key_expr.is_some() {
                    return Err(ParseError {
                        message: "bracket access can only have one key expression".to_string(),
                        span: Some(expr.span),
                    });
                }
                *key_expr = Some(expr);
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
            ..
        }) => {
            if let Some(key) = pending_key.take() {
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
    fn test_bracket_access_inside_dict_errors() {
        // [a: $x[0]] — BracketAccess token arrives when current_document_expressions is empty
        // (we're inside a Dict frame), so bracket access is not supported in that context.
        let err = parse2("[a: $x[0]]").unwrap_err();
        assert!(
            err.message.contains("bracket access inside"),
            "expected error about bracket access inside dict/call context, got: {}",
            err.message
        );
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
}
