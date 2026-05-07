//! Iterative parser for tinct.
//!
//! Hand-written recursive-descent parser that replaced the pest-based parser in sprint parser-core-c3.
//! Implements all language constructs: call/fn/type-alias/type-assert forms, keyed entries,
//! dot access chains (identifier and integer keys), pipe expressions, document separators,
//! comment collection, and fn param list parsing (simple, annotated, variadic).
//!
//! The parser enforces a maximum nesting depth of 256 brackets to prevent stack overflow.
//! Unlike the previous pest parser (which recursed on Rust's call stack), this implementation
//! uses an explicit stack for bracket nesting, making it safe for deeply nested inputs.

use std::collections::BTreeMap;
use std::rc::Rc;

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
/// Call) return None here and are checked at eval-time.
fn key_to_string(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Str(s) => Some(s.clone()),
        Expr::Int(n) => Some(n.to_string()),
        Expr::Float(n) => Some(n.to_string()),
        Expr::Bool(b) => Some(b.to_string()),
        Expr::VarRef { name, .. } => {
            // In new syntax, variable references display as the bare name (no $ prefix).
            // Both bare-word identifiers and $escaped-refs store the name without sigil.
            Some(name.clone())
        }
        _ => None,
    }
}

/// Desugar an interpolated string into a str call.
///
/// Converts `i"Hello $name"` (represented as InterpolatedString parts) into
/// `[str "Hello " name]` (a Call expression).
///
/// `InterpolatedPart::Expr(raw)` segments (`${expr}`) are re-parsed as tinct expressions
/// and included inline. If re-parsing fails, the raw text is included as a quoted string
/// with a TODO comment in the error.
fn desugar_interpolated_string(
    parts: &[lexer::InterpolatedPart],
    span: Span,
) -> Result<Spanned<Expr>, ParseError> {
    use std::cell::RefCell;

    let mut args = Vec::new();

    for part in parts {
        match part {
            lexer::InterpolatedPart::Literal(s) => {
                args.push(Rc::new(Spanned::new(Expr::Str(s.clone()), span)));
            }
            lexer::InterpolatedPart::VarRef(name) => {
                args.push(Rc::new(Spanned::new(
                    Expr::VarRef {
                        name: name.clone(),
                        resolved: RefCell::new(None),
                    },
                    span,
                )));
            }
            lexer::InterpolatedPart::Expr(raw) => {
                // Re-parse the raw expression string as a tinct expression.
                // TODO: span reporting inside ${...} is approximate — the inner spans
                // are relative to `raw` not to the outer source, so error locations
                // inside ${...} will point to the start of the interpolated string.
                match parse_expression(raw) {
                    Ok(inner_expr) => {
                        // Re-span the inner expression to the outer interpolated string span
                        // so that evaluation errors point to a reasonable location.
                        args.push(Rc::new(Spanned::new(inner_expr.node, span)));
                    }
                    Err(_) => {
                        // Fallback: include the raw text as a quoted string.
                        // This preserves round-trip output at the cost of incorrect runtime behavior.
                        // TODO: propagate the inner parse error with adjusted spans.
                        args.push(Rc::new(Spanned::new(Expr::Str(raw.clone()), span)));
                    }
                }
            }
        }
    }

    // Build the [str ...] call
    let str_fn = Box::new(Spanned::new(
        Expr::VarRef {
            name: "str".to_string(),
            resolved: RefCell::new(None),
        },
        span,
    ));

    Ok(Spanned::new(
        Expr::Call {
            func: str_fn,
            args,
            named_args: Vec::new(),
            implied: false,
        },
        span,
    ))
}

/// Helper: count how many whitespace/newline/semicolon tokens to skip from the current position.
/// Also collects comment tokens into the leading_comments map (keyed by the next non-whitespace token's offset).
/// Detects blank lines (consecutive newlines) and marks the next token with blank_before: true.
fn skip_whitespace_tokens(
    tokens: &[Spanned<Token>],
    current_index: usize,
    leading_comments: &mut BTreeMap<usize, Vec<String>>,
    blank_before: &mut BTreeMap<usize, bool>,
) -> usize {
    let mut count = 0;
    let mut idx = current_index;
    let mut collected_comments = Vec::new();
    let mut consecutive_newlines = 0;
    let mut has_blank_line = false;

    while idx < tokens.len() {
        match &tokens[idx].node {
            Token::Comment(text) => {
                collected_comments.push(text.clone());
                consecutive_newlines = 0; // Reset on comment
                count += 1;
                idx += 1;
            }
            Token::Newline => {
                consecutive_newlines += 1;
                if consecutive_newlines >= 2 {
                    has_blank_line = true;
                }
                count += 1;
                idx += 1;
            }
            Token::Semicolon => {
                consecutive_newlines = 0; // Semicolon doesn't contribute to blank lines
                count += 1;
                idx += 1;
            }
            _ => {
                // Found non-whitespace token — attach collected comments and blank-line flag to it
                if idx < tokens.len() {
                    let next_offset = tokens[idx].span.start.offset;
                    if !collected_comments.is_empty() {
                        leading_comments
                            .entry(next_offset)
                            .or_insert_with(Vec::new)
                            .extend(collected_comments);
                    }
                    if has_blank_line {
                        blank_before.insert(next_offset, true);
                    }
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
        | Expr::VarRef { .. }
        | Expr::Rest(_) => expr,

        // Error nodes contain a span that needs adjustment
        Expr::Error(span) => Expr::Error(adjust_span(span, base)),

        Expr::DotAccess { expr, field } => Expr::DotAccess {
            expr: Box::new(adjust_spanned_expr(*expr, base)),
            field,
        },
        Expr::Dict(entries) => Expr::Dict(adjust_entries(entries, base)),
        Expr::Call {
            func,
            args,
            named_args,
            implied,
        } => Expr::Call {
            func: Box::new(adjust_spanned_expr(*func, base)),
            args: args
                .into_iter()
                .map(|a| {
                    Rc::new(adjust_spanned_expr(
                        Rc::try_unwrap(a).unwrap_or_else(|rc| (*rc).clone()),
                        base,
                    ))
                })
                .collect(),
            named_args: named_args
                .into_iter()
                .map(|na| Spanned {
                    span: adjust_span(na.span, base),
                    node: NamedArg {
                        name: na.node.name,
                        value: Rc::new(adjust_spanned_expr((*na.node.value).clone(), base)),
                    },
                })
                .collect(),
            implied,
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
            body: Rc::new(adjust_spanned_expr((*body).clone(), base)),
            desugared,
        },
        Expr::TypeAlias { params, body } => Expr::TypeAlias {
            params: params.clone(),
            body: Box::new(adjust_spanned_expr(*body, base)),
        },
        Expr::Pipe { lhs, rhs } => Expr::Pipe {
            lhs: Box::new(adjust_spanned_expr(*lhs, base)),
            rhs: Box::new(adjust_spanned_expr(*rhs, base)),
        },
        Expr::Sequential(exprs) => Expr::Sequential(
            exprs
                .into_iter()
                .map(|e| {
                    Rc::new(adjust_spanned_expr(
                        Rc::try_unwrap(e).unwrap_or_else(|rc| (*rc).clone()),
                        base,
                    ))
                })
                .collect(),
        ),
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
        Expr::Quote(inner) => Expr::Quote(Box::new(adjust_spanned_expr(*inner, base))),
        Expr::Unquote(inner) => Expr::Unquote(Box::new(adjust_spanned_expr(*inner, base))),
        Expr::UnquoteSplice(inner) => {
            Expr::UnquoteSplice(Box::new(adjust_spanned_expr(*inner, base)))
        }
        Expr::DefMacro { name, transformer } => Expr::DefMacro {
            name,
            transformer: Box::new(adjust_spanned_expr(*transformer, base)),
        },
        Expr::Match { scrutinee, arms } => Expr::Match {
            scrutinee: Box::new(adjust_spanned_expr(*scrutinee, base)),
            arms: arms
                .into_iter()
                .map(|arm| crate::ast::MatchArm {
                    pattern: Spanned {
                        span: adjust_span(arm.pattern.span, base),
                        node: arm.pattern.node,
                    },
                    guard: arm.guard.map(|g| Box::new(adjust_spanned_expr(*g, base))),
                    body: Box::new(adjust_spanned_expr(*arm.body, base)),
                })
                .collect(),
        },
        Expr::ClassDecl {
            name,
            params,
            superclasses,
            methods,
        } => Expr::ClassDecl {
            name,
            params,
            superclasses,
            methods: adjust_entries(methods, base),
        },
        Expr::InstanceDecl {
            class_name,
            instance_type,
            methods,
        } => Expr::InstanceDecl {
            class_name,
            instance_type: Box::new(adjust_spanned_expr(*instance_type, base)),
            methods: adjust_entries(methods, base),
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
                value: Rc::new(adjust_spanned_expr((*se.node.value).clone(), base)),
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
///
/// If `recovered_errors` is provided, certain errors will be recovered from by returning an
/// error annotation placeholder and collecting the error instead of propagating it.
fn parse_annotation(
    tokens: &[Spanned<Token>],
    start_index: usize,
    leading_comments: &mut BTreeMap<usize, Vec<String>>,
    blank_before: &mut BTreeMap<usize, bool>,
    input: &str,
    recovered_errors: Option<&mut Vec<ParseError>>,
) -> Result<(Spanned<Annotation>, usize), ParseError> {
    let mut i = start_index;

    // Skip the @ token
    match &tokens[i].node {
        Token::At | Token::ImmediateAt => {
            i += 1;
        }
        _ => {
            return Err(ParseError {
                message: "expected @ to start annotation".to_string(),
                span: Some(tokens[i].span),
            });
        }
    }

    // Skip whitespace after @
    i += skip_whitespace_tokens(tokens, i, leading_comments, blank_before);

    if i >= tokens.len() {
        let err = ParseError {
            message: "unexpected end of input after @".to_string(),
            span: None,
        };
        if let Some(errors) = recovered_errors {
            errors.push(err);
            // Return a placeholder error annotation
            let placeholder_span = tokens[start_index].span;
            return Ok((
                Spanned::new(Annotation::Simple("Error".to_string()), placeholder_span),
                i,
            ));
        }
        return Err(err);
    }

    let ann_token = &tokens[i];

    match &ann_token.node {
        Token::Identifier(name) => {
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
                    Token::OpenBracket => depth += 1,
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
                let err = ParseError {
                    message: "unclosed bracket in property dict annotation".to_string(),
                    span: Some(bracket_start_span),
                };
                if let Some(errors) = recovered_errors {
                    errors.push(err);
                    // Return a placeholder error annotation
                    return Ok((
                        Spanned::new(Annotation::Simple("Error".to_string()), bracket_start_span),
                        tokens.len(),
                    ));
                }
                return Err(err);
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
            let sub_output = match parse2(sub_source) {
                Ok(output) => output,
                Err(e) => {
                    let err = ParseError {
                        message: format!("error in property dict annotation: {}", e.message),
                        span: Some(ann_span),
                    };
                    if let Some(errors) = recovered_errors {
                        errors.push(err);
                        // Return a placeholder error annotation
                        return Ok((
                            Spanned::new(Annotation::Simple("Error".to_string()), ann_span),
                            end_i + 1,
                        ));
                    }
                    return Err(err);
                }
            };

            // Extract the first expression from the first document.
            let first_expr = sub_output
                .file
                .node
                .documents
                .into_iter()
                .next()
                .and_then(|doc| doc.node.expressions.into_iter().next());

            match first_expr {
                Some(spanned_expr_rc) => match &spanned_expr_rc.node {
                    Expr::Dict(entries) => {
                        // The sub-parse produced spans relative to sub_source (offset 0 = `[`).
                        // Adjust all entry spans back to the original file's coordinate space.
                        let base = bracket_start_span.start;
                        let adjusted = adjust_entries(entries.clone(), base);
                        Ok((
                            Spanned::new(Annotation::PropertyDict(adjusted), ann_span),
                            end_i + 1,
                        ))
                    }
                    // Implied call with uppercase head: @[AliasName Arg1 Arg2] parsed as Call.
                    // Convert to PropertyDict with auto-indexed entries so the
                    // type resolver can detect parameterized alias applications.
                    // Only applies when the func is an uppercase identifier (type name convention).
                    Expr::Call {
                        implied: true,
                        func,
                        args,
                        ..
                    } if matches!(&func.node, Expr::VarRef { name, .. } if name.starts_with(|c: char| c.is_uppercase())) =>
                    {
                        let base = bracket_start_span.start;
                        let mut entries = Vec::new();
                        // func as first auto-indexed entry
                        let adjusted_func = adjust_spanned_expr((**func).clone(), base);
                        let func_span = adjusted_func.span;
                        let func_entry = Spanned::new(
                            Entry {
                                key: None,
                                value: Rc::new(adjusted_func),
                            },
                            func_span,
                        );
                        entries.push(func_entry);
                        // args as subsequent auto-indexed entries
                        for arg in args {
                            let adjusted_arg = adjust_spanned_expr(arg.as_ref().clone(), base);
                            let arg_span = adjusted_arg.span;
                            let arg_entry = Spanned::new(
                                Entry {
                                    key: None,
                                    value: Rc::new(adjusted_arg),
                                },
                                arg_span,
                            );
                            entries.push(arg_entry);
                        }
                        Ok((
                            Spanned::new(Annotation::PropertyDict(entries), ann_span),
                            end_i + 1,
                        ))
                    }
                    other => {
                        let err = ParseError {
                            message: format!(
                                "property dict annotation must be a dict expression, got: {other}"
                            ),
                            span: Some(ann_span),
                        };
                        if let Some(errors) = recovered_errors {
                            errors.push(err);
                            // Return a placeholder error annotation
                            return Ok((
                                Spanned::new(Annotation::Simple("Error".to_string()), ann_span),
                                end_i + 1,
                            ));
                        }
                        Err(err)
                    }
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
        _ => {
            let err = ParseError {
                message: format!(
                    "expected annotation name or bracket dict after @, found {:?}",
                    ann_token.node
                ),
                span: Some(ann_token.span),
            };
            if let Some(errors) = recovered_errors {
                errors.push(err);
                // Return a placeholder error annotation.
                // Do NOT advance past CloseBracket — the main loop needs to see it to pop the frame.
                // For other invalid tokens, advance by 1 to avoid infinite loop.
                return Ok((
                    Spanned::new(Annotation::Simple("Error".to_string()), ann_token.span),
                    if matches!(ann_token.node, Token::CloseBracket) {
                        i
                    } else {
                        i + 1
                    },
                ));
            }
            Err(err)
        }
    }
}

/// Parse a function parameter list: `[param1 param2@Type ...rest]`
/// Expects the current token to be OpenBracket. Advances index past CloseBracket.
/// Returns (Vec<Param>, next_index) on success.
///
/// If `recovered_errors` is provided, certain errors will be recovered from by collecting
/// valid parameters and recording errors instead of failing.
fn parse_param_list(
    tokens: &[Spanned<Token>],
    i: &mut usize,
    leading_comments: &mut BTreeMap<usize, Vec<String>>,
    blank_before: &mut BTreeMap<usize, bool>,
    input: &str,
    mut recovered_errors: Option<&mut Vec<ParseError>>,
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
        *i += skip_whitespace_tokens(tokens, *i, leading_comments, blank_before);

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
                    let err = ParseError {
                        message: "multiple variadic parameters".to_string(),
                        span: Some(tokens[*i].span),
                    };
                    if let Some(errors) = recovered_errors.as_deref_mut() {
                        errors.push(err);
                        *i += 1; // Skip the invalid ellipsis
                        continue;
                    }
                    return Err(err);
                }
                saw_variadic = true;
                let ellipsis_span = tokens[*i].span;
                *i += 1;

                *i += skip_whitespace_tokens(tokens, *i, leading_comments, blank_before);

                if *i >= tokens.len() {
                    let err = ParseError {
                        message: "expected parameter name after '...'".to_string(),
                        span: Some(ellipsis_span),
                    };
                    if let Some(errors) = recovered_errors.as_deref_mut() {
                        errors.push(err);
                        break; // End of input, exit loop
                    }
                    return Err(err);
                }

                match &tokens[*i].node {
                    Token::Identifier(name) => {
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
                            let err = ParseError {
                                message: "annotations on variadic parameters are not allowed"
                                    .to_string(),
                                span: Some(tokens[*i].span),
                            };
                            if let Some(errors) = recovered_errors.as_deref_mut() {
                                errors.push(err);
                                *i += 1; // Skip the annotation token
                                continue;
                            }
                            return Err(err);
                        }
                    }
                    _ => {
                        let err = ParseError {
                            message: "expected parameter name after '...'".to_string(),
                            span: Some(tokens[*i].span),
                        };
                        if let Some(errors) = recovered_errors.as_deref_mut() {
                            errors.push(err);
                            *i += 1; // Skip the invalid token
                            continue;
                        }
                        return Err(err);
                    }
                }
            }
            Token::Identifier(name) => {
                if saw_variadic {
                    let err = ParseError {
                        message: "parameter after variadic parameter".to_string(),
                        span: Some(tokens[*i].span),
                    };
                    if let Some(errors) = recovered_errors.as_deref_mut() {
                        errors.push(err);
                        *i += 1; // Skip the invalid param
                        continue;
                    }
                    return Err(err);
                }

                let param_start_span = tokens[*i].span;
                let param_name = name.clone();
                *i += 1;

                // Check for annotation: param@Type or param@[type: ...]
                let annotation =
                    if *i < tokens.len() && matches!(&tokens[*i].node, Token::ImmediateAt) {
                        match parse_annotation(
                            tokens,
                            *i,
                            leading_comments,
                            blank_before,
                            input,
                            recovered_errors.as_deref_mut(),
                        ) {
                            Ok((ann, next_i)) => {
                                *i = next_i;
                                Some(ann)
                            }
                            Err(e) => {
                                // Invariant: parse_annotation with Some(errors) should always return Ok
                                // with a placeholder. Err here means we're NOT in recovery mode.
                                if recovered_errors.is_none() {
                                    return Err(e);
                                }
                                // If we reach here, parse_annotation failed despite being in recovery mode.
                                // This shouldn't happen (indicates a programming error), but handle gracefully.
                                None
                            }
                        }
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
                let err = ParseError {
                    message: format!("unexpected token in param list: {:?}", tokens[*i].node),
                    span: Some(tokens[*i].span),
                };
                if let Some(errors) = recovered_errors.as_deref_mut() {
                    errors.push(err);
                    *i += 1; // Skip the unexpected token
                    continue;
                }
                return Err(err);
            }
        }
    }

    Ok(params)
}

/// Stack frame types for the iterative parser.
///
/// Each variant corresponds to a bracket-form being parsed. The parser pushes
/// a frame on `Token::OpenBracket`, collects entries/args/params
/// during iteration, then pops and constructs the AST node on `Token::CloseBracket`.
#[derive(Debug, Clone)]
enum StackFrame {
    /// Dictionary literal: `[key: value ...]`
    Dict {
        entries: Vec<Entry>,
        /// Pending key from an Identifier/QuotedString/EscapedRef before a colon
        pending_key: Option<Spanned<Expr>>,
        /// Track seen keys for duplicate detection (literal keys only)
        seen_keys: std::collections::HashSet<String>,
        span_start: Position,
    },
    /// Function call: `[call func arg1 arg2 name: val]` or `[func arg1 arg2 name: val]`
    Call {
        /// For implied calls (`[f x]`), the function is extracted from the head Identifier at frame-push time.
        /// For explicit calls (`[call f x]`), this is None and func is extracted from args[0].
        func: Option<Spanned<Expr>>,
        /// Whether this is an implied call ([f x]) or explicit call ([call f x])
        implied: bool,
        args: Vec<CallArg>,
        /// Pending key for named args (Identifier before colon)
        pending_key: Option<(String, Span)>,
        span_start: Position,
    },
    /// Function definition: `[fn [params] body]` or `[fn@Type [params] body]`
    Fn {
        /// Parameter list — parsed from `[fn [x y] body]` syntax
        params: Vec<Spanned<Param>>,
        /// Body expressions — for multi-expression bodies (let-binding)
        body: Vec<Spanned<Expr>>,
        return_ann: Option<Spanned<Annotation>>,
        span_start: Position,
    },
    /// Type alias: `[type expr]` or `[type [params] expr]` or `[type T1 T2 ...]`
    TypeAlias {
        params: Vec<String>,
        /// Multiple type expressions for multi-entry union declarations.
        /// Single-entry `[type T]` has exactly one element.
        /// Multi-entry `[type T1 T2 ...]` has 2+ elements.
        type_exprs: Vec<Spanned<Expr>>,
        span_start: Position,
    },
    /// Type assertion: `[@Annotation expr]`
    TypeAssert {
        annotation: Option<Spanned<Annotation>>,
        expr: Option<Spanned<Expr>>,
        span_start: Position,
    },
    /// Quote special form: `[quote expr]`
    Quote {
        expr: Option<Spanned<Expr>>,
        span_start: Position,
    },
    /// Unquote special form: `[unquote expr]` (only valid inside quote)
    Unquote {
        expr: Option<Spanned<Expr>>,
        span_start: Position,
    },
    /// Unquote-splice special form: `[unquote-splice expr]` (only valid in list positions inside quote)
    UnquoteSplice {
        expr: Option<Spanned<Expr>>,
        span_start: Position,
    },
    /// Macro definition: `[defmacro name transformer]`
    DefMacro {
        name: Option<String>,
        transformer: Option<Spanned<Expr>>,
        span_start: Position,
    },
    /// Match expression: `[match scrutinee pat1 body1 pat2 body2 ...]`
    Match {
        scrutinee: Option<Spanned<Expr>>,
        arms: Vec<MatchArm>,
        /// Pending pattern (with optional guard) for the current arm
        pending_pattern: Option<(Spanned<Pattern>, Option<Box<Spanned<Expr>>>)>,
        span_start: Position,
    },
    /// Class declaration: `[class [ClassName param...] method: Type ...]`
    ClassDecl {
        name: Option<String>,
        params: Vec<String>,
        superclasses: Vec<String>,
        methods: Vec<Entry>,
        /// Pending key for method entries
        pending_key: Option<Spanned<Expr>>,
        span_start: Position,
    },
    /// Instance declaration: `[instance [ClassName Type] method: impl ...]`
    InstanceDecl {
        class_name: Option<String>,
        instance_type: Option<Spanned<Expr>>,
        methods: Vec<Entry>,
        /// Pending key for method entries
        pending_key: Option<Spanned<Expr>>,
        span_start: Position,
    },
    /// Pipe operator: `lhs | rhs`
    /// Holds the LHS and waits for the RHS to be parsed
    Pipe {
        lhs: Spanned<Expr>,
        span_start: Position,
    },
}

/// Intermediate representation for call arguments (positional or named).
///
/// During call parsing, arguments are collected in order. The evaluator later
/// enforces the C-PRIORITY binding order (see `doc/04-functions.md §Call Convention`).
#[derive(Debug, Clone, PartialEq)]
enum CallArg {
    Positional(Rc<Spanned<Expr>>),
    Named(String, Rc<Spanned<Expr>>),
}

/// Parse output: AST plus comment side-channels for the formatter.
///
/// `leading_comments` are keyed by the `span.start.offset` of the node they precede.
/// `trailing_comments` are keyed by the `span.start.offset` of the node they follow.
/// `blank_before` is keyed by the `span.start.offset` of the node and set to `true` when
/// there was a blank line (consecutive newlines) before that node.
///
/// `errors` contains parse errors that were recovered from during parsing. These are
/// errors that occurred inside bracket forms; the parser substituted an `Expr::Error`
/// node and continued. Fatal errors (lexer failure, unclosed brackets at top level)
/// still cause `parse2()` to return `Err(...)`.
///
/// The evaluator and type checker consume only `file`; the formatter uses all three fields.
///
/// External pipeline consumers that don't need comments should access `.file` directly.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseOutput {
    pub file: Spanned<File>,
    pub source: String,
    pub leading_comments: BTreeMap<usize, Vec<String>>,
    pub trailing_comments: BTreeMap<usize, String>,
    pub blank_before: BTreeMap<usize, bool>,
    /// Recovered parse errors (errors inside bracket forms where the parser continued).
    pub errors: Vec<ParseError>,
}

/// Given a token slice and a start index pointing just past an `[`,
/// advance until the matching `]` is found (tracking nesting depth).
///
/// Returns the index of the `CloseBracket` token that closes the bracket opened before
/// `from_idx` (i.e. the `]` whose depth-count reaches zero). If no matching `]` is
/// found before the end of the token slice, returns `tokens.len()` (pointing past the end).
///
/// `from_idx` is the index of the first token *inside* the bracket (not the `[` itself).
/// The returned index points at the closing `]`, so the caller should advance past it
/// with `i = result + 1`.
fn skip_to_closing_bracket(tokens: &[Spanned<Token>], from_idx: usize) -> usize {
    let mut depth: usize = 1; // we are already inside one bracket
    let mut idx = from_idx;
    while idx < tokens.len() {
        match &tokens[idx].node {
            Token::OpenBracket => depth += 1,
            Token::CloseBracket => {
                depth -= 1;
                if depth == 0 {
                    return idx;
                }
            }
            _ => {}
        }
        idx += 1;
    }
    // No matching close found — return past-end sentinel
    tokens.len()
}

/// Recover from a parse error that occurred *inside* a bracket form (the frame is already pushed).
///
/// Called when an error occurs while processing tokens inside an existing `StackFrame`. This function:
/// 1. Records the error in `recovered_errors`.
/// 2. Pops the innermost `StackFrame` (which contained the error).
/// 3. For Dict/Call frames: builds a partial expression with valid entries collected so far,
///    plus an `Expr::Error` entry for the malformed part.
/// 4. For other frames: pushes `Expr::Error(error_span)` to the parent.
/// 5. Skips `i` past the `]` that closes the abandoned frame, accounting for nested brackets.
///
/// After calling this, the caller should `continue` the main token loop.
///
/// `error_span`: the span to use for the `Expr::Error` node.
/// `skip_from_idx`: index of the first token to search from when looking for the matching `]`.
///
/// Returns the new token index (pointing past the closing `]`, or at `tokens.len()` if not found).
fn recover_from_bracket_error(
    error: ParseError,
    error_span: Span,
    tokens: &[Spanned<Token>],
    skip_from_idx: usize,
    stack: &mut Vec<StackFrame>,
    current_document_expressions: &mut Vec<Rc<Spanned<Expr>>>,
    recovered_errors: &mut Vec<ParseError>,
) -> usize {
    recovered_errors.push(error);

    // Pop the frame that contained the error (the innermost one).
    // If the stack is empty, we have nothing to pop — this shouldn't happen if the caller
    // checked `!stack.is_empty()`, but handle it gracefully.
    let frame = if !stack.is_empty() { stack.pop() } else { None };

    // Build a partial expression with valid entries plus an error marker.
    let partial_expr = if let Some(frame) = frame {
        match frame {
            StackFrame::Dict {
                entries,
                span_start,
                ..
            } => {
                // If there are valid entries, build a partial dict with them plus an error entry.
                // If there are no valid entries, just emit Expr::Error.
                if entries.is_empty() {
                    Spanned::new(Expr::Error(error_span), error_span)
                } else {
                    let mut partial_entries = entries
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
                        .collect::<Vec<_>>();

                    // Add the error as an auto-indexed entry
                    partial_entries.push(Spanned::new(
                        Entry {
                            key: None,
                            value: Rc::new(Spanned::new(Expr::Error(error_span), error_span)),
                        },
                        error_span,
                    ));

                    let dict_span = Span {
                        start: span_start,
                        end: error_span.end,
                    };
                    Spanned::new(Expr::Dict(partial_entries), dict_span)
                }
            }
            StackFrame::Call {
                func: frame_func,
                implied,
                args,
                span_start,
                ..
            } => {
                // Build a partial call with valid args plus an error arg.
                let func = if let Some(ref f) = frame_func {
                    // Implied call: func already captured
                    Some(f.clone())
                } else if !args.is_empty() {
                    // Explicit call: try to extract func from args[0]
                    match &args[0] {
                        CallArg::Positional(f) => {
                            Some(Rc::try_unwrap(Rc::clone(f)).unwrap_or_else(|rc| (*rc).clone()))
                        }
                        CallArg::Named(_, _) => None, // Invalid
                    }
                } else {
                    None
                };

                if func.is_none() {
                    // No function — can't build a call, use plain error
                    Spanned::new(Expr::Error(error_span), error_span)
                } else {
                    let func = func.unwrap();
                    let mut positional_args = Vec::new();
                    let mut named_args = Vec::new();

                    // For implied calls, args starts at 0. For explicit calls, skip args[0] (the func).
                    let args_iter = if frame_func.is_some() {
                        args.into_iter()
                    } else {
                        args.into_iter().skip(1).collect::<Vec<_>>().into_iter()
                    };

                    for arg in args_iter {
                        match arg {
                            CallArg::Positional(expr) => positional_args.push(expr),
                            CallArg::Named(name, expr) => {
                                named_args.push(Spanned::new(
                                    NamedArg {
                                        name,
                                        value: Rc::clone(&expr),
                                    },
                                    expr.span,
                                ));
                            }
                        }
                    }

                    let has_args_now = !positional_args.is_empty() || !named_args.is_empty();

                    // Only build a partial call if there were actual args.
                    // If it's just [f] or [call f] with an error, emit plain Error.
                    if !has_args_now {
                        Spanned::new(Expr::Error(error_span), error_span)
                    } else {
                        // Add the error as a positional argument
                        positional_args
                            .push(Rc::new(Spanned::new(Expr::Error(error_span), error_span)));

                        let call_span = Span {
                            start: span_start,
                            end: error_span.end,
                        };
                        Spanned::new(
                            Expr::Call {
                                func: Box::new(func),
                                args: positional_args,
                                named_args,
                                implied,
                            },
                            call_span,
                        )
                    }
                }
            }
            _ => {
                // For other frame types (Fn, TypeAlias, TypeAssert, Pipe),
                // we can't meaningfully preserve partial state, so just emit Error.
                Spanned::new(Expr::Error(error_span), error_span)
            }
        }
    } else {
        Spanned::new(Expr::Error(error_span), error_span)
    };

    // Push the partial expression to the parent context.
    if stack.is_empty() {
        current_document_expressions.push(Rc::new(partial_expr));
    } else {
        // push_value can itself error (e.g. duplicate key). If it does, we just ignore it —
        // we're already in recovery mode and the partial_expr is best-effort.
        let _ = push_value(stack, current_document_expressions, partial_expr);
    }

    // Skip past the matching `]`. `skip_to_closing_bracket` expects to be called with
    // depth=1 (already inside one bracket), starting from the first token inside the bracket.
    let close_idx = skip_to_closing_bracket(tokens, skip_from_idx);

    // Return the index past the closing `]` (or past-end if no matching close found).
    if close_idx < tokens.len() {
        close_idx + 1
    } else {
        tokens.len()
    }
}

/// Recover from a parse error that occurred when *opening* a bracket form (frame NOT yet pushed).
///
/// Called when the parser fails to push a new `StackFrame` (e.g., depth limit exceeded).
/// Since no frame was pushed, this
/// function does NOT pop anything. It:
/// 1. Records the error in `recovered_errors`.
/// 2. Pushes `Expr::Error(error_span)` to the current top frame (or document).
/// 3. Skips `i` past the `]` that closes the bracket that failed to open.
///
/// `skip_from_idx` should be the token index just after the `[` that triggered the error
/// (i.e., the first token *inside* the bracket we failed to process).
///
/// Returns the new token index (pointing past the closing `]`, or at `tokens.len()` if not found).
fn recover_from_failed_open(
    error: ParseError,
    error_span: Span,
    tokens: &[Spanned<Token>],
    skip_from_idx: usize,
    stack: &mut Vec<StackFrame>,
    current_document_expressions: &mut Vec<Rc<Spanned<Expr>>>,
    recovered_errors: &mut Vec<ParseError>,
) -> usize {
    recovered_errors.push(error);

    // Push Expr::Error into the current top frame (without popping — no frame was pushed).
    let error_expr = Spanned::new(Expr::Error(error_span), error_span);

    if stack.is_empty() {
        current_document_expressions.push(Rc::new(error_expr));
    } else {
        let _ = push_value(stack, current_document_expressions, error_expr);
    }

    // Skip past the matching `]`.
    let close_idx = skip_to_closing_bracket(tokens, skip_from_idx);

    if close_idx < tokens.len() {
        close_idx + 1
    } else {
        tokens.len()
    }
}

/// Parse tinct source text using the iterative parser.
///
/// This is the main entry point for Phase 2c-1 (complete feature set). The parser handles:
/// - Basic literals: `Int`, `Float`, `BoolLit`, `QuotedString`, `Identifier`, `EscapedRef`
/// - Dicts: `[]`, `[42]`, `[a: 1 b: 2]`, keyed and auto-indexed entries
/// - Call forms: `[call $f arg1 arg2 name: val]`
/// - Fn forms: `[fn [x y@Int ...rest] body]`, `[fn@Type [params] body]` with full param parsing
/// - Type-alias: `[type expr]`
/// - Type-assert: `[@Annotation expr]`
/// - Dot access chains: `$a.b.c`, `$a.0` (identifier and integer keys)
/// - Pipe expressions: `$a | $f` (reverse-apply)
/// - Document separators: `---` between document sections
/// - Comment collection: leading and trailing comments attached by span offset
///
/// When errors occur inside bracket forms, the parser recovers by substituting an
/// `Expr::Error` node and skipping to the matching `]`. Recovered errors are collected
/// in `ParseOutput.errors`. Fatal errors (lexer failure, unclosed brackets) still
/// cause this function to return `Err(...)`.
pub fn parse2(input: &str) -> Result<ParseOutput, ParseError> {
    // Tokenize the input via the lexer
    let tokens = lexer::tokenize(input).map_err(|e| ParseError {
        message: e.message,
        span: Some(e.span),
    })?;

    // Stack of frames tracking bracket nesting
    let mut stack: Vec<StackFrame> = Vec::new();

    // Quote nesting depth — incremented when entering [quote ...], decremented when leaving.
    // Used to track whether we're inside a quote (depth > 0) for unquote/unquote-splice validation.
    let mut quote_depth: u32 = 0;

    // Current document being built (one or more expressions)
    let mut current_document_expressions: Vec<Rc<Spanned<Expr>>> = Vec::new();

    // All documents in the file
    let mut documents: Vec<Spanned<Document>> = Vec::new();

    // Comment maps
    let mut leading_comments: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut trailing_comments: BTreeMap<usize, String> = BTreeMap::new();
    let mut blank_before: BTreeMap<usize, bool> = BTreeMap::new();

    // Recovered parse errors (errors inside bracket forms)
    let mut recovered_errors: Vec<ParseError> = Vec::new();

    // Track the span of the last significant token for trailing comment detection
    let mut last_significant_span: Option<Span> = None;

    // Phase 1: Track next document's header components (parsed from --- line)
    let mut next_doc_name: Option<String> = None;
    // Span of the %name token that set next_doc_name; used to point errors at the name,
    // not at the --- separator whose span is in the outer-loop `span` variable.
    let mut next_doc_name_span: Option<Span> = None;
    let mut next_doc_output_type: Option<Spanned<Annotation>> = None;
    let mut next_doc_expects: Option<Spanned<Annotation>> = None;

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
                    let err = ParseError {
                        message: format!(
                            "maximum nesting depth exceeded (limit: {MAX_PARSE_DEPTH})"
                        ),
                        span: Some(span),
                    };
                    if !stack.is_empty() {
                        // Recovery: failed to open the bracket (no frame pushed yet).
                        // Skip the entire bracket form, push Error to current parent.
                        i = recover_from_failed_open(
                            err,
                            span,
                            &token_vec,
                            i + 1, // skip from inside the bracket we tried to open
                            &mut stack,
                            &mut current_document_expressions,
                            &mut recovered_errors,
                        );
                        continue;
                    }
                    return Err(err);
                }

                // Peek at next non-whitespace/non-newline token for form classification
                let next_token = peek_next_significant(&token_vec, i);

                match next_token {
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "call"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Explicit call form: [call func args...]
                        // (Not a call form if the keyword is followed by colon: [call: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::Call {
                            func: None, // func extracted from args[0]
                            implied: false,
                            args: Vec::new(),
                            pending_key: None,
                            span_start: span.start,
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "call" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
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
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1; // Consume the "fn" token

                        // Check for return annotation: fn@RetType
                        let return_ann = if i < token_vec.len()
                            && matches!(&token_vec[i].node, Token::ImmediateAt)
                        {
                            match parse_annotation(
                                &token_vec,
                                i,
                                &mut leading_comments,
                                &mut blank_before,
                                input,
                                Some(&mut recovered_errors),
                            ) {
                                Ok((ann, next_i)) => {
                                    i = next_i;
                                    Some(ann)
                                }
                                Err(ann_err) => {
                                    // The [fn ...] bracket was opened but the Fn frame was not yet
                                    // pushed; use recover_from_failed_open (no pop needed).
                                    if !stack.is_empty() {
                                        i = recover_from_failed_open(
                                            ann_err,
                                            span,
                                            &token_vec,
                                            i,
                                            &mut stack,
                                            &mut current_document_expressions,
                                            &mut recovered_errors,
                                        );
                                    } else {
                                        // At top level: push to doc, skip to close.
                                        i = recover_from_failed_open(
                                            ann_err,
                                            span,
                                            &token_vec,
                                            i,
                                            &mut stack,
                                            &mut current_document_expressions,
                                            &mut recovered_errors,
                                        );
                                    }
                                    continue;
                                }
                            }
                        } else {
                            None
                        };

                        // Parse param list if present: [fn [params] body]
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        let params = if i < token_vec.len()
                            && matches!(&token_vec[i].node, Token::OpenBracket)
                        {
                            match parse_param_list(
                                &token_vec,
                                &mut i,
                                &mut leading_comments,
                                &mut blank_before,
                                input,
                                Some(&mut recovered_errors),
                            ) {
                                Ok(ps) => ps,
                                Err(param_err) => {
                                    // The [fn ...] bracket was opened but the Fn frame was not
                                    // yet pushed; use recover_from_failed_open (no pop needed).
                                    i = recover_from_failed_open(
                                        param_err,
                                        span,
                                        &token_vec,
                                        i,
                                        &mut stack,
                                        &mut current_document_expressions,
                                        &mut recovered_errors,
                                    );
                                    continue;
                                }
                            }
                        } else {
                            Vec::new()
                        };

                        stack.push(StackFrame::Fn {
                            params,
                            body: Vec::new(),
                            return_ann,
                            span_start: span.start,
                        });
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "type"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Type-alias form: [type expr] or [type [params] expr] or [type T1 T2 ...]
                        // (Not a type form if the keyword is followed by colon: [type: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::TypeAlias {
                            params: Vec::new(),
                            type_exprs: Vec::new(),
                            span_start: span.start,
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "type" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "quote"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Quote form: [quote expr]
                        // (Not a quote form if the keyword is followed by colon: [quote: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::Quote {
                            expr: None,
                            span_start: span.start,
                        });
                        quote_depth += 1; // Entering a quote context
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "quote" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "unquote"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Unquote form: [unquote expr]
                        // (Not an unquote form if the keyword is followed by colon: [unquote: x] is a dict.)
                        // (depth already checked above)
                        if quote_depth == 0 {
                            return Err(ParseError {
                                message: "unquote is only valid inside [quote ...]".to_string(),
                                span: Some(span),
                            });
                        }
                        stack.push(StackFrame::Unquote {
                            expr: None,
                            span_start: span.start,
                        });
                        quote_depth -= 1; // Unquote decrements depth (evaluates in outer context)
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "unquote" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "unquote-splice"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Unquote-splice form: [unquote-splice expr]
                        // (Not an unquote-splice form if the keyword is followed by colon: [unquote-splice: x] is a dict.)
                        // (depth already checked above)
                        if quote_depth == 0 {
                            return Err(ParseError {
                                message: "unquote-splice is only valid inside [quote ...]"
                                    .to_string(),
                                span: Some(span),
                            });
                        }
                        // Check if we're at the top level of a quote (not in a list position).
                        // If the parent frame is Quote, that's an error per Bawden (1999).
                        if matches!(stack.last(), Some(StackFrame::Quote { .. })) {
                            return Err(ParseError {
                                message: "unquote-splice at top level of [quote ...] is invalid; it must be in a list position".to_string(),
                                span: Some(span),
                            });
                        }
                        stack.push(StackFrame::UnquoteSplice {
                            expr: None,
                            span_start: span.start,
                        });
                        quote_depth -= 1; // Unquote-splice decrements depth (evaluates in outer context)
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "unquote-splice" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "defmacro"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // DefMacro form: [defmacro name transformer]
                        // (Not a defmacro form if the keyword is followed by colon: [defmacro: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::DefMacro {
                            name: None,
                            transformer: None,
                            span_start: span.start,
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "defmacro" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "match"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Match form: [match scrutinee pat1 body1 pat2 body2 ...]
                        // (Not a match form if the keyword is followed by colon: [match: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::Match {
                            scrutinee: None,
                            arms: Vec::new(),
                            pending_pattern: None,
                            span_start: span.start,
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "match" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "class"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Class declaration: [class [Name a] method: Type ...]
                        // (Not a class form if the keyword is followed by colon: [class: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::ClassDecl {
                            name: None,
                            params: Vec::new(),
                            superclasses: Vec::new(),
                            methods: Vec::new(),
                            pending_key: None,
                            span_start: span.start,
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "class" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::Identifier(s), keyword_idx))
                        if s == "instance"
                            && !matches!(
                                peek_next_horizontal(&token_vec, keyword_idx),
                                Some((Token::Colon, _))
                            ) =>
                    {
                        // Instance declaration: [instance [Name Type] method: impl ...]
                        // (Not an instance form if the keyword is followed by colon: [instance: x] is a dict.)
                        // (depth already checked above)
                        stack.push(StackFrame::InstanceDecl {
                            class_name: None,
                            instance_type: None,
                            methods: Vec::new(),
                            pending_key: None,
                            span_start: span.start,
                        });
                        i += 1; // Consume the OpenBracket
                                // Skip whitespace and consume the "instance" token
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );
                        i += 1;
                        continue;
                    }
                    Some((Token::At, _)) | Some((Token::ImmediateAt, _)) => {
                        // Type-assert form: [@Annotation expr]
                        // (depth already checked above)
                        stack.push(StackFrame::TypeAssert {
                            annotation: None,
                            expr: None,
                            span_start: span.start,
                        });
                        i += 1; // Consume the OpenBracket
                        continue;
                    }
                    Some((Token::Identifier(_name), identifier_idx))
                        if matches!(
                            peek_next_horizontal(&token_vec, identifier_idx),
                            Some((Token::Colon, _))
                        ) =>
                    {
                        // Priority 2: Identifier followed by horizontal colon → Dict
                        // [name: val] is a dict entry, not a call
                        // (depth already checked above)
                        stack.push(StackFrame::Dict {
                            entries: Vec::new(),
                            pending_key: None,
                            seen_keys: std::collections::HashSet::new(),
                            span_start: span.start,
                        });
                        i += 1; // Consume the OpenBracket
                        continue;
                    }
                    Some((Token::Identifier(_name), identifier_idx))
                        if matches!(
                            peek_next_horizontal(&token_vec, identifier_idx),
                            Some((Token::ImmediateAt, _))
                        ) =>
                    {
                        // Priority 2b: Identifier followed by ImmediateAt → Dict (data)
                        // [Foo@String] and [x@Number: 42] are data forms, not implied calls.
                        // Annotations attach to bare words in data position; call heads are never annotated.
                        // (depth already checked above)
                        stack.push(StackFrame::Dict {
                            entries: Vec::new(),
                            pending_key: None,
                            seen_keys: std::collections::HashSet::new(),
                            span_start: span.start,
                        });
                        i += 1; // Consume the OpenBracket
                        continue;
                    }
                    Some((Token::Identifier(name), _identifier_idx)) => {
                        // Priority 3: Identifier in head (not keyword, no horizontal colon, no ImmediateAt) → Implied call
                        // [f x y] calls f
                        // (depth already checked above)

                        // Consume the OpenBracket
                        i += 1;
                        // Skip whitespace to the identifier
                        i += skip_whitespace_tokens(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                        );

                        // Capture the identifier span and value
                        let func_span = token_vec[i].span;
                        let func_name = name.clone();

                        // Consume the identifier token
                        i += 1;

                        // Create VarRef expr for the function
                        let func_expr = Spanned::new(Expr::var_ref(func_name), func_span);

                        stack.push(StackFrame::Call {
                            func: Some(func_expr),
                            implied: true,
                            args: Vec::new(),
                            pending_key: None,
                            span_start: span.start,
                        });
                        continue;
                    }
                    _ => {
                        // Priority 4 & 5: EscapedRef, literals, or anything else in head → Dict (data)
                        // [$f x y] is data, [1 2 3] is data, ["a" "b"] is data
                        // (depth already checked above)
                        stack.push(StackFrame::Dict {
                            entries: Vec::new(),
                            pending_key: None,
                            seen_keys: std::collections::HashSet::new(),
                            span_start: span.start,
                        });
                        i += 1;
                        continue;
                    }
                }
            }

            Token::CloseBracket => {
                // Pop the frame and construct the AST node
                let frame = stack.pop().ok_or_else(|| ParseError {
                    message: "unmatched closing bracket".to_string(),
                    span: Some(span),
                })?;

                let dict_span = |span_start: Position| Span {
                    start: span_start,
                    end: span.end,
                };

                // Helper: recover from a CloseBracket-handler error (frame already popped).
                // Pushes Expr::Error to the new top-of-stack or doc, records the error, and
                // falls through to the `last_significant_span`/`i += 1`/`continue` at the end.
                macro_rules! close_bracket_recover {
                    ($err:expr) => {{
                        let err: ParseError = $err;
                        let error_span = err.span.unwrap_or(span);
                        recovered_errors.push(err);
                        let error_expr = Spanned::new(Expr::Error(error_span), error_span);
                        // Push to parent context (stack has already had the frame popped).
                        if stack.is_empty() {
                            current_document_expressions.push(Rc::new(error_expr.clone()));
                        } else {
                            // Ignore secondary errors during recovery.
                            let _ = push_value(
                                &mut stack,
                                &mut current_document_expressions,
                                error_expr,
                            );
                        }
                        // Fall through to advance i and continue.
                    }};
                }

                match frame {
                    StackFrame::Dict {
                        entries,
                        pending_key,
                        seen_keys: _,
                        span_start,
                    } => {
                        // If there's a pending key, that's an error — key without value
                        if let Some(key_expr) = pending_key {
                            close_bracket_recover!(ParseError {
                                message: "key without value: expected `:` and value".to_string(),
                                span: Some(key_expr.span),
                            });
                        } else {
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
                            if let Err(push_err) = push_value(
                                &mut stack,
                                &mut current_document_expressions,
                                spanned_dict,
                            ) {
                                close_bracket_recover!(push_err);
                            }
                        }
                    }

                    StackFrame::Call {
                        func: frame_func,
                        implied,
                        args,
                        pending_key,
                        span_start,
                    } => {
                        // If there's a pending key, that's an error — named arg without value
                        if let Some((key, key_span)) = pending_key {
                            close_bracket_recover!(ParseError {
                                message: format!("named argument `{}` without value", key),
                                span: Some(key_span),
                            });
                        } else {
                            // Determine the function expression
                            let func = if let Some(ref f) = frame_func {
                                // Implied call: func was captured from head Identifier at frame-push time
                                Ok(f.clone())
                            } else if args.is_empty() {
                                // Explicit call with no args: [call] is an error
                                Err(ParseError {
                                    message: "call form requires at least a function expression"
                                        .to_string(),
                                    span: Some(span),
                                })
                            } else {
                                // Explicit call: func is args[0]
                                match &args[0] {
                                    CallArg::Positional(expr) => Ok(Rc::try_unwrap(Rc::clone(expr))
                                        .unwrap_or_else(|rc| (*rc).clone())),
                                    CallArg::Named(name, _) => Err(ParseError {
                                        message: format!(
                                            "call function cannot be a named argument (got `{name}:`)",
                                        ),
                                        span: Some(span),
                                    }),
                                }
                            };

                            match func {
                                Err(func_err) => {
                                    close_bracket_recover!(func_err);
                                }
                                Ok(func) => {
                                    let mut positional_args = Vec::new();
                                    let mut named_args = Vec::new();

                                    // For implied calls, args starts at 0. For explicit calls, skip args[0] (the func).
                                    let args_iter = if frame_func.is_some() {
                                        args.into_iter()
                                    } else {
                                        args.into_iter().skip(1).collect::<Vec<_>>().into_iter()
                                    };

                                    for arg in args_iter {
                                        match arg {
                                            CallArg::Positional(expr) => positional_args.push(expr),
                                            CallArg::Named(name, expr) => {
                                                named_args.push(Spanned::new(
                                                    NamedArg {
                                                        name,
                                                        value: Rc::clone(&expr),
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
                                        implied,
                                    };

                                    let spanned_call =
                                        Spanned::new(call_expr, dict_span(span_start));
                                    if let Err(push_err) = push_value(
                                        &mut stack,
                                        &mut current_document_expressions,
                                        spanned_call,
                                    ) {
                                        close_bracket_recover!(push_err);
                                    }
                                }
                            }
                        }
                    }

                    StackFrame::Fn {
                        params,
                        body,
                        return_ann,
                        span_start,
                    } => {
                        if body.is_empty() {
                            close_bracket_recover!(ParseError {
                                message: "fn form requires a body expression".to_string(),
                                span: Some(span),
                            });
                        }

                        // For single-expression bodies, use the expression directly.
                        // For multi-expression bodies, wrap in Sequential.
                        let body_expr = if body.len() == 1 {
                            Rc::new(body.into_iter().next().unwrap())
                        } else {
                            Rc::new(Spanned::new(
                                Expr::Sequential(body.into_iter().map(Rc::new).collect()),
                                dict_span(span_start),
                            ))
                        };

                        let fn_expr = Expr::Fn {
                            return_ann,
                            params,
                            body: body_expr,
                            desugared: false,
                        };

                        let spanned_fn = Spanned::new(fn_expr, dict_span(span_start));
                        if let Err(push_err) =
                            push_value(&mut stack, &mut current_document_expressions, spanned_fn)
                        {
                            close_bracket_recover!(push_err);
                        }
                    }

                    StackFrame::TypeAlias {
                        params,
                        type_exprs,
                        span_start,
                    } => {
                        if type_exprs.is_empty() {
                            close_bracket_recover!(ParseError {
                                message: "type-alias form requires at least one type expression"
                                    .to_string(),
                                span: Some(span),
                            });
                        } else {
                            // Multi-entry union: wrap all entries in a single TypeAlias with Union body.
                            // The type checker will construct Type::Union from multiple entries.
                            // Single-entry: remains a simple alias (type checker unwraps single-element unions).
                            let body = if type_exprs.len() == 1 {
                                type_exprs.into_iter().next().unwrap()
                            } else {
                                // Create a synthetic Dict expression with positional entries.
                                // The type checker recognizes multiple positional entries as a union.
                                let entries: Vec<Spanned<Entry>> = type_exprs
                                    .into_iter()
                                    .map(|e| {
                                        let entry_span = e.span;
                                        Spanned::new(
                                            Entry {
                                                key: None,
                                                value: Rc::new(e),
                                            },
                                            entry_span,
                                        )
                                    })
                                    .collect();
                                Spanned::new(Expr::Dict(entries), dict_span(span_start))
                            };
                            let alias_expr = Expr::TypeAlias {
                                params,
                                body: Box::new(body),
                            };
                            let spanned_alias = Spanned::new(alias_expr, dict_span(span_start));
                            if let Err(push_err) = push_value(
                                &mut stack,
                                &mut current_document_expressions,
                                spanned_alias,
                            ) {
                                close_bracket_recover!(push_err);
                            }
                        }
                    }

                    StackFrame::TypeAssert {
                        annotation,
                        expr,
                        span_start,
                    } => match (annotation, expr) {
                        (None, _) => {
                            close_bracket_recover!(ParseError {
                                message: "type-assert form requires an annotation".to_string(),
                                span: Some(span),
                            });
                        }
                        (_, None) => {
                            close_bracket_recover!(ParseError {
                                message: "type-assert form requires an expression".to_string(),
                                span: Some(span),
                            });
                        }
                        (Some(annotation), Some(expr)) => {
                            use std::cell::RefCell;
                            let type_assert_expr = Expr::TypeAssert {
                                annotation,
                                expr: Box::new(expr),
                                resolved_type: RefCell::new(None),
                            };

                            let spanned_type_assert =
                                Spanned::new(type_assert_expr, dict_span(span_start));
                            if let Err(push_err) = push_value(
                                &mut stack,
                                &mut current_document_expressions,
                                spanned_type_assert,
                            ) {
                                close_bracket_recover!(push_err);
                            }
                        }
                    },

                    StackFrame::Quote { expr, span_start } => {
                        quote_depth -= 1; // Leaving a quote context
                        match expr {
                            None => {
                                close_bracket_recover!(ParseError {
                                    message: "quote form requires an expression".to_string(),
                                    span: Some(span),
                                });
                            }
                            Some(expr) => {
                                let quote_expr = Expr::Quote(Box::new(expr));
                                let spanned_quote = Spanned::new(quote_expr, dict_span(span_start));
                                if let Err(push_err) = push_value(
                                    &mut stack,
                                    &mut current_document_expressions,
                                    spanned_quote,
                                ) {
                                    close_bracket_recover!(push_err);
                                }
                            }
                        }
                    }

                    StackFrame::Unquote { expr, span_start } => {
                        quote_depth += 1; // Leaving an unquote context (back to quote context)
                        match expr {
                            None => {
                                close_bracket_recover!(ParseError {
                                    message: "unquote form requires an expression".to_string(),
                                    span: Some(span),
                                });
                            }
                            Some(expr) => {
                                let unquote_expr = Expr::Unquote(Box::new(expr));
                                let spanned_unquote =
                                    Spanned::new(unquote_expr, dict_span(span_start));
                                if let Err(push_err) = push_value(
                                    &mut stack,
                                    &mut current_document_expressions,
                                    spanned_unquote,
                                ) {
                                    close_bracket_recover!(push_err);
                                }
                            }
                        }
                    }

                    StackFrame::UnquoteSplice { expr, span_start } => {
                        quote_depth += 1; // Leaving an unquote-splice context (back to quote context)
                        match expr {
                            None => {
                                close_bracket_recover!(ParseError {
                                    message: "unquote-splice form requires an expression"
                                        .to_string(),
                                    span: Some(span),
                                });
                            }
                            Some(expr) => {
                                let unquote_splice_expr = Expr::UnquoteSplice(Box::new(expr));
                                let spanned_unquote_splice =
                                    Spanned::new(unquote_splice_expr, dict_span(span_start));
                                if let Err(push_err) = push_value(
                                    &mut stack,
                                    &mut current_document_expressions,
                                    spanned_unquote_splice,
                                ) {
                                    close_bracket_recover!(push_err);
                                }
                            }
                        }
                    }

                    StackFrame::DefMacro {
                        name,
                        transformer,
                        span_start,
                    } => match (name, transformer) {
                        (None, _) => {
                            close_bracket_recover!(ParseError {
                                message: "defmacro form requires a name".to_string(),
                                span: Some(span),
                            });
                        }
                        (Some(_), None) => {
                            close_bracket_recover!(ParseError {
                                message: "defmacro form requires a transformer expression"
                                    .to_string(),
                                span: Some(span),
                            });
                        }
                        (Some(name), Some(transformer)) => {
                            let defmacro_expr = Expr::DefMacro {
                                name,
                                transformer: Box::new(transformer),
                            };
                            let spanned_defmacro =
                                Spanned::new(defmacro_expr, dict_span(span_start));
                            if let Err(push_err) = push_value(
                                &mut stack,
                                &mut current_document_expressions,
                                spanned_defmacro,
                            ) {
                                close_bracket_recover!(push_err);
                            }
                        }
                    },

                    StackFrame::Match {
                        scrutinee,
                        arms,
                        pending_pattern,
                        span_start,
                    } => {
                        if scrutinee.is_none() {
                            close_bracket_recover!(ParseError {
                                message: "match form requires a scrutinee expression".to_string(),
                                span: Some(span),
                            });
                        }
                        if pending_pattern.is_some() {
                            close_bracket_recover!(ParseError {
                                message: "match form has unpaired pattern (missing body)"
                                    .to_string(),
                                span: Some(span),
                            });
                        }
                        if arms.is_empty() {
                            close_bracket_recover!(ParseError {
                                message: "match form requires at least one pattern-body pair"
                                    .to_string(),
                                span: Some(span),
                            });
                        }
                        let match_expr = Expr::Match {
                            scrutinee: Box::new(scrutinee.unwrap()),
                            arms,
                        };
                        let spanned_match = Spanned::new(match_expr, dict_span(span_start));
                        if let Err(push_err) =
                            push_value(&mut stack, &mut current_document_expressions, spanned_match)
                        {
                            close_bracket_recover!(push_err);
                        }
                    }

                    StackFrame::ClassDecl {
                        name,
                        params,
                        superclasses,
                        methods,
                        pending_key,
                        span_start,
                    } => {
                        if name.is_none() {
                            close_bracket_recover!(ParseError {
                                message: "class form requires a class name".to_string(),
                                span: Some(span),
                            });
                        } else if pending_key.is_some() {
                            close_bracket_recover!(ParseError {
                                message: "class form has incomplete method (key without value)"
                                    .to_string(),
                                span: Some(span),
                            });
                        } else {
                            let class_expr = Expr::ClassDecl {
                                name: name.unwrap(),
                                params,
                                superclasses,
                                methods: methods
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
                            };
                            let spanned_class = Spanned::new(class_expr, dict_span(span_start));
                            if let Err(push_err) = push_value(
                                &mut stack,
                                &mut current_document_expressions,
                                spanned_class,
                            ) {
                                close_bracket_recover!(push_err);
                            }
                        }
                    }

                    StackFrame::InstanceDecl {
                        class_name,
                        instance_type,
                        methods,
                        pending_key,
                        span_start,
                    } => {
                        if class_name.is_none() {
                            close_bracket_recover!(ParseError {
                                message: "instance form requires a class name".to_string(),
                                span: Some(span),
                            });
                        } else if instance_type.is_none() {
                            close_bracket_recover!(ParseError {
                                message: "instance form requires an instance type".to_string(),
                                span: Some(span),
                            });
                        } else if pending_key.is_some() {
                            close_bracket_recover!(ParseError {
                                message: "instance form has incomplete method (key without value)"
                                    .to_string(),
                                span: Some(span),
                            });
                        } else {
                            let instance_expr = Expr::InstanceDecl {
                                class_name: class_name.unwrap(),
                                instance_type: Box::new(instance_type.unwrap()),
                                methods: methods
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
                            };
                            let spanned_instance =
                                Spanned::new(instance_expr, dict_span(span_start));
                            if let Err(push_err) = push_value(
                                &mut stack,
                                &mut current_document_expressions,
                                spanned_instance,
                            ) {
                                close_bracket_recover!(push_err);
                            }
                        }
                    }

                    StackFrame::Pipe { .. } => {
                        // A Pipe frame is never opened by `[` — pipe is an infix operator
                        // between two expressions, not a bracket form. A CloseBracket here
                        // means the enclosing bracket form is being closed while a Pipe is
                        // on the stack, which indicates a malformed expression like `[x | ]`.
                        // Recover gracefully with a parse error instead of panicking.
                        close_bracket_recover!(ParseError {
                            message: "pipe operator '|' requires a right-hand expression"
                                .to_string(),
                            span: Some(span),
                        });
                    }
                }

                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::Colon => {
                // Key-value separator
                let colon_err: Option<ParseError> = match stack.last_mut() {
                    Some(StackFrame::Dict {
                        ref mut pending_key,
                        ..
                    }) => {
                        if pending_key.is_none() {
                            Some(ParseError {
                                message: "`:` without a key (expected key before `:`)".to_string(),
                                span: Some(span),
                            })
                        } else {
                            None // Pending key is set; next expression will be the value
                        }
                    }
                    Some(StackFrame::Call {
                        ref mut pending_key,
                        ..
                    }) => {
                        if pending_key.is_none() {
                            Some(ParseError {
                                message: "`:` without a name (expected bare word before `:` for named arg)".to_string(),
                                span: Some(span),
                            })
                        } else {
                            None // Pending key is set; next expression will be the value
                        }
                    }
                    Some(StackFrame::ClassDecl {
                        ref mut pending_key,
                        ..
                    }) => {
                        if pending_key.is_none() {
                            Some(ParseError {
                                message: "`:` without a method name (expected method: Type)"
                                    .to_string(),
                                span: Some(span),
                            })
                        } else {
                            None // Pending key is set; next expression will be the value
                        }
                    }
                    Some(StackFrame::InstanceDecl {
                        ref mut pending_key,
                        ..
                    }) => {
                        if pending_key.is_none() {
                            Some(ParseError {
                                message: "`:` without a method name (expected method: impl)"
                                    .to_string(),
                                span: Some(span),
                            })
                        } else {
                            None // Pending key is set; next expression will be the value
                        }
                    }
                    _ => Some(ParseError {
                        message: "`:` can only appear in dict, call, class, or instance forms"
                            .to_string(),
                        span: Some(span),
                    }),
                };
                if let Some(err) = colon_err {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_expressions,
                            &mut recovered_errors,
                        );
                        continue;
                    }
                    return Err(err);
                }
                i += 1;
                continue;
            }

            // Literals: collect as values, but detect colon-ahead for dict key position.
            Token::Int(n) => {
                let expr = Spanned::new(Expr::Int(*n), span);
                // Check if this integer is a potential dict key (e.g. [0: $x]).
                // Use peek_next_horizontal: a newline before `:` breaks key detection per spec.
                if let Some((Token::Colon, _)) = peek_next_horizontal(&token_vec, i) {
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
                if let Err(push_err) =
                    push_value(&mut stack, &mut current_document_expressions, expr)
                {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            push_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_expressions,
                            &mut recovered_errors,
                        );
                        continue;
                    }
                    return Err(push_err);
                }
                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::Float(f) => {
                let expr = Spanned::new(Expr::Float(*f), span);
                if let Err(push_err) =
                    push_value(&mut stack, &mut current_document_expressions, expr)
                {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            push_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_expressions,
                            &mut recovered_errors,
                        );
                        continue;
                    }
                    return Err(push_err);
                }
                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::BoolLit(b) => {
                let expr = Spanned::new(Expr::Bool(*b), span);
                if let Err(push_err) =
                    push_value(&mut stack, &mut current_document_expressions, expr)
                {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            push_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_expressions,
                            &mut recovered_errors,
                        );
                        continue;
                    }
                    return Err(push_err);
                }
                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::QuotedString(s) => {
                let expr = Spanned::new(Expr::Str(s.clone()), span);
                // Check if this quoted string is a potential dict key (e.g. ["key": value]).
                // Use peek_next_horizontal: a newline before `:` breaks key detection per spec.
                if let Some((Token::Colon, _)) = peek_next_horizontal(&token_vec, i) {
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
                if let Err(push_err) =
                    push_value(&mut stack, &mut current_document_expressions, expr)
                {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            push_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_expressions,
                            &mut recovered_errors,
                        );
                        continue;
                    }
                    return Err(push_err);
                }
                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::InterpolatedString(parts) => {
                // Desugar i"Hello $name" to [str "Hello " name]
                let expr = desugar_interpolated_string(parts, span)?;
                if let Err(push_err) =
                    push_value(&mut stack, &mut current_document_expressions, expr)
                {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            push_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_expressions,
                            &mut recovered_errors,
                        );
                        continue;
                    }
                    return Err(push_err);
                }
                last_significant_span = Some(span);
                i += 1;
                continue;
            }

            Token::Identifier(s) => {
                // Check for annotation: word@Type
                if i + 1 < token_vec.len() && matches!(&token_vec[i + 1].node, Token::ImmediateAt) {
                    // Annotated bare word
                    let name = s.clone();
                    let name_span = span;
                    i += 1; // Move to ImmediateAt token
                    match parse_annotation(
                        &token_vec,
                        i,
                        &mut leading_comments,
                        &mut blank_before,
                        input,
                        Some(&mut recovered_errors),
                    ) {
                        Ok((annotation, next_i)) => {
                            i = next_i;
                            let full_span = Span {
                                start: name_span.start,
                                end: annotation.span.end,
                            };
                            let expr =
                                Spanned::new(Expr::Annotated { name, annotation }, full_span);
                            // If the annotated expression is immediately followed by ':', treat
                            // it as a dict key candidate (e.g. [x@Number: 42]).
                            // After parse_annotation, `i` points to the token right after the
                            // annotation type, so check token_vec[i] directly (not +1).
                            let next_is_colon =
                                i < token_vec.len() && matches!(&token_vec[i].node, Token::Colon);
                            if next_is_colon {
                                if let Some(StackFrame::Dict {
                                    ref mut pending_key,
                                    ..
                                }) = stack.last_mut()
                                {
                                    *pending_key = Some(expr);
                                    last_significant_span = Some(full_span);
                                    continue;
                                }
                            }
                            if let Err(push_err) =
                                push_value(&mut stack, &mut current_document_expressions, expr)
                            {
                                if !stack.is_empty() {
                                    i = recover_from_bracket_error(
                                        push_err,
                                        full_span,
                                        &token_vec,
                                        i,
                                        &mut stack,
                                        &mut current_document_expressions,
                                        &mut recovered_errors,
                                    );
                                    continue;
                                }
                                return Err(push_err);
                            }
                            last_significant_span = Some(full_span);
                            continue;
                        }
                        Err(ann_err) => {
                            if !stack.is_empty() {
                                i = recover_from_bracket_error(
                                    ann_err,
                                    name_span,
                                    &token_vec,
                                    i,
                                    &mut stack,
                                    &mut current_document_expressions,
                                    &mut recovered_errors,
                                );
                                continue;
                            }
                            return Err(ann_err);
                        }
                    }
                }

                // Identifiers are variable references in value position, string keys in key position.
                // Check if this is a potential key (next token is colon).
                // Use peek_next_horizontal: a newline before `:` breaks key detection per spec.
                if let Some((Token::Colon, _)) = peek_next_horizontal(&token_vec, i) {
                    // This identifier is a key candidate
                    match stack.last_mut() {
                        Some(StackFrame::Dict {
                            ref mut pending_key,
                            ..
                        }) => {
                            // Dict key: Expr::Str
                            let key_expr = Spanned::new(Expr::Str(s.clone()), span);
                            *pending_key = Some(key_expr);
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        Some(StackFrame::Call {
                            ref mut pending_key,
                            ..
                        }) => {
                            // Named arg key — store the string name
                            *pending_key = Some((s.clone(), span));
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        Some(StackFrame::ClassDecl {
                            ref mut pending_key,
                            ..
                        }) => {
                            // Method name in class: Expr::Str
                            let key_expr = Spanned::new(Expr::Str(s.clone()), span);
                            *pending_key = Some(key_expr);
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        Some(StackFrame::InstanceDecl {
                            ref mut pending_key,
                            ..
                        }) => {
                            // Method name in instance: Expr::Str
                            let key_expr = Spanned::new(Expr::Str(s.clone()), span);
                            *pending_key = Some(key_expr);
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                        _ => {
                            // Not in dict/call context; treat as normal value (VarRef)
                            let expr = Spanned::new(Expr::var_ref(s.clone()), span);
                            if let Err(push_err) =
                                push_value(&mut stack, &mut current_document_expressions, expr)
                            {
                                if !stack.is_empty() {
                                    i = recover_from_bracket_error(
                                        push_err,
                                        span,
                                        &token_vec,
                                        i + 1,
                                        &mut stack,
                                        &mut current_document_expressions,
                                        &mut recovered_errors,
                                    );
                                    continue;
                                }
                                return Err(push_err);
                            }
                            last_significant_span = Some(span);
                            i += 1;
                            continue;
                        }
                    }
                } else {
                    // Not followed by colon; regular value (VarRef)
                    let expr = Spanned::new(Expr::var_ref(s.clone()), span);
                    if let Err(push_err) =
                        push_value(&mut stack, &mut current_document_expressions, expr)
                    {
                        if !stack.is_empty() {
                            i = recover_from_bracket_error(
                                push_err,
                                span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_expressions,
                                &mut recovered_errors,
                            );
                            continue;
                        }
                        return Err(push_err);
                    }
                    last_significant_span = Some(span);
                    i += 1;
                    continue;
                }
            }

            Token::EscapedRef(name) => {
                let expr = Spanned::new(Expr::var_ref(name.clone()), span);
                // Check if this VarRef is a potential dict key (followed by colon).
                // Use peek_next_horizontal: a newline before `:` breaks key detection per spec.
                if let Some((Token::Colon, _)) = peek_next_horizontal(&token_vec, i) {
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
                if let Err(push_err) =
                    push_value(&mut stack, &mut current_document_expressions, expr)
                {
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            push_err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_expressions,
                            &mut recovered_errors,
                        );
                        continue;
                    }
                    return Err(push_err);
                }
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

                // Finalize current document (even if empty) with previously parsed header
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
                documents.push(Spanned::new(
                    Document {
                        expressions: exprs,
                        name: next_doc_name.take(),
                        output_type: next_doc_output_type.take(),
                        expects: next_doc_expects.take(),
                    },
                    doc_span,
                ));

                // Parse section header (Phase 1): consume tokens until Newline or Semicolon
                // Format: --- %name@Type expects: Type
                // This header applies to the NEXT document
                i += 1;

                while i < token_vec.len() {
                    match &token_vec[i].node {
                        Token::Newline | Token::Semicolon => {
                            // End of header
                            i += 1;
                            break;
                        }
                        Token::Identifier(s) if s.starts_with('%') => {
                            // Section name: %name
                            let name_after_percent = &s[1..];
                            if name_after_percent.is_empty() {
                                return Err(ParseError {
                                    message: "bare % with no identifier is not allowed in section headers".to_string(),
                                    span: Some(token_vec[i].span),
                                });
                            }
                            if next_doc_name.is_some() {
                                return Err(ParseError {
                                    message: "duplicate section name in header".to_string(),
                                    span: Some(token_vec[i].span),
                                });
                            }
                            next_doc_name_span = Some(token_vec[i].span);
                            next_doc_name = Some(name_after_percent.to_string());

                            // Check for @Type annotation on the name
                            if i + 1 < token_vec.len()
                                && matches!(&token_vec[i + 1].node, Token::ImmediateAt | Token::At)
                            {
                                i += 1; // Move to @ token
                                match parse_annotation(
                                    &token_vec,
                                    i,
                                    &mut leading_comments,
                                    &mut blank_before,
                                    input,
                                    Some(&mut recovered_errors),
                                ) {
                                    Ok((annotation, next_i)) => {
                                        next_doc_output_type = Some(annotation);
                                        i = next_i;
                                        continue;
                                    }
                                    Err(ann_err) => {
                                        return Err(ann_err);
                                    }
                                }
                            }

                            i += 1;
                        }
                        Token::Identifier(s) if s == "expects" => {
                            // expects: pragma
                            if next_doc_expects.is_some() {
                                return Err(ParseError {
                                    message: "duplicate expects: pragma in header".to_string(),
                                    span: Some(token_vec[i].span),
                                });
                            }
                            i += 1;
                            // Expect colon
                            if i >= token_vec.len() || !matches!(&token_vec[i].node, Token::Colon) {
                                return Err(ParseError {
                                    message: "expected ':' after 'expects' pragma".to_string(),
                                    span: Some(if i < token_vec.len() {
                                        token_vec[i].span
                                    } else {
                                        token_vec[i - 1].span
                                    }),
                                });
                            }
                            i += 1;
                            // Parse annotation
                            match parse_annotation(
                                &token_vec,
                                i,
                                &mut leading_comments,
                                &mut blank_before,
                                input,
                                Some(&mut recovered_errors),
                            ) {
                                Ok((annotation, next_i)) => {
                                    next_doc_expects = Some(annotation);
                                    i = next_i;
                                }
                                Err(ann_err) => {
                                    return Err(ann_err);
                                }
                            }
                        }
                        Token::At | Token::ImmediateAt => {
                            // Standalone @Type annotation (no name)
                            if next_doc_output_type.is_some() {
                                return Err(ParseError {
                                    message: "duplicate output type annotation in header"
                                        .to_string(),
                                    span: Some(token_vec[i].span),
                                });
                            }
                            match parse_annotation(
                                &token_vec,
                                i,
                                &mut leading_comments,
                                &mut blank_before,
                                input,
                                Some(&mut recovered_errors),
                            ) {
                                Ok((annotation, next_i)) => {
                                    next_doc_output_type = Some(annotation);
                                    i = next_i;
                                }
                                Err(ann_err) => {
                                    return Err(ann_err);
                                }
                            }
                        }
                        _ => {
                            // Unexpected token in header
                            return Err(ParseError {
                                message: format!(
                                    "unexpected token in section header: {:?}",
                                    token_vec[i].node
                                ),
                                span: Some(token_vec[i].span),
                            });
                        }
                    }
                }

                // Check for duplicate section names across the file.
                // Use next_doc_name_span (the %name token's span) rather than span
                // (the --- separator's span) so the error underlines the duplicate name.
                if let Some(ref name) = next_doc_name {
                    for doc in &documents {
                        if let Some(ref existing_name) = doc.node.name {
                            if existing_name == name {
                                return Err(ParseError {
                                    message: format!("duplicate section name '%{}' in file", name),
                                    span: next_doc_name_span,
                                });
                            }
                        }
                    }
                }

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
                    let rc = current_document_expressions.pop().unwrap();
                    Rc::try_unwrap(rc).unwrap_or_else(|rc| (*rc).clone())
                } else {
                    // Inside a frame — pop the last value from the current frame
                    match pop_last_value_from_frame(&mut stack, span) {
                        Ok(t) => t,
                        Err(pop_err) => {
                            i = recover_from_bracket_error(
                                pop_err,
                                span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_expressions,
                                &mut recovered_errors,
                            );
                            continue;
                        }
                    }
                };

                i += 1; // Consume the Dot

                // Skip whitespace
                i +=
                    skip_whitespace_tokens(&token_vec, i, &mut leading_comments, &mut blank_before);

                if i >= token_vec.len() {
                    let err = ParseError {
                        message: "expected field name after '.'".to_string(),
                        span: Some(span),
                    };
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            err,
                            span,
                            &token_vec,
                            i,
                            &mut stack,
                            &mut current_document_expressions,
                            &mut recovered_errors,
                        );
                        continue;
                    }
                    return Err(err);
                }

                // Next token must be an Identifier or Int for the field name
                match &token_vec[i].node {
                    Token::Identifier(field) => {
                        let field_key = crate::ast::DotKey::Ident(field.clone());
                        let dot_access_span = Span {
                            start: target.span.start,
                            end: token_vec[i].span.end,
                        };

                        let dot_access_expr = Expr::DotAccess {
                            expr: Box::new(target),
                            field: field_key,
                        };

                        let spanned_access = Spanned::new(dot_access_expr, dot_access_span);

                        if stack.is_empty() {
                            current_document_expressions.push(Rc::new(spanned_access.clone()));
                        } else if let Err(push_err) = push_value(
                            &mut stack,
                            &mut current_document_expressions,
                            spanned_access,
                        ) {
                            i = recover_from_bracket_error(
                                push_err,
                                dot_access_span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_expressions,
                                &mut recovered_errors,
                            );
                            continue;
                        }

                        i += 1;
                        continue;
                    }
                    Token::Int(n) => {
                        let field_key = crate::ast::DotKey::Int(*n);
                        let dot_access_span = Span {
                            start: target.span.start,
                            end: token_vec[i].span.end,
                        };

                        let dot_access_expr = Expr::DotAccess {
                            expr: Box::new(target),
                            field: field_key,
                        };

                        let spanned_access = Spanned::new(dot_access_expr, dot_access_span);

                        if stack.is_empty() {
                            current_document_expressions.push(Rc::new(spanned_access.clone()));
                        } else if let Err(push_err) = push_value(
                            &mut stack,
                            &mut current_document_expressions,
                            spanned_access,
                        ) {
                            i = recover_from_bracket_error(
                                push_err,
                                dot_access_span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_expressions,
                                &mut recovered_errors,
                            );
                            continue;
                        }

                        i += 1;
                        continue;
                    }
                    _ => {
                        let err = ParseError {
                            message: format!(
                                "expected field name (identifier or integer) after '.', found {:?}",
                                token_vec[i].node
                            ),
                            span: Some(token_vec[i].span),
                        };
                        if !stack.is_empty() {
                            i = recover_from_bracket_error(
                                err,
                                span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_expressions,
                                &mut recovered_errors,
                            );
                            continue;
                        }
                        return Err(err);
                    }
                }
            }

            Token::Pipe => {
                // Pipe operator: pop the preceding expression (LHS) and push a Pipe frame
                // Precedence: . > call > |
                // Inside [...], | terminates call argument accumulation
                let lhs = if stack.is_empty() {
                    if current_document_expressions.is_empty() {
                        return Err(ParseError {
                            message: "pipe operator requires a left-hand expression before '|'"
                                .to_string(),
                            span: Some(span),
                        });
                    }
                    let rc = current_document_expressions.pop().unwrap();
                    Rc::try_unwrap(rc).unwrap_or_else(|rc| (*rc).clone())
                } else {
                    // Inside a frame — pop the last value from the current frame
                    match pop_last_value_from_frame(&mut stack, span) {
                        Ok(lhs_expr) => lhs_expr,
                        Err(pop_err) => {
                            i = recover_from_bracket_error(
                                pop_err,
                                span,
                                &token_vec,
                                i + 1,
                                &mut stack,
                                &mut current_document_expressions,
                                &mut recovered_errors,
                            );
                            continue;
                        }
                    }
                };

                // Push a Pipe frame to wait for the RHS expression
                stack.push(StackFrame::Pipe {
                    lhs,
                    span_start: span.start,
                });

                i += 1; // Consume the Pipe token
                continue;
            }

            Token::At | Token::ImmediateAt => {
                // Check context: if we're in a TypeAssert frame and don't have annotation yet, parse it
                let is_type_assert_no_ann = matches!(
                    stack.last(),
                    Some(StackFrame::TypeAssert {
                        annotation: None,
                        ..
                    })
                );
                if is_type_assert_no_ann {
                    match stack.last_mut() {
                        Some(StackFrame::TypeAssert {
                            ref mut annotation, ..
                        }) => match parse_annotation(
                            &token_vec,
                            i,
                            &mut leading_comments,
                            &mut blank_before,
                            input,
                            Some(&mut recovered_errors),
                        ) {
                            Ok((ann, next_i)) => {
                                *annotation = Some(ann);
                                i = next_i;
                                continue;
                            }
                            Err(ann_err) => {
                                i = recover_from_bracket_error(
                                    ann_err,
                                    span,
                                    &token_vec,
                                    i + 1,
                                    &mut stack,
                                    &mut current_document_expressions,
                                    &mut recovered_errors,
                                );
                                continue;
                            }
                        },
                        _ => unreachable!("checked above"),
                    }
                } else {
                    let err = ParseError {
                        message:
                            "@ annotations outside type-assert or param contexts not yet supported"
                                .to_string(),
                        span: Some(span),
                    };
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_expressions,
                            &mut recovered_errors,
                        );
                        continue;
                    }
                    return Err(err);
                }
            }

            Token::Ellipsis => {
                // Rest/open-row marker: `...` or `...name` inside a dict expression.
                // Only valid inside a Dict frame (type expression context).
                // Produces Expr::Rest(None) for anonymous open row, Expr::Rest(Some(name)) for named.
                if let Some(StackFrame::Dict { .. }) = stack.last() {
                    let ellipsis_span = span;
                    i += 1; // Consume ellipsis
                    i += skip_whitespace_tokens(
                        &token_vec,
                        i,
                        &mut leading_comments,
                        &mut blank_before,
                    );
                    // Check for optional name after ...
                    let (rest_name, rest_end) = if i < token_vec.len() {
                        match &token_vec[i].node {
                            Token::Identifier(name) => {
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
                    if let Err(push_err) =
                        push_value(&mut stack, &mut current_document_expressions, rest_expr)
                    {
                        i = recover_from_bracket_error(
                            push_err,
                            rest_end,
                            &token_vec,
                            i + name_advance,
                            &mut stack,
                            &mut current_document_expressions,
                            &mut recovered_errors,
                        );
                        continue;
                    }
                    last_significant_span = Some(rest_end);
                    i += name_advance;
                    continue;
                } else {
                    let err = ParseError {
                        message: "variadic/rest markers not yet supported outside dict context"
                            .to_string(),
                        span: Some(span),
                    };
                    if !stack.is_empty() {
                        i = recover_from_bracket_error(
                            err,
                            span,
                            &token_vec,
                            i + 1,
                            &mut stack,
                            &mut current_document_expressions,
                            &mut recovered_errors,
                        );
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }

    // Check for unclosed brackets
    if !stack.is_empty() {
        // Get the innermost unclosed bracket's position
        let innermost_frame = stack.last().unwrap();
        let start_pos = match innermost_frame {
            StackFrame::Dict { span_start, .. } => *span_start,
            StackFrame::Call { span_start, .. } => *span_start,
            StackFrame::Fn { span_start, .. } => *span_start,
            StackFrame::TypeAlias { span_start, .. } => *span_start,
            StackFrame::TypeAssert { span_start, .. } => *span_start,
            StackFrame::Quote { span_start, .. } => *span_start,
            StackFrame::Unquote { span_start, .. } => *span_start,
            StackFrame::UnquoteSplice { span_start, .. } => *span_start,
            StackFrame::DefMacro { span_start, .. } => *span_start,
            StackFrame::Match { span_start, .. } => *span_start,
            StackFrame::ClassDecl { span_start, .. } => *span_start,
            StackFrame::InstanceDecl { span_start, .. } => *span_start,
            StackFrame::Pipe { span_start, .. } => *span_start,
        };

        let unclosed_span = Span {
            start: start_pos,
            end: Position {
                offset: start_pos.offset + 1,
                line: start_pos.line,
                column: start_pos.column + 1,
            },
        };

        let count = stack.len();
        let message = match innermost_frame {
            StackFrame::Pipe { .. } => {
                "pipe operator '|' requires a right-hand expression".to_string()
            }
            _ if count == 1 => "unclosed bracket".to_string(),
            _ => format!("{} unclosed brackets", count),
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
            name: next_doc_name.take(),
            output_type: next_doc_output_type.take(),
            expects: next_doc_expects.take(),
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
            name: next_doc_name.take(),
            output_type: next_doc_output_type.take(),
            expects: next_doc_expects.take(),
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
        source: input.to_string(),
        leading_comments,
        trailing_comments,
        blank_before,
        errors: recovered_errors,
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
            Ok(Rc::try_unwrap(last_entry.value).unwrap_or_else(|rc| (*rc).clone()))
        }
        Some(StackFrame::Call { ref mut args, .. }) => {
            if args.is_empty() {
                return Err(ParseError {
                    message: "dot access requires a target before '.'".to_string(),
                    span: Some(span),
                });
            }
            match args.pop().unwrap() {
                CallArg::Positional(expr) => {
                    Ok(Rc::try_unwrap(expr).unwrap_or_else(|rc| (*rc).clone()))
                }
                CallArg::Named(name, _expr) => Err(ParseError {
                    message: format!("dot access cannot operate on named argument '{}'", name),
                    span: Some(span),
                }),
            }
        }
        Some(StackFrame::Fn { ref mut body, .. }) => {
            if let Some(b) = body.pop() {
                Ok(b)
            } else {
                Err(ParseError {
                    message: "dot access requires a target before '.'".to_string(),
                    span: Some(span),
                })
            }
        }
        Some(StackFrame::TypeAlias { type_exprs: _, .. }) => {
            // TypeAlias frames don't support dot access in type context.
            // This case should be unreachable since dot access only applies to value expressions.
            Err(ParseError {
                message: "dot access is not valid in type alias expressions".to_string(),
                span: Some(span),
            })
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
        Some(StackFrame::Quote { ref mut expr, .. }) => {
            if let Some(e) = expr.take() {
                Ok(e)
            } else {
                Err(ParseError {
                    message: "dot access requires a target before '.'".to_string(),
                    span: Some(span),
                })
            }
        }
        Some(StackFrame::Unquote { ref mut expr, .. }) => {
            if let Some(e) = expr.take() {
                Ok(e)
            } else {
                Err(ParseError {
                    message: "dot access requires a target before '.'".to_string(),
                    span: Some(span),
                })
            }
        }
        Some(StackFrame::UnquoteSplice { ref mut expr, .. }) => {
            if let Some(e) = expr.take() {
                Ok(e)
            } else {
                Err(ParseError {
                    message: "dot access requires a target before '.'".to_string(),
                    span: Some(span),
                })
            }
        }
        Some(StackFrame::Match {
            ref mut scrutinee,
            ref mut arms,
            ref mut pending_pattern,
            ..
        }) => {
            if pending_pattern.is_some() {
                // Last push was a pattern (already converted) — can't retroactively transform
                Err(ParseError {
                    message: "dot access on a pattern is not supported".to_string(),
                    span: Some(span),
                })
            } else if !arms.is_empty() {
                // Pop the last arm, restore its pattern+guard as pending, return body
                let last_arm = arms.pop().unwrap();
                *pending_pattern = Some((last_arm.pattern, last_arm.guard));
                Ok(*last_arm.body)
            } else if let Some(s) = scrutinee.take() {
                Ok(s)
            } else {
                Err(ParseError {
                    message: "dot access requires a target before '.'".to_string(),
                    span: Some(span),
                })
            }
        }
        Some(StackFrame::DefMacro { .. }) => Err(ParseError {
            message: "dot access is not valid inside defmacro form".to_string(),
            span: Some(span),
        }),
        Some(StackFrame::ClassDecl { .. }) => Err(ParseError {
            message: "dot access is not valid inside class form".to_string(),
            span: Some(span),
        }),
        Some(StackFrame::InstanceDecl { .. }) => Err(ParseError {
            message: "dot access is not valid inside instance form".to_string(),
            span: Some(span),
        }),
        Some(StackFrame::Pipe { .. }) => Err(ParseError {
            message: "pipe operator '|' requires a right-hand expression".to_string(),
            span: Some(span),
        }),
        None => Err(ParseError {
            message: "dot access requires a target before '.'".to_string(),
            span: Some(span),
        }),
    }
}

/// Helper: push an expression to the parent frame or current document.
/// Convert an expression to a pattern for match arms.
///
/// Pattern syntax (basic implementation):
/// - `_` → Wildcard
/// - Bare lowercase identifier → Variable binding
/// - Bare uppercase identifier → TypeTag (Int, Str, Dict, etc.)
/// - Int/Float/Bool/Str literal → Literal pattern
///
/// TODO: Pin patterns (`$name`) require tracking whether the VarRef came from
/// Token::EscapedRef or Token::Identifier, which is lost after expr parsing.
/// Either parse patterns directly from tokens or add escaped flag to VarRef.
/// Extract all variable names bound by a pattern
fn pattern_variables(pattern: &Pattern) -> std::collections::HashSet<String> {
    let mut vars = std::collections::HashSet::new();
    collect_pattern_variables(pattern, &mut vars);
    vars
}

/// Recursively collect all variable bindings from a pattern
fn collect_pattern_variables(pattern: &Pattern, vars: &mut std::collections::HashSet<String>) {
    match pattern {
        Pattern::Variable(name) => {
            vars.insert(name.clone());
        }
        Pattern::Dict { fields, .. } => {
            for (_, field_pattern) in fields {
                collect_pattern_variables(&field_pattern.node, vars);
            }
        }
        Pattern::Seq { head, tail } => {
            collect_pattern_variables(&head.node, vars);
            collect_pattern_variables(&tail.node, vars);
        }
        Pattern::Constructor { binding, .. } => {
            if let Some(binding_pattern) = binding {
                collect_pattern_variables(&binding_pattern.node, vars);
            }
        }
        Pattern::Or(patterns) => {
            // For or-patterns, we only collect from the first branch
            // (all branches must bind the same variables, verified separately)
            if let Some(first) = patterns.first() {
                collect_pattern_variables(&first.node, vars);
            }
        }
        Pattern::Wildcard | Pattern::TypeTag(_) | Pattern::Literal(_) | Pattern::Pin(_) => {
            // These don't bind variables
        }
    }
}

/// Extract guard expression from annotation if present
fn extract_guard(annotation: &Spanned<Annotation>) -> Option<Box<Spanned<Expr>>> {
    match &annotation.node {
        Annotation::PropertyDict(props) => {
            for entry in props {
                if let Some(ref key_expr) = entry.node.key {
                    match &key_expr.node {
                        Expr::VarRef { name, .. } if name == "is" => {
                            return Some(Box::new((*entry.node.value).clone()));
                        }
                        Expr::Str(s) if s == "is" => {
                            return Some(Box::new((*entry.node.value).clone()));
                        }
                        _ => {}
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn expr_to_pattern(expr: Spanned<Expr>) -> Result<Spanned<Pattern>, ParseError> {
    expr_to_pattern_with_guard(expr).map(|(pat, _)| pat)
}

/// Convert expression to pattern, extracting guard if present
fn expr_to_pattern_with_guard(
    expr: Spanned<Expr>,
) -> Result<(Spanned<Pattern>, Option<Box<Spanned<Expr>>>), ParseError> {
    let span = expr.span;
    let (pattern, guard) = match expr.node {
        // Handle Pipe as or-pattern separator
        Expr::Pipe { lhs, rhs } => {
            let (left_pat, left_guard) = expr_to_pattern_with_guard((*lhs).clone())?;
            let (right_pat, right_guard) = expr_to_pattern_with_guard((*rhs).clone())?;

            // Or-patterns can't have guards on individual branches
            if left_guard.is_some() || right_guard.is_some() {
                return Err(ParseError {
                    message:
                        "or-pattern branches cannot have guards (use guard on the whole pattern)"
                            .to_string(),
                    span: Some(span),
                });
            }

            // Check that both branches bind the same set of variables
            let left_vars = pattern_variables(&left_pat.node);
            let right_vars = pattern_variables(&right_pat.node);
            if left_vars != right_vars {
                let missing_left: Vec<_> = right_vars.difference(&left_vars).collect();
                let missing_right: Vec<_> = left_vars.difference(&right_vars).collect();
                let mut msg = "or-pattern branches must bind the same variables".to_string();
                if !missing_left.is_empty() {
                    msg.push_str(&format!(
                        " (left branch missing: {})",
                        missing_left
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if !missing_right.is_empty() {
                    msg.push_str(&format!(
                        " (right branch missing: {})",
                        missing_right
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                return Err(ParseError {
                    message: msg,
                    span: Some(span),
                });
            }

            (Pattern::Or(vec![left_pat, right_pat]), None)
        }
        // Handle annotated patterns (e.g., n@Int, n@[is: pred])
        Expr::Annotated { name, annotation } => {
            // Extract guard from `is:` annotation
            let guard = extract_guard(&annotation);

            // Determine the base pattern
            let base_pattern = if name == "_" {
                Pattern::Wildcard
            } else if name.chars().next().map_or(false, |c| c.is_lowercase()) {
                Pattern::Variable(name.clone())
            } else if name.chars().next().map_or(false, |c| c.is_uppercase()) {
                Pattern::TypeTag(name.clone())
            } else {
                return Err(ParseError {
                    message: format!(
                        "invalid pattern: '{}' (must start with lowercase, uppercase, or be '_')",
                        name
                    ),
                    span: Some(span),
                });
            };

            (base_pattern, guard)
        }
        Expr::VarRef { name, .. } if name == "_" => (Pattern::Wildcard, None),
        Expr::VarRef { name, .. } if name.chars().next().map_or(false, |c| c.is_lowercase()) => {
            (Pattern::Variable(name), None)
        }
        Expr::VarRef { name, .. } if name.chars().next().map_or(false, |c| c.is_uppercase()) => {
            (Pattern::TypeTag(name), None)
        }
        Expr::VarRef { name, .. } => {
            // Other cases (e.g., names starting with special chars like %)
            return Err(ParseError {
                message: format!(
                    "invalid pattern: '{}' (must start with lowercase, uppercase, or be '_')",
                    name
                ),
                span: Some(span),
            });
        }
        Expr::Int(n) => (Pattern::Literal(LiteralPattern::Int(n)), None),
        Expr::Float(f) => (Pattern::Literal(LiteralPattern::Float(f)), None),
        Expr::Bool(b) => (Pattern::Literal(LiteralPattern::Bool(b)), None),
        Expr::Str(s) => (Pattern::Literal(LiteralPattern::Str(s)), None),
        Expr::Dict(entries) => {
            // Dict pattern: [key1: pat1  key2: pat2] or [key: pat ...]
            // Check for `seq` keyword for seq patterns: [seq h t]
            // Seq pattern has 3 auto-indexed entries: "seq" (bare word), h, t
            if entries.len() == 3 {
                if let Some(first_entry) = entries.first() {
                    // Check if first entry is auto-indexed and is VarRef("seq")
                    if first_entry.node.key.is_none() {
                        if let Expr::VarRef { ref name, .. } = first_entry.node.value.node {
                            if name == "seq" {
                                // This is a seq pattern: [seq h t]
                                if let Some(second_entry) = entries.get(1) {
                                    if let Some(third_entry) = entries.get(2) {
                                        let head_pat =
                                            expr_to_pattern((*second_entry.node.value).clone())?;
                                        let tail_pat =
                                            expr_to_pattern((*third_entry.node.value).clone())?;
                                        return Ok((
                                            Spanned::new(
                                                Pattern::Seq {
                                                    head: Box::new(head_pat),
                                                    tail: Box::new(tail_pat),
                                                },
                                                span,
                                            ),
                                            None,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Regular dict pattern
            let mut fields = Vec::new();
            let mut has_rest = true; // Default to open matching (extra keys allowed)

            for entry in entries {
                if let Expr::Rest(ref _rest_expr) = entry.node.value.node {
                    // This is a `...` rest marker (explicit open matching)
                    has_rest = true;
                    continue;
                }

                let key_str = if let Some(ref k) = entry.node.key {
                    match &k.node {
                        Expr::VarRef { ref name, .. } => name.clone(),
                        Expr::Str(s) => s.clone(),
                        _ => {
                            return Err(ParseError {
                                message: "dict pattern key must be an identifier or string"
                                    .to_string(),
                                span: Some(k.span),
                            });
                        }
                    }
                } else {
                    return Err(ParseError {
                        message: "dict pattern requires named fields (auto-indexed entries not supported)".to_string(),
                        span: Some(entry.span),
                    });
                };

                let value_pattern = expr_to_pattern((*entry.node.value).clone())?;
                fields.push((key_str, value_pattern));
            }

            (
                Pattern::Dict {
                    fields,
                    rest: has_rest,
                },
                None,
            )
        }
        Expr::Call {
            func,
            args,
            named_args,
            ..
        } if named_args.is_empty() => {
            // Check if this is a special pattern form: [seq h t] or [Constructor payload]
            if let Expr::VarRef { ref name, .. } = func.node {
                match (name.as_str(), args.len()) {
                    ("seq", 2) => {
                        // [seq h t] — seq pattern
                        let head_pat = expr_to_pattern((*args[0]).clone())?;
                        let tail_pat = expr_to_pattern((*args[1]).clone())?;
                        (
                            Pattern::Seq {
                                head: Box::new(head_pat),
                                tail: Box::new(tail_pat),
                            },
                            None,
                        )
                    }
                    (_, 1) if name.chars().next().map_or(false, |c| c.is_uppercase()) => {
                        // [Constructor payload] — nominal variant payload pattern
                        let payload_pat = expr_to_pattern((*args[0]).clone())?;
                        (
                            Pattern::Constructor {
                                tag: name.clone(),
                                binding: Some(Box::new(payload_pat)),
                            },
                            None,
                        )
                    }
                    _ => {
                        return Err(ParseError {
                            message: "invalid pattern: expected identifier, literal, dict, or _"
                                .to_string(),
                            span: Some(span),
                        });
                    }
                }
            } else {
                return Err(ParseError {
                    message: "invalid pattern: expected identifier, literal, dict, or _"
                        .to_string(),
                    span: Some(span),
                });
            }
        }
        _ => {
            return Err(ParseError {
                message: "invalid pattern: expected identifier, literal, dict, or _".to_string(),
                span: Some(span),
            });
        }
    };
    Ok((Spanned::new(pattern, span), guard))
}

fn push_expr_to_parent(
    stack: &mut Vec<StackFrame>,
    current_document_expressions: &mut Vec<Rc<Spanned<Expr>>>,
    expr: Spanned<Expr>,
) -> Result<(), ParseError> {
    if stack.is_empty() {
        current_document_expressions.push(Rc::new(expr));
        Ok(())
    } else {
        match stack.last_mut() {
            Some(StackFrame::Dict {
                ref mut entries, ..
            }) => {
                entries.push(Entry {
                    key: None,
                    value: Rc::new(expr),
                });
                Ok(())
            }
            Some(StackFrame::Call { ref mut args, .. }) => {
                args.push(CallArg::Positional(Rc::new(expr)));
                Ok(())
            }
            Some(StackFrame::Fn { ref mut body, .. }) => {
                body.push(expr);
                Ok(())
            }
            Some(StackFrame::TypeAlias {
                ref mut params,
                ref mut type_exprs,
                ..
            }) => {
                // First expression: check if it's a parameter list
                if params.is_empty() && type_exprs.is_empty() {
                    // Try to parse as parameter list: [a b c]
                    // Case 1: Dict with auto-indexed lowercase identifiers
                    if let Expr::Dict(entries) = &expr.node {
                        // Check if all entries are auto-indexed identifiers (no colons)
                        let all_params = entries.iter().all(|entry| {
                            entry.node.key.is_none()
                                && matches!(&entry.node.value.node, Expr::VarRef { name, .. } if name.chars().all(|c| c.is_lowercase() || c == '_'))
                        });

                        if all_params {
                            // Extract parameter names
                            for entry in entries {
                                if let Expr::VarRef { name, .. } = &entry.node.value.node {
                                    params.push(name.clone());
                                }
                            }
                            return Ok(());
                        }
                    }
                    // Case 2: Implied call [a b c] parsed as Call { func: VarRef("a"), args: [VarRef("b"), ...] }
                    // When all identifiers are lowercase, this is a parameter list, not a call.
                    if let Expr::Call {
                        implied: true,
                        func,
                        args,
                        ..
                    } = &expr.node
                    {
                        if let Expr::VarRef {
                            name: func_name, ..
                        } = &func.node
                        {
                            if func_name.chars().all(|c| c.is_lowercase() || c == '_') {
                                let all_lowercase_args = args.iter().all(|arg| {
                                    matches!(&arg.node, Expr::VarRef { name, .. } if name.chars().all(|c| c.is_lowercase() || c == '_'))
                                });
                                if all_lowercase_args {
                                    params.push(func_name.clone());
                                    for arg in args {
                                        if let Expr::VarRef { name, .. } = &arg.node {
                                            params.push(name.clone());
                                        }
                                    }
                                    return Ok(());
                                }
                            }
                        }
                    }
                }

                // Not a parameter list (or already have params) — this is a type expression entry.
                // Multi-entry `[type T1 T2 ...]` accumulates all positional type expressions.
                type_exprs.push(expr);
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
            Some(StackFrame::Quote {
                expr: ref mut quote_expr,
                ..
            }) => {
                if quote_expr.is_some() {
                    return Err(ParseError {
                        message: "quote form can only have one expression".to_string(),
                        span: Some(expr.span),
                    });
                }
                *quote_expr = Some(expr);
                Ok(())
            }
            Some(StackFrame::Unquote {
                expr: ref mut unquote_expr,
                ..
            }) => {
                if unquote_expr.is_some() {
                    return Err(ParseError {
                        message: "unquote form can only have one expression".to_string(),
                        span: Some(expr.span),
                    });
                }
                *unquote_expr = Some(expr);
                Ok(())
            }
            Some(StackFrame::UnquoteSplice {
                expr: ref mut unquote_splice_expr,
                ..
            }) => {
                if unquote_splice_expr.is_some() {
                    return Err(ParseError {
                        message: "unquote-splice form can only have one expression".to_string(),
                        span: Some(expr.span),
                    });
                }
                *unquote_splice_expr = Some(expr);
                Ok(())
            }
            Some(StackFrame::DefMacro {
                ref mut name,
                ref mut transformer,
                ..
            }) => {
                // DefMacro expects: [defmacro name-identifier transformer-expr]
                // First expression is the name (must be an identifier), second is the transformer
                if name.is_none() {
                    // This is the name — must be an identifier
                    match &expr.node {
                        Expr::VarRef { name: n, .. } => {
                            *name = Some(n.clone());
                            Ok(())
                        }
                        _ => Err(ParseError {
                            message: "defmacro name must be an identifier".to_string(),
                            span: Some(expr.span),
                        }),
                    }
                } else if transformer.is_none() {
                    // This is the transformer expression
                    *transformer = Some(expr);
                    Ok(())
                } else {
                    Err(ParseError {
                        message: "defmacro form takes exactly two arguments: name and transformer"
                            .to_string(),
                        span: Some(expr.span),
                    })
                }
            }
            Some(StackFrame::Match {
                ref mut scrutinee,
                ref mut arms,
                ref mut pending_pattern,
                ..
            }) => {
                // Match expects: [match scrutinee pat1 body1 pat2 body2 ...]
                // First expression is the scrutinee, then alternating pattern and body
                if scrutinee.is_none() {
                    // This is the scrutinee
                    *scrutinee = Some(expr);
                    Ok(())
                } else if pending_pattern.is_none() {
                    // This is a pattern — convert expression to pattern
                    match expr_to_pattern_with_guard(expr) {
                        Ok((pattern, guard)) => {
                            *pending_pattern = Some((pattern, guard));
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    // This is a body — complete the arm
                    let (pattern, guard) = pending_pattern.take().unwrap();
                    arms.push(MatchArm {
                        pattern,
                        guard,
                        body: Box::new(expr),
                    });
                    Ok(())
                }
            }
            Some(StackFrame::ClassDecl {
                ref mut name,
                ref mut params,
                superclasses: _,
                ..
            }) => {
                // ClassDecl expects: [class [Name params...] method: Type ...]
                // First expression is the class header: [Name params...]
                if name.is_none() {
                    // This is the class header — must be a dict or call with class name
                    // Parse: [Equatable a] or [Ord] or just Ord
                    match &expr.node {
                        Expr::VarRef {
                            name: class_name, ..
                        } => {
                            // Simple class: [class Equatable ...]
                            *name = Some(class_name.clone());
                            Ok(())
                        }
                        Expr::Dict(entries) if !entries.is_empty() => {
                            // Dict form: [class [Equatable a] ...]
                            // First entry should be the class name, rest are params
                            if let Expr::VarRef {
                                name: class_name, ..
                            } = &entries[0].node.value.node
                            {
                                *name = Some(class_name.clone());
                                for entry in entries.iter().skip(1) {
                                    if let Expr::VarRef {
                                        name: param_name, ..
                                    } = &entry.node.value.node
                                    {
                                        params.push(param_name.clone());
                                    } else {
                                        return Err(ParseError {
                                            message: "class parameters must be identifiers"
                                                .to_string(),
                                            span: Some(entry.span),
                                        });
                                    }
                                }
                                Ok(())
                            } else {
                                Err(ParseError {
                                    message: "class header must start with class name".to_string(),
                                    span: Some(expr.span),
                                })
                            }
                        }
                        Expr::Call {
                            func,
                            args,
                            implied: true,
                            ..
                        } => {
                            // Implied call form: [class [Equatable a b] ...]
                            if let Expr::VarRef {
                                name: class_name, ..
                            } = &func.node
                            {
                                *name = Some(class_name.clone());
                                for arg in args {
                                    if let Expr::VarRef {
                                        name: param_name, ..
                                    } = &arg.node
                                    {
                                        params.push(param_name.clone());
                                    } else {
                                        return Err(ParseError {
                                            message: "class parameters must be identifiers"
                                                .to_string(),
                                            span: Some(arg.span),
                                        });
                                    }
                                }
                                Ok(())
                            } else {
                                Err(ParseError {
                                    message: "class header must start with class name".to_string(),
                                    span: Some(expr.span),
                                })
                            }
                        }
                        _ => Err(ParseError {
                            message: "class header must be class name or [ClassName params...]"
                                .to_string(),
                            span: Some(expr.span),
                        }),
                    }
                } else {
                    // Already have name/params — subsequent expressions should be handled
                    // by push_value (which handles pending_key for method entries)
                    Err(ParseError {
                        message:
                            "unexpected expression in class form (expected method: Type entries)"
                                .to_string(),
                        span: Some(expr.span),
                    })
                }
            }
            Some(StackFrame::InstanceDecl {
                ref mut class_name,
                ref mut instance_type,
                ..
            }) => {
                // InstanceDecl expects: [instance [ClassName Type] method: impl ...]
                // First expression is the instance header: [ClassName Type]
                if class_name.is_none() {
                    // This is the instance header
                    match &expr.node {
                        Expr::Dict(entries) if entries.len() >= 2 => {
                            // Dict form: [instance [Equatable Int] ...]
                            if let Expr::VarRef { name: cls_name, .. } = &entries[0].node.value.node
                            {
                                *class_name = Some(cls_name.clone());
                                // Instance type is the second entry
                                *instance_type = Some((*entries[1].node.value).clone());
                                Ok(())
                            } else {
                                Err(ParseError {
                                    message: "instance header must start with class name"
                                        .to_string(),
                                    span: Some(expr.span),
                                })
                            }
                        }
                        Expr::Call {
                            func,
                            args,
                            implied: true,
                            ..
                        } if !args.is_empty() => {
                            // Implied call form: [instance [Equatable Int] ...]
                            if let Expr::VarRef { name: cls_name, .. } = &func.node {
                                *class_name = Some(cls_name.clone());
                                *instance_type = Some((*args[0]).clone());
                                Ok(())
                            } else {
                                Err(ParseError {
                                    message: "instance header must start with class name"
                                        .to_string(),
                                    span: Some(expr.span),
                                })
                            }
                        }
                        _ => Err(ParseError {
                            message: "instance header must be [ClassName Type]".to_string(),
                            span: Some(expr.span),
                        }),
                    }
                } else {
                    // Already have class_name/instance_type — subsequent expressions should be handled
                    // by push_value (which handles pending_key for method entries)
                    Err(ParseError {
                        message:
                            "unexpected expression in instance form (expected method: impl entries)"
                                .to_string(),
                        span: Some(expr.span),
                    })
                }
            }
            Some(StackFrame::Pipe { lhs, span_start }) => {
                // We have the RHS expression; pop the frame and create the Pipe node
                let lhs_expr = lhs.clone();
                let pipe_span = Span {
                    start: *span_start,
                    end: expr.span.end,
                };
                stack.pop(); // Remove the Pipe frame

                let pipe_expr = Expr::Pipe {
                    lhs: Box::new(lhs_expr),
                    rhs: Box::new(expr),
                };

                let spanned_pipe = Spanned::new(pipe_expr, pipe_span);

                // Push to parent context
                push_value(stack, current_document_expressions, spanned_pipe)
            }
            None => unreachable!("stack.is_empty() was false but last_mut returned None"),
        }
    }
}

/// Helper: push a value expression, handling keyed entries in dict/call contexts.
fn push_value(
    stack: &mut Vec<StackFrame>,
    current_document_expressions: &mut Vec<Rc<Spanned<Expr>>>,
    expr: Spanned<Expr>,
) -> Result<(), ParseError> {
    if stack.is_empty() {
        current_document_expressions.push(Rc::new(expr));
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
                    value: Rc::new(expr),
                });
            } else {
                // Auto-indexed entry
                entries.push(Entry {
                    key: None,
                    value: Rc::new(expr),
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
                args.push(CallArg::Named(name, Rc::new(expr)));
            } else {
                // Positional argument
                args.push(CallArg::Positional(Rc::new(expr)));
            }
            Ok(())
        }
        Some(StackFrame::ClassDecl {
            ref name,
            ref mut methods,
            ref mut pending_key,
            ..
        }) => {
            if name.is_none() {
                // Header not yet parsed — delegate to push_expr_to_parent
                push_expr_to_parent(stack, current_document_expressions, expr)
            } else if let Some(key) = pending_key.take() {
                // This value completes a method signature entry
                methods.push(Entry {
                    key: Some(key),
                    value: Rc::new(expr),
                });
                Ok(())
            } else {
                // ClassDecl expects keyed entries only (method names)
                Err(ParseError {
                    message: "class methods must have names (e.g., `eq: Type`)".to_string(),
                    span: Some(expr.span),
                })
            }
        }
        Some(StackFrame::InstanceDecl {
            ref class_name,
            ref instance_type,
            ref mut methods,
            ref mut pending_key,
            ..
        }) => {
            if class_name.is_none() || instance_type.is_none() {
                // Header not yet fully parsed — delegate to push_expr_to_parent
                push_expr_to_parent(stack, current_document_expressions, expr)
            } else if let Some(key) = pending_key.take() {
                // This value completes a method implementation entry
                methods.push(Entry {
                    key: Some(key),
                    value: Rc::new(expr),
                });
                Ok(())
            } else {
                // InstanceDecl expects keyed entries only (method names)
                Err(ParseError {
                    message: "instance methods must have names (e.g., `eq: [fn [x y] ...]`)"
                        .to_string(),
                    span: Some(expr.span),
                })
            }
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
/// This function returns `Err` if the input has any parse errors — both fatal errors
/// (lexer failure, unclosed brackets) and recoverable errors (errors inside bracket forms).
/// The first error encountered is returned. This maintains the pre-recovery behavior.
///
/// For multi-error reporting, use `parse2()` or `parse_with_recovery()` directly and
/// access `ParseOutput.errors`. The formatter uses `parse2()` for comment preservation.
pub fn parse(input: &str) -> Result<Spanned<File>, ParseError> {
    let output = parse2(input)?;
    // Surface any recovered errors as a failure: the `parse()` API promises
    // "no errors means valid input". Callers that want partial ASTs with error nodes
    // should use `parse2()` or `parse_with_recovery()` instead.
    if let Some(first_err) = output.errors.into_iter().next() {
        return Err(first_err);
    }
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

    Ok((*first_doc.node.expressions[0]).clone())
}

/// Parse tinct source text with error recovery.
///
/// This function ALWAYS succeeds (returns a `ParseOutput`), even when there are parse errors.
/// Unlike `parse()` and `parse2()` (which return `Err` on fatal errors like lexer failure
/// or unclosed brackets at top level), this function converts all fatal errors into
/// `ParseOutput.errors` and returns a synthetic AST.
///
/// Errors that occur inside bracket forms are recovered from: the parser substitutes
/// `Expr::Error` nodes and continues. Fatal errors (lexer failure, unclosed brackets at
/// top level) are also recovered: they are recorded in `ParseOutput.errors` and a minimal
/// empty `File` AST is returned.
///
/// Use this function when you want to report ALL parse errors at once (e.g. in an LSP
/// diagnostic pass or a batch linting tool) and always need an AST, even if it's empty.
pub fn parse_with_recovery(input: &str) -> ParseOutput {
    match parse2(input) {
        Ok(output) => output,
        Err(fatal_error) => {
            // Fatal error (lexer failure, unclosed brackets, etc.)
            // Construct a synthetic empty File with the error recorded.
            let empty_file = Spanned::new(File { documents: vec![] }, Span::origin());
            ParseOutput {
                file: empty_file,
                source: input.to_string(),
                leading_comments: BTreeMap::new(),
                trailing_comments: BTreeMap::new(),
                blank_before: BTreeMap::new(),
                errors: vec![fatal_error],
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: parse successfully and return the first expression from the first document.
    fn parse_expr(input: &str) -> Spanned<Expr> {
        let output = parse2(input).expect("parse failed");
        (*output.file.node.documents[0].node.expressions[0]).clone()
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
                ..
            } => {
                match &func.node {
                    Expr::VarRef { name, .. } => assert_eq!(name, "f"),
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
                ..
            } => {
                match &func.node {
                    Expr::VarRef { name, .. } => assert_eq!(name, "f"),
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
            Expr::TypeAlias { params, body } => {
                assert!(params.is_empty());
                assert!(matches!(&body.node, Expr::Int(42)));
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
        // MAX_PARSE_DEPTH + 1 opening brackets exceeds the limit.
        // parse() returns Err (depth-limit errors propagate as hard errors).
        let mut input = String::new();
        for _ in 0..MAX_PARSE_DEPTH {
            input.push('[');
        }
        input.push('['); // One more to exceed
        for _ in 0..=MAX_PARSE_DEPTH {
            input.push(']');
        }

        let err = parse(&input).unwrap_err();
        assert!(
            err.message.contains("maximum nesting depth exceeded"),
            "expected depth-limit error message, got: {}",
            err.message
        );
    }

    /// Verify that nesting up to MAX_PARSE_DEPTH - 1 (200 levels below limit) succeeds.
    ///
    /// This regression test guards the lower boundary of the depth limit: inputs
    /// well below 256 must parse successfully. The parser is iterative (Vec<StackFrame>),
    /// so 200 levels creates no Rust call-stack pressure.
    #[test]
    fn test_depth_limit_well_below_maximum_succeeds() {
        const DEPTH: usize = 200;
        assert!(
            DEPTH < MAX_PARSE_DEPTH,
            "test depth must be less than MAX_PARSE_DEPTH"
        );

        let mut input = String::new();
        for _ in 0..DEPTH {
            input.push('[');
        }
        input.push('1'); // innermost value
        for _ in 0..DEPTH {
            input.push(']');
        }

        let result = parse2(&input);
        assert!(
            result.is_ok(),
            "parsing {DEPTH} levels of nesting should succeed (limit is {MAX_PARSE_DEPTH}), got: {:?}",
            result.unwrap_err()
        );
    }

    /// Verify that exactly MAX_PARSE_DEPTH nesting levels succeeds.
    ///
    /// The depth check is `stack.len() >= MAX_PARSE_DEPTH` and fires BEFORE pushing,
    /// so the Nth bracket is processed when stack.len() = N-1. Therefore exactly
    /// MAX_PARSE_DEPTH brackets produces stack.len() = MAX_PARSE_DEPTH - 1 at the
    /// last push, which passes the check.
    #[test]
    fn test_depth_limit_at_exact_boundary_succeeds() {
        let mut input = String::new();
        for _ in 0..MAX_PARSE_DEPTH {
            input.push('[');
        }
        input.push('1');
        for _ in 0..MAX_PARSE_DEPTH {
            input.push(']');
        }

        let result = parse(&input);
        assert!(
            result.is_ok(),
            "exactly MAX_PARSE_DEPTH levels should succeed (check fires before push), got: {:?}",
            result.unwrap_err()
        );
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
    fn test_bracket_access_removed_parses_as_two_expressions() {
        // $a[0] — BracketAccess syntax removed. Now parses as two separate expressions:
        // VarRef("a") and Dict([Int(0)]). The `[` is always OpenBracket.
        let output = parse2("$a[0]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        assert_eq!(doc.expressions.len(), 2);
        match &doc.expressions[0].node {
            Expr::VarRef { name, .. } => assert_eq!(name, "a"),
            other => panic!("expected VarRef, got {other:?}"),
        }
        match &doc.expressions[1].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::Int(0)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // --- Error path tests ---
    //
    // Errors inside bracket forms are now recovered from: parse2() returns Ok with
    // ParseOutput.errors non-empty rather than returning Err. Tests use `output.errors`.
    // Only top-level / structural errors (unmatched ], unclosed [, DocSeparator inside
    // brackets) remain as parse2() returning Err.

    #[test]
    fn test_call_empty() {
        let output = parse2("[call]").expect("recovery should succeed");
        assert!(
            !output.errors.is_empty(),
            "expected recovered error for empty call form"
        );
        assert!(
            output.errors[0].message.contains("call form requires"),
            "expected error about call form requiring a function, got: {}",
            output.errors[0].message
        );
    }

    #[test]
    fn test_call_func_as_named_arg() {
        // [call f: $x] — first arg is Named("f", ...) which is forbidden as func
        let output = parse2("[call f: $x]").expect("recovery should succeed");
        assert!(
            !output.errors.is_empty(),
            "expected recovered error for named-arg func"
        );
        assert!(
            output.errors[0].message.contains("named argument"),
            "expected error about named argument, got: {}",
            output.errors[0].message
        );
    }

    #[test]
    fn test_dict_pending_key_no_value() {
        // [a:] — key with no value before closing bracket
        let output = parse2("[a:]").expect("recovery should succeed");
        assert!(
            !output.errors.is_empty(),
            "expected recovered error for key without value"
        );
        assert!(
            output.errors[0].message.contains("key without value"),
            "expected 'key without value' error, got: {}",
            output.errors[0].message
        );
    }

    #[test]
    fn test_call_pending_named_arg_no_value() {
        // [call $f x:] — named arg x with no value before closing bracket
        let output = parse2("[call $f x:]").expect("recovery should succeed");
        assert!(
            !output.errors.is_empty(),
            "expected recovered error for named arg without value"
        );
        assert!(
            output.errors[0].message.contains("without value"),
            "expected 'without value' error for named arg, got: {}",
            output.errors[0].message
        );
    }

    #[test]
    fn test_type_alias_empty() {
        let output = parse2("[type]").expect("recovery should succeed");
        assert!(
            !output.errors.is_empty(),
            "expected recovered error for empty type-alias"
        );
        assert!(
            output.errors[0]
                .message
                .contains("type-alias form requires"),
            "expected error about type-alias requiring a type expression, got: {}",
            output.errors[0].message
        );
    }

    #[test]
    fn test_type_assert_no_annotation() {
        // [@] — type-assert with @; parse_annotation sees CloseBracket after @ → error
        let output = parse_with_recovery("[@]");
        assert!(
            !output.errors.is_empty(),
            "expected recovered error for type-assert without annotation"
        );
        assert!(
            output.errors[0]
                .message
                .contains("expected annotation name or bracket dict after @"),
            "expected error about invalid annotation token, got: {}",
            output.errors[0].message
        );
    }

    #[test]
    fn test_type_assert_no_expr() {
        // [@Number] — annotation parsed, but no expression
        let output = parse2("[@Number]").expect("recovery should succeed");
        assert!(
            !output.errors.is_empty(),
            "expected recovered error for type-assert without expression"
        );
        assert!(
            output.errors[0]
                .message
                .contains("type-assert form requires an expression"),
            "expected error about missing expression, got: {}",
            output.errors[0].message
        );
    }

    #[test]
    fn test_colon_outside_dict_call() {
        // [fn :] — "fn" not followed by colon directly → Fn form.
        // Then ":" in Fn frame → "`:` can only appear in dict, call, class, or instance forms" (recovered).
        let output = parse2("[fn :]").expect("recovery should succeed");
        assert!(
            !output.errors.is_empty(),
            "expected recovered error for colon in fn form"
        );
        assert!(
            output.errors[0].message.contains("key without value")
                || output.errors[0].message.contains("`:` without a key")
                || output.errors[0]
                    .message
                    .contains("`:` can only appear in dict, call, class, or instance forms"),
            "expected key-related error for [fn :], got: {}",
            output.errors[0].message
        );
        // Also test the true "colon outside dict/call" case: colon in a TypeAlias frame
        let output2 = parse2("[type x :]").expect("recovery should succeed");
        assert!(
            !output2.errors.is_empty(),
            "expected recovered error for colon in type-alias form"
        );
        assert!(
            output2.errors[0]
                .message
                .contains("`:` can only appear in dict, call, class, or instance forms"),
            "expected error about colon in wrong context for [type x :], got: {}",
            output2.errors[0].message
        );
    }

    #[test]
    fn test_colon_without_key_in_dict() {
        // [:] — colon with no preceding key in a dict
        let output = parse2("[:]").expect("recovery should succeed");
        assert!(
            !output.errors.is_empty(),
            "expected recovered error for colon without key"
        );
        assert!(
            output.errors[0].message.contains("`:` without a key"),
            "expected error about colon without key, got: {}",
            output.errors[0].message
        );
    }

    #[test]
    fn test_fn_multiple_bodies() {
        // [fn 1 2] — two body expressions in an fn form (Sequential wrapping)
        let output = parse2("[fn 1 2]").expect("parse should succeed");
        assert!(
            output.errors.is_empty(),
            "multi-expression fn bodies should parse successfully via Sequential, got errors: {:?}",
            output.errors
        );
        // The fn body should be wrapped in Expr::Sequential
        let file = output.file;
        let doc = &file.node.documents[0].node;
        let expr = &doc.expressions[0].node;
        match expr {
            Expr::Fn { body, .. } => match &body.node {
                Expr::Sequential(exprs) => {
                    assert_eq!(exprs.len(), 2, "expected 2 expressions in Sequential body");
                }
                other => panic!("expected Sequential body, got: {other}"),
            },
            other => panic!("expected Fn expression, got: {other}"),
        }
    }

    #[test]
    fn test_type_alias_multiple_exprs() {
        // [type 1 2] — multi-entry type-alias form (union declaration)
        let output = parse2("[type 1 2]").expect("parse should succeed");
        assert!(
            output.errors.is_empty(),
            "multi-entry [type T1 T2 ...] should parse without errors, got: {:?}",
            output.errors
        );
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::TypeAlias { params, body } => {
                assert!(params.is_empty());
                // Multi-entry body is wrapped in a synthetic Dict with positional entries
                match &body.node {
                    Expr::Dict(entries) => {
                        assert_eq!(entries.len(), 2, "expected 2 positional entries");
                        assert!(
                            entries[0].node.key.is_none(),
                            "entries should be auto-indexed"
                        );
                        assert!(
                            entries[1].node.key.is_none(),
                            "entries should be auto-indexed"
                        );
                    }
                    other => panic!("expected Dict body for multi-entry type alias, got {other:?}"),
                }
            }
            other => panic!("expected TypeAlias, got {other:?}"),
        }
    }

    #[test]
    fn test_type_assert_multiple_exprs() {
        // [@Number 1 2] — two expressions in a type-assert form
        let output = parse2("[@Number 1 2]").expect("recovery should succeed");
        assert!(
            !output.errors.is_empty(),
            "expected recovered error for multiple type-assert expressions"
        );
        assert!(
            output.errors[0]
                .message
                .contains("type-assert form can only have one expression"),
            "expected error about multiple expressions, got: {}",
            output.errors[0].message
        );
    }

    #[test]
    fn test_fn_empty() {
        // [fn] — fn with no body
        let output = parse2("[fn]").expect("recovery should succeed");
        assert!(
            !output.errors.is_empty(),
            "expected recovered error for empty fn form"
        );
        assert!(
            output.errors[0]
                .message
                .contains("fn form requires a body expression"),
            "expected error about fn requiring a body, got: {}",
            output.errors[0].message
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
                ..
            } => {
                match &func.node {
                    Expr::VarRef { name, .. } => assert_eq!(name, "f"),
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
                ..
            } => {
                match &func.node {
                    Expr::VarRef { name, .. } => assert_eq!(name, "f"),
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
                ..
            } => {
                match &func.node {
                    Expr::VarRef { name, .. } => assert_eq!(name, "f"),
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
        // [call $f :] — colon inside Call frame with pending_key=None (no preceding identifier)
        let output = parse2("[call $f :]").expect("recovery should succeed");
        assert!(
            !output.errors.is_empty(),
            "expected recovered error for colon without name in call frame"
        );
        assert!(
            output.errors[0].message.contains("without a name"),
            "expected error about colon without a name in call frame, got: {}",
            output.errors[0].message
        );
    }

    #[test]
    fn test_annotation_invalid_token() {
        // [@123] — parse_annotation receives Int(123) after @, not Identifier or OpenBracket
        let output = parse2("[@123]").expect("recovery should succeed");
        assert!(
            !output.errors.is_empty(),
            "expected recovered error for invalid annotation token"
        );
        assert!(
            output.errors[0]
                .message
                .contains("expected annotation name or bracket dict after @"),
            "expected error about invalid annotation token, got: {}",
            output.errors[0].message
        );
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
                assert!(matches!(&body.node, Expr::VarRef { name, .. } if name == "x"));
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
                    Expr::VarRef { name, .. } => assert_eq!(name, "a"),
                    other => panic!("expected VarRef, got {other:?}"),
                }
                assert_eq!(*field, DotKey::Ident("b".to_string()));
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
                assert_eq!(*outer_field, DotKey::Ident("c".to_string()));
                match &outer_expr.node {
                    Expr::DotAccess {
                        expr: inner_expr,
                        field: inner_field,
                    } => {
                        assert_eq!(*inner_field, DotKey::Ident("b".to_string()));
                        match &inner_expr.node {
                            Expr::VarRef { name, .. } => assert_eq!(name, "a"),
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
                ..
            } => {
                match &func.node {
                    Expr::VarRef { name, .. } => assert_eq!(name, "fn"),
                    other => panic!("expected VarRef for func, got {other:?}"),
                }
                assert_eq!(args.len(), 1);
                match &args[0].node {
                    Expr::DotAccess { expr, field } => {
                        assert_eq!(*field, DotKey::Ident("b".to_string()));
                        match &expr.node {
                            Expr::VarRef { name, .. } => assert_eq!(name, "a"),
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
                        assert_eq!(*field, DotKey::Ident("z".to_string()));
                        match &expr.node {
                            Expr::VarRef { name, .. } => assert_eq!(name, "y"),
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
        // [...args x] — variadic param not last: parse returns Err.
        // The param-list error triggers recovery which consumes the fn form;
        // the surface error may be the param error or an unmatched-bracket cascade.
        assert!(
            parse("[fn [...args x] $x]").is_err(),
            "expected parse to fail for param after variadic"
        );
    }

    #[test]
    fn test_fn_multiple_variadic() {
        // [...args ...rest] — multiple variadic params: parse returns Err
        assert!(
            parse("[fn [...args ...rest] $x]").is_err(),
            "expected parse to fail for multiple variadic params"
        );
    }

    #[test]
    fn test_fn_variadic_with_annotation_errors() {
        // [...args@Int] — annotation on variadic param: parse returns Err
        assert!(
            parse("[fn [...args@Int] $args]").is_err(),
            "expected parse to fail for variadic annotation"
        );
    }

    #[test]
    fn test_range_outside_bracket_access() {
        // `..` always emits two consecutive Dot tokens — `Token::Range` has been removed.
        // `1..5` lexes as Int(1), Dot, Dot, Int(5). The first Dot triggers dot-access on Int(1)
        // but the next token is another Dot (not an identifier) → parse error at top level.
        // At top level (stack empty), the parser returns Err rather than recovering.
        let err = parse2("1..5").unwrap_err();
        assert!(
            err.message.contains("expected field name") || err.message.contains("found Dot"),
            "expected a dot-access parse error, got: {}",
            err.message
        );
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
    fn test_whitespace_allows_dot_access() {
        // "$a .b" has whitespace before dot; dot access is not whitespace-sensitive (unlike '['),
        // so this parses as a single DotAccess expression (same as "$a.b").
        let output = parse2("$a .b").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        assert_eq!(
            doc.expressions.len(),
            1,
            "expected 1 expression (DotAccess), got {}",
            doc.expressions.len()
        );
        match &doc.expressions[0].node {
            Expr::DotAccess { expr: inner, field } => {
                assert_eq!(*field, DotKey::Ident("b".to_string()));
                match &inner.node {
                    Expr::VarRef { name, .. } => assert_eq!(name, "a"),
                    other => panic!("expected VarRef('a') inside DotAccess, got {other:?}"),
                }
            }
            other => panic!("expected DotAccess, got {other:?}"),
        }
    }

    #[test]
    fn test_bracket_parses_as_separate_dict() {
        // "$a [0]" parses as two separate expressions: VarRef and Dict([Int(0)])
        let output = parse2("$a [0]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        assert_eq!(
            doc.expressions.len(),
            2,
            "expected 2 expressions (VarRef 'a' + Dict containing Int(0)), got {}",
            doc.expressions.len()
        );
        match &doc.expressions[0].node {
            Expr::VarRef { name, .. } => assert_eq!(name, "a"),
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
                assert!(matches!(&body.node, Expr::VarRef { name, .. } if name == "x"));
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
                assert!(matches!(&body.node, Expr::VarRef { name, .. } if name == "x"));
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn test_dot_access_on_dict_literal() {
        // "[x: 1].x" — dot access immediately after closing bracket (no whitespace)
        // The lexer emits Dot (access operator) after ']' since CloseBracket is in access context.
        let output = parse2("[x: 1].x").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        assert_eq!(
            doc.expressions.len(),
            1,
            "expected 1 expression (DotAccess)"
        );
        match &doc.expressions[0].node {
            Expr::DotAccess { expr, field } => {
                assert_eq!(*field, DotKey::Ident("x".to_string()));
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
        let output = parse2("[a: 1  a: 2]").expect("recovery should succeed");
        assert!(
            !output.errors.is_empty(),
            "expected recovered error for duplicate key"
        );
        assert!(
            output.errors[0].message.contains("duplicate key"),
            "expected 'duplicate key' in error, got: {}",
            output.errors[0].message
        );
        assert!(
            output.errors[0].message.contains("\"a\""),
            "expected key name in error, got: {}",
            output.errors[0].message
        );
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

    /// Regression test: bracket forms should have correct line and column numbers in their spans.
    /// Previously, all bracket forms had line:1 col:1 regardless of actual position.
    #[test]
    fn test_bracket_form_span_line_column() {
        // Dict on line 4 (after 3 comment lines)
        let input = "# Line 1\n# Line 2\n# Line 3\n[x: 10\n y: 20]";
        let output = parse2(input).expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        let dict_expr = &doc.expressions[0];

        // The opening bracket '[' is at line 4, column 1
        assert_eq!(dict_expr.span.start.line, 4, "Dict should start on line 4");
        assert_eq!(
            dict_expr.span.start.column, 1,
            "Dict should start at column 1"
        );

        // Also test a nested bracket form
        let input2 = "# Line 1\n[outer: [inner: 1]]";
        let output2 = parse2(input2).expect("parse failed");
        let doc2 = &output2.file.node.documents[0].node;
        match &doc2.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                // Outer dict starts on line 2
                assert_eq!(
                    doc2.expressions[0].span.start.line, 2,
                    "Outer dict should start on line 2"
                );
                // Inner dict should also have correct line/column (line 2, after "outer: ")
                match &entries[0].node.value.node {
                    Expr::Dict(_) => {
                        let inner_span = entries[0].node.value.span;
                        assert_eq!(
                            inner_span.start.line, 2,
                            "Inner dict should start on line 2"
                        );
                        assert_eq!(
                            inner_span.start.column, 9,
                            "Inner dict should start at column 9 (after 'outer: ')"
                        );
                    }
                    other => panic!("expected inner Dict, got {other:?}"),
                }
            }
            other => panic!("expected outer Dict, got {other:?}"),
        }
    }

    /// Regression test: Call, Fn, TypeAlias, and TypeAssert bracket forms
    /// should have correct line and column numbers in their spans.
    #[test]
    fn test_bracket_form_span_variants() {
        // Call form on line 3
        let input_call = "# Line 1\n# Line 2\n[call $f 1]";
        let output = parse2(input_call).expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        let call_expr = &doc.expressions[0];
        match &call_expr.node {
            Expr::Call { .. } => {
                assert_eq!(call_expr.span.start.line, 3, "Call should start on line 3");
            }
            other => panic!("expected Call, got {other:?}"),
        }

        // Fn form on line 3
        let input_fn = "# Line 1\n# Line 2\n[fn [x] $x]";
        let output = parse2(input_fn).expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        let fn_expr = &doc.expressions[0];
        match &fn_expr.node {
            Expr::Fn { .. } => {
                assert_eq!(fn_expr.span.start.line, 3, "Fn should start on line 3");
            }
            other => panic!("expected Fn, got {other:?}"),
        }

        // TypeAlias form on line 3
        let input_type = "# Line 1\n# Line 2\n[type Int]";
        let output = parse2(input_type).expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        let type_expr = &doc.expressions[0];
        match &type_expr.node {
            Expr::TypeAlias { .. } => {
                assert_eq!(
                    type_expr.span.start.line, 3,
                    "TypeAlias should start on line 3"
                );
            }
            other => panic!("expected TypeAlias, got {other:?}"),
        }

        // TypeAssert form on line 3
        let input_assert = "# Line 1\n# Line 2\n[@Int 42]";
        let output = parse2(input_assert).expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        let assert_expr = &doc.expressions[0];
        match &assert_expr.node {
            Expr::TypeAssert { .. } => {
                assert_eq!(
                    assert_expr.span.start.line, 3,
                    "TypeAssert should start on line 3"
                );
            }
            other => panic!("expected TypeAssert, got {other:?}"),
        }
    }

    #[test]
    fn test_call_newline_colon_not_dict() {
        // [call\n: x] — newline before colon should not create dict with "call" key
        // Instead, it's a call form with zero args followed by unexpected colon (recovered)
        let output = parse2("[call\n: x]").expect("recovery should succeed");
        assert!(
            !output.errors.is_empty(),
            "expected recovered error for colon without name in call form"
        );
        assert!(
            output.errors[0].message.contains("`:` without a name"),
            "expected error about colon without name, got: {}",
            output.errors[0].message
        );
    }

    // --- Error recovery tests (Items 2-5 of parser-error-recovery sprint) ---

    /// A single error inside brackets is recovered from: parse2() returns Ok, the
    /// document contains an Expr::Error node, and ParseOutput.errors has one entry.
    #[test]
    fn test_recovery_single_error_inside_brackets() {
        // [a:] — key without value; recovered with Expr::Error node
        let output = parse2("[a:]").expect("recovery should succeed");
        assert_eq!(output.errors.len(), 1, "expected exactly 1 recovered error");
        assert!(
            output.errors[0].message.contains("key without value"),
            "expected 'key without value' error, got: {}",
            output.errors[0].message
        );
        // The document should contain one expression (the Expr::Error node)
        let doc = &output.file.node.documents[0].node;
        assert_eq!(
            doc.expressions.len(),
            1,
            "expected 1 expression (Error node)"
        );
        assert!(
            matches!(doc.expressions[0].node, Expr::Error(_)),
            "expected Expr::Error node after recovery, got: {:?}",
            doc.expressions[0].node
        );
    }

    /// Multiple errors are all collected: parse2() returns Ok with multiple entries in
    /// ParseOutput.errors, and the document contains multiple Expr::Error nodes.
    #[test]
    fn test_recovery_multiple_errors() {
        // Two consecutive broken bracket forms at document level
        let output = parse2("[a:] [b:]").expect("recovery should succeed");
        assert_eq!(
            output.errors.len(),
            2,
            "expected 2 recovered errors, got {:?}",
            output.errors.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
        let doc = &output.file.node.documents[0].node;
        assert_eq!(
            doc.expressions.len(),
            2,
            "expected 2 expressions (2 Error nodes), got {}",
            doc.expressions.len()
        );
        assert!(
            matches!(doc.expressions[0].node, Expr::Error(_)),
            "expected first expression to be Expr::Error"
        );
        assert!(
            matches!(doc.expressions[1].node, Expr::Error(_)),
            "expected second expression to be Expr::Error"
        );
    }

    /// An error in a nested bracket is recovered from, and the outer bracket continues
    /// to parse normally. The outer dict should contain an Error node as its value.
    #[test]
    fn test_recovery_error_in_nested_brackets() {
        // [outer: [inner:]] — inner bracket has key without value; outer should still parse
        let output = parse2("[outer: [inner:]]").expect("recovery should succeed");
        assert_eq!(output.errors.len(), 1, "expected 1 recovered error");
        assert!(
            output.errors[0].message.contains("key without value"),
            "expected 'key without value' error, got: {}",
            output.errors[0].message
        );
        // The outer dict should have one entry
        let doc = &output.file.node.documents[0].node;
        assert_eq!(doc.expressions.len(), 1, "expected 1 top-level expression");
        match &doc.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1, "expected 1 outer entry");
                match &entries[0].node.key.as_ref().unwrap().node {
                    Expr::Str(s) => assert_eq!(s, "outer"),
                    other => panic!("expected key 'outer', got {other:?}"),
                }
                // The value should be the Expr::Error from the inner bracket
                assert!(
                    matches!(entries[0].node.value.node, Expr::Error(_)),
                    "expected Expr::Error as outer value, got: {:?}",
                    entries[0].node.value.node
                );
            }
            other => panic!("expected outer Dict, got {other:?}"),
        }
    }

    /// parse_with_recovery is an infallible wrapper that always returns ParseOutput.
    /// Valid input produces ParseOutput.errors=[] and a well-formed AST.
    #[test]
    fn test_parse_with_recovery_valid_input() {
        let output = parse_with_recovery("[a: 1 b: 2]");
        assert!(
            output.errors.is_empty(),
            "expected no errors for valid input, got: {:?}",
            output.errors
        );
        let doc = &output.file.node.documents[0].node;
        assert_eq!(doc.expressions.len(), 1);
        assert!(matches!(doc.expressions[0].node, Expr::Dict(_)));
    }

    /// parse_with_recovery on errored input returns ParseOutput with errors collected.
    #[test]
    fn test_parse_with_recovery_error_input() {
        let output = parse_with_recovery("[fn]");
        assert_eq!(output.errors.len(), 1, "expected 1 recovered error");
        assert!(
            output.errors[0].message.contains("fn form requires a body"),
            "expected fn-body error, got: {}",
            output.errors[0].message
        );
    }

    /// parse_with_recovery on fatal errors (unclosed brackets) returns synthetic empty File.
    #[test]
    fn test_parse_with_recovery_fatal_error() {
        let output = parse_with_recovery("[");
        assert_eq!(output.errors.len(), 1, "expected 1 fatal error");
        assert!(
            output.errors[0].message.contains("unclosed bracket"),
            "expected unclosed-bracket error, got: {}",
            output.errors[0].message
        );
        assert_eq!(
            output.file.node.documents.len(),
            0,
            "expected empty documents for fatal error"
        );
    }

    /// Task 1: Partial dict preservation - valid entries before error are kept.
    #[test]
    fn test_recovery_partial_dict_preservation() {
        // [a: 1  a: 2] — has one valid entry (a: 1) before the duplicate key error
        // Should recover with a partial dict containing the valid entry plus an error entry
        let output = parse2("[a: 1  a: 2]").expect("recovery should succeed");
        assert_eq!(output.errors.len(), 1, "expected exactly 1 recovered error");
        assert!(
            output.errors[0].message.contains("duplicate key"),
            "expected 'duplicate key' error, got: {}",
            output.errors[0].message
        );

        let doc = &output.file.node.documents[0].node;
        assert_eq!(doc.expressions.len(), 1, "expected 1 expression");

        // The recover_from_bracket_error() function builds a partial dict when there are
        // valid entries, adding an error entry for the failed part
        match &doc.expressions[0].node {
            Expr::Dict(entries) => {
                // Should have 2 entries: the valid "a: 1" plus the error entry
                assert_eq!(entries.len(), 2, "expected 2 entries (1 valid + 1 error)");

                // First entry should be a: 1
                match &entries[0].node.key.as_ref().unwrap().node {
                    Expr::Str(s) => assert_eq!(s, "a", "expected key 'a'"),
                    other => panic!("expected key 'a', got {other:?}"),
                }
                match &entries[0].node.value.node {
                    Expr::Int(n) => assert_eq!(*n, 1, "expected value 1"),
                    other => panic!("expected value 1, got {other:?}"),
                }

                // Second entry should be an error (auto-indexed, no key)
                assert!(
                    entries[1].node.key.is_none(),
                    "expected error entry to have no key"
                );
                assert!(
                    matches!(entries[1].node.value.node, Expr::Error(_)),
                    "expected Expr::Error as second entry value"
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    /// Task 2: Annotation recovery - invalid token after @ should recover without infinite loop.
    #[test]
    fn test_recovery_annotation_invalid_token() {
        // [@123 x: 1] — invalid token (Int) after @, should recover without infinite loop
        let output = parse_with_recovery("[@123 x: 1]");
        assert!(
            !output.errors.is_empty(),
            "expected at least 1 recovered error"
        );
        assert!(
            output.errors[0]
                .message
                .contains("expected annotation name or bracket dict after @"),
            "expected annotation error, got: {}",
            output.errors[0].message
        );
        // The annotation error cascades: invalid token after @ is recovered, but the
        // subsequent `:` in TypeAssert context triggers a second recovery that produces
        // Expr::Error for the whole form. This is correct — the form is malformed.
        assert_eq!(output.file.node.documents.len(), 1, "expected 1 document");
        let doc = &output.file.node.documents[0].node;
        assert!(
            !doc.expressions.is_empty(),
            "expected at least 1 expression"
        );
        match &doc.expressions[0].node {
            Expr::Error(_) | Expr::Dict(_) => {
                // Either Expr::Error (cascading recovery) or Dict (partial preservation)
            }
            other => panic!("expected Error or Dict, got {other:?}"),
        }
    }

    /// skip_to_closing_bracket correctly finds the matching ] accounting for nesting.
    #[test]
    fn test_skip_to_closing_bracket() {
        // Tokenize "[a [b c] d]" and verify skip_to_closing_bracket from index 1
        // (just past the opening '[') finds the matching ']' at the end.
        let tokens = crate::lexer::tokenize("[a [b c] d]").expect("tokenize failed");
        // tokens: [ Identifier("a") OpenBracket Identifier("b") Identifier("c") CloseBracket Identifier("d") CloseBracket
        // from_idx=1 (Identifier("a")), depth starts at 1
        let close = skip_to_closing_bracket(&tokens, 1);
        assert!(
            close < tokens.len(),
            "expected to find closing bracket, got tokens.len()"
        );
        assert!(
            matches!(tokens[close].node, crate::lexer::Token::CloseBracket),
            "expected CloseBracket at close index, got {:?}",
            tokens[close].node
        );
        // The outer ']' should be the last token
        assert_eq!(
            close,
            tokens.len() - 1,
            "expected close to be the last token"
        );
    }

    #[test]
    fn test_underscore_exclusion_positions() {
        // Verify that $_ in exclusion positions (dict key) is parsed as VarRef.
        // Bracket access and range access syntax have been removed.

        // $_ in dict key position: [$_: 42]
        let expr = parse_expr("[$_: 42]");
        match &expr.node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                let key_expr = entries[0].node.key.as_ref().expect("expected key");
                match &key_expr.node {
                    Expr::VarRef { name, .. } => {
                        assert_eq!(name, "_", "$_ as dict key should be VarRef")
                    }
                    other => panic!("expected VarRef(_) for dict key, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // --- Interpolated string desugaring tests ---

    /// i"Hello $name" desugars to [str "Hello " name].
    #[test]
    fn test_desugar_interpolated_string_varref() {
        let expr = parse_expr(r#"i"Hello $name""#);
        match &expr.node {
            Expr::Call {
                func,
                args,
                named_args,
                implied,
            } => {
                assert!(!implied, "expected non-implied call");
                assert!(named_args.is_empty());
                match &func.node {
                    Expr::VarRef { name, .. } => assert_eq!(name, "str"),
                    other => panic!("expected func=VarRef(str), got {other:?}"),
                }
                assert_eq!(args.len(), 2);
                match &args[0].node {
                    Expr::Str(s) => assert_eq!(s, "Hello "),
                    other => panic!("expected args[0]=Str(\"Hello \"), got {other:?}"),
                }
                match &args[1].node {
                    Expr::VarRef { name, .. } => assert_eq!(name, "name"),
                    other => panic!("expected args[1]=VarRef(name), got {other:?}"),
                }
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    /// i"${[+ $x 1]}" desugars to [str [+ $x 1]] — the inner expression is re-parsed.
    #[test]
    fn test_desugar_interpolated_string_expr() {
        let expr = parse_expr(r#"i"${[+ $x 1]}""#);
        match &expr.node {
            Expr::Call {
                func,
                args,
                implied,
                ..
            } => {
                assert!(!implied);
                match &func.node {
                    Expr::VarRef { name, .. } => assert_eq!(name, "str"),
                    other => panic!("expected func=VarRef(str), got {other:?}"),
                }
                assert_eq!(args.len(), 1, "expected 1 arg for lone interpolation expr");
                // The arg should be the re-parsed [+ $x 1] — an implied Call
                match &args[0].node {
                    Expr::Call {
                        func: inner_func,
                        args: inner_args,
                        implied: inner_implied,
                        ..
                    } => {
                        assert!(*inner_implied, "inner call [+ $x 1] should be implied");
                        match &inner_func.node {
                            Expr::VarRef { name, .. } => assert_eq!(name, "+"),
                            other => panic!("expected inner func VarRef(+), got {other:?}"),
                        }
                        assert_eq!(inner_args.len(), 2);
                        match &inner_args[0].node {
                            Expr::VarRef { name, .. } => assert_eq!(name, "x"),
                            other => panic!("expected VarRef(x) as arg 0, got {other:?}"),
                        }
                        match &inner_args[1].node {
                            Expr::Int(1) => {}
                            other => panic!("expected Int(1) as arg 1, got {other:?}"),
                        }
                    }
                    other => panic!("expected inner Call, got {other:?}"),
                }
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    /// i"prefix $name suffix ${[+ $x 1]} end" — mixed literal, varref, and expr parts.
    #[test]
    fn test_desugar_interpolated_string_mixed() {
        let expr = parse_expr(r#"i"prefix $name suffix ${[+ $x 1]} end""#);
        match &expr.node {
            Expr::Call { func, args, .. } => {
                match &func.node {
                    Expr::VarRef { name, .. } => assert_eq!(name, "str"),
                    other => panic!("expected func=VarRef(str), got {other:?}"),
                }
                assert_eq!(
                    args.len(),
                    5,
                    "expected 5 parts: literal, varref, literal, expr, literal"
                );
                // args[0]: "prefix "
                assert!(matches!(&args[0].node, Expr::Str(s) if s == "prefix "));
                // args[1]: VarRef(name)
                assert!(matches!(&args[1].node, Expr::VarRef { name, .. } if name == "name"));
                // args[2]: " suffix "
                assert!(matches!(&args[2].node, Expr::Str(s) if s == " suffix "));
                // args[3]: re-parsed expression [+ $x 1]
                assert!(matches!(&args[3].node, Expr::Call { .. }));
                // args[4]: " end"
                assert!(matches!(&args[4].node, Expr::Str(s) if s == " end"));
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    /// Unterminated ${...} produces a lex error (propagated as ParseError).
    #[test]
    fn test_desugar_interpolated_string_expr_unclosed() {
        let result = parse2(r#"i"foo ${bar""#);
        assert!(result.is_err(), "expected parse error for unclosed ${{}}");
        let err = result.unwrap_err();
        assert!(
            err.message.contains("unterminated ${...}"),
            "expected unterminated error, got: {}",
            err.message
        );
    }

    // --- Pipe (|) operator parsing tests ---

    /// `a | b` parses as Pipe { lhs: VarRef("a"), rhs: VarRef("b") }.
    #[test]
    fn test_pipe_basic() {
        let expr = parse_expr("a | b");
        match &expr.node {
            Expr::Pipe { lhs, rhs } => {
                assert!(
                    matches!(&lhs.node, Expr::VarRef { name, .. } if name == "a"),
                    "expected lhs = VarRef(a), got {:?}",
                    lhs.node
                );
                assert!(
                    matches!(&rhs.node, Expr::VarRef { name, .. } if name == "b"),
                    "expected rhs = VarRef(b), got {:?}",
                    rhs.node
                );
            }
            other => panic!("expected Pipe, got {other:?}"),
        }
    }

    /// `a | b | c` is left-associative: parsed as `(a | b) | c`.
    #[test]
    fn test_pipe_left_assoc() {
        let expr = parse_expr("a | b | c");
        match &expr.node {
            Expr::Pipe { lhs, rhs } => {
                // rhs must be VarRef("c")
                assert!(
                    matches!(&rhs.node, Expr::VarRef { name, .. } if name == "c"),
                    "expected rhs = VarRef(c), got {:?}",
                    rhs.node
                );
                // lhs must be Pipe { a | b }
                match &lhs.node {
                    Expr::Pipe {
                        lhs: inner_lhs,
                        rhs: inner_rhs,
                    } => {
                        assert!(
                            matches!(&inner_lhs.node, Expr::VarRef { name, .. } if name == "a"),
                            "expected inner_lhs = VarRef(a), got {:?}",
                            inner_lhs.node
                        );
                        assert!(
                            matches!(&inner_rhs.node, Expr::VarRef { name, .. } if name == "b"),
                            "expected inner_rhs = VarRef(b), got {:?}",
                            inner_rhs.node
                        );
                    }
                    other => panic!("expected nested Pipe for lhs, got {other:?}"),
                }
            }
            other => panic!("expected Pipe, got {other:?}"),
        }
    }

    /// `$x | [f $y]` — pipe where RHS is an explicit Call bracket expression.
    /// Note: dot access (`.`) has higher precedence than pipe (`|`). So `a.b | c.d`
    /// parses as `DotAccess(Pipe(DotAccess(a,b), c), d)` — the trailing `.d` extends
    /// beyond the pipe. To test pipe with a bracket RHS, use an explicit `[...]` form.
    #[test]
    fn test_pipe_inside_brackets() {
        // $x | [f $y] — top-level pipe, RHS is an explicit Call
        let expr = parse_expr("$x | [f $y]");
        match &expr.node {
            Expr::Pipe { lhs, rhs } => {
                assert!(
                    matches!(&lhs.node, Expr::VarRef { name, .. } if name == "x"),
                    "expected lhs = VarRef(x), got {:?}",
                    lhs.node
                );
                assert!(
                    matches!(&rhs.node, Expr::Call { .. }),
                    "expected rhs = Call, got {:?}",
                    rhs.node
                );
            }
            other => panic!("expected Pipe, got {other:?}"),
        }
    }

    /// `a.b | c.d` — dot access on both sides of pipe.
    #[test]
    fn test_pipe_dot_then_pipe() {
        let expr = parse_expr("$data.name | upper");
        match &expr.node {
            Expr::Pipe { lhs, rhs } => {
                assert!(
                    matches!(&lhs.node, Expr::DotAccess { .. }),
                    "expected lhs = DotAccess, got {:?}",
                    lhs.node
                );
                assert!(
                    matches!(&rhs.node, Expr::VarRef { name, .. } if name == "upper"),
                    "expected rhs = VarRef(upper), got {:?}",
                    rhs.node
                );
            }
            other => panic!("expected Pipe, got {other:?}"),
        }
    }

    // --- DotKey::Int parsing tests ---

    /// `$a.0` parses as DotAccess with DotKey::Int(0).
    #[test]
    fn test_dot_access_int_key() {
        let expr = parse_expr("$a.0");
        match &expr.node {
            Expr::DotAccess { field, .. } => {
                assert!(
                    matches!(field, DotKey::Int(0)),
                    "expected DotKey::Int(0), got {:?}",
                    field
                );
            }
            other => panic!("expected DotAccess, got {other:?}"),
        }
    }

    /// `$a.0.name` parses as chained DotAccess: outer is Ident("name"), inner is Int(0).
    #[test]
    fn test_dot_access_int_then_ident() {
        let expr = parse_expr("$a.0.name");
        match &expr.node {
            Expr::DotAccess {
                expr: target,
                field,
                ..
            } => {
                // outer field: "name"
                assert!(
                    matches!(field, DotKey::Ident(s) if s == "name"),
                    "expected outer DotKey::Ident(name), got {:?}",
                    field
                );
                // inner: DotAccess on $a with Int(0)
                match &target.node {
                    Expr::DotAccess {
                        field: inner_field, ..
                    } => {
                        assert!(
                            matches!(inner_field, DotKey::Int(0)),
                            "expected inner DotKey::Int(0), got {:?}",
                            inner_field
                        );
                    }
                    other => panic!("expected inner DotAccess, got {other:?}"),
                }
            }
            other => panic!("expected DotAccess, got {other:?}"),
        }
    }

    /// `$a.0.1` parses as chained DotAccess with two Int keys (not Float 0.1).
    /// Regression test: the lexer must suppress float detection after access-dot,
    /// otherwise `0.1` would be lexed as a single Float token.
    #[test]
    fn test_dot_access_int_chain() {
        let expr = parse_expr("$a.0.1");
        match &expr.node {
            Expr::DotAccess {
                expr: target,
                field,
                ..
            } => {
                // outer field: Int(1)
                assert!(
                    matches!(field, DotKey::Int(1)),
                    "expected outer DotKey::Int(1), got {:?}",
                    field
                );
                // inner: DotAccess on $a with Int(0)
                match &target.node {
                    Expr::DotAccess {
                        field: inner_field, ..
                    } => {
                        assert!(
                            matches!(inner_field, DotKey::Int(0)),
                            "expected inner DotKey::Int(0), got {:?}",
                            inner_field
                        );
                    }
                    other => panic!("expected inner DotAccess, got {other:?}"),
                }
            }
            other => panic!("expected DotAccess, got {other:?}"),
        }
    }
}
