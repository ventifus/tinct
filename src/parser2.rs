//! Iterative parser for tinct — will replace the pest-based parser.
//!
//! This is Phase 2a of the parser rewrite: core data structures and skeleton.
//! Subsequent sprints (parser-core-b, parser-core-c) will fill in the full parsing logic.
//!
//! See `doc/whatif/parser-rewrite.md` for the complete design.

use std::collections::BTreeMap;

use crate::ast::*;
use crate::lexer::{self, Token};
use crate::parser::ParseError;

/// Maximum nesting depth for bracket expressions (enforced before allocation).
const MAX_PARSE_DEPTH: usize = 256;

/// Helper: push a literal expression to either the current document or parent dict frame.
fn push_literal(
    stack: &mut Vec<StackFrame>,
    current_document_expressions: &mut Vec<Spanned<Expr>>,
    expr: Spanned<Expr>,
) {
    if stack.is_empty() {
        // Top-level literal
        current_document_expressions.push(expr);
    } else {
        // Inside a dict — add to current frame's entries
        if let Some(StackFrame::Dict {
            ref mut entries, ..
        }) = stack.last_mut()
        {
            entries.push(Entry {
                key: None,
                value: expr,
            });
        }
    }
}

/// Stack frame types for the iterative parser.
///
/// Each variant corresponds to a bracket-form being parsed. The parser pushes
/// a frame on `Token::OpenBracket` or `Token::BracketAccess`, collects entries/args/params
/// during iteration, then pops and constructs the AST node on `Token::CloseBracket`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
enum StackFrame {
    /// Dictionary literal: `[key: value ...]`
    Dict {
        entries: Vec<Entry>,
        span_start: usize,
    },
    /// Function call: `[call $func arg1 arg2 name: val]`
    Call {
        args: Vec<CallArg>,
        span_start: usize,
    },
    /// Function definition: `[fn [params] body]` or `[fn@Type [params] body]`
    Fn {
        params: Vec<Param>,
        body_start: Option<usize>,
        span_start: usize,
    },
    /// Type alias: `[type expr]`
    TypeAlias {
        name: String,
        params: Vec<String>,
        span_start: usize,
    },
    /// Type assertion: `[@Annotation expr]`
    TypeAssert {
        annotation: Spanned<Annotation>,
        span_start: usize,
    },
    /// Bracket access key: `$a[key_expr]` where `key_expr` may contain nested brackets
    BracketAccessKey { span_start: usize },
}

/// Intermediate representation for call arguments (positional or named).
///
/// During call parsing, arguments are collected in order. The evaluator later
/// enforces the C-PRIORITY binding order (see `doc/04-functions.md §Call Convention`).
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
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
/// This is the main entry point for Phase 2a. The skeleton handles:
/// - Basic literals: `Int`, `Float`, `BoolLit`, `QuotedString`, `BareWord`, `VarRef`
/// - Empty dicts: `[]`
/// - Simple dicts: `[42]`, `[a: 1]`, `[a: 1 b: 2]`
///
/// Not yet implemented (deferred to parser-core-b and later sprints):
/// - Call forms, fn forms, type-alias, type-assert
/// - Access chains (dot and bracket)
/// - Annotations (simple and property dict)
/// - Document separators (`---`)
/// - Nested structures beyond basic bracket nesting
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

    // Iterate over tokens
    for spanned_token in tokens {
        let token = &spanned_token.node;
        let span = spanned_token.span;

        match token {
            Token::OpenBracket | Token::BracketAccess => {
                // Check depth before pushing
                if stack.len() >= MAX_PARSE_DEPTH {
                    return Err(ParseError {
                        message: format!(
                            "maximum nesting depth exceeded (limit: {MAX_PARSE_DEPTH})"
                        ),
                        span: Some(span),
                    });
                }

                // For now, push a placeholder Dict frame
                // Full form dispatch (call/fn/type-alias/type-assert) comes in parser-core-b
                stack.push(StackFrame::Dict {
                    entries: Vec::new(),
                    span_start: span.start.offset,
                });
            }

            Token::CloseBracket => {
                // Pop the frame and construct the AST node
                let frame = stack.pop().ok_or_else(|| ParseError {
                    message: "unmatched closing bracket".to_string(),
                    span: Some(span),
                })?;

                match frame {
                    StackFrame::Dict {
                        entries,
                        span_start,
                    } => {
                        // Construct the dict expression
                        let dict_expr = Expr::Dict(
                            entries
                                .into_iter()
                                .map(|e| {
                                    // TODO(parser-core-b): for keyed entries, span should include key span
                                    let entry_span = Span {
                                        start: e.value.span.start,
                                        end: e.value.span.end,
                                    };
                                    Spanned::new(e, entry_span)
                                })
                                .collect(),
                        );

                        let dict_span = Span {
                            start: Position {
                                offset: span_start,
                                line: 1, // TODO(parser-core-b): proper line tracking from lexer tokens
                                column: 1,
                            },
                            end: span.end,
                        };

                        let spanned_dict = Spanned::new(dict_expr, dict_span);

                        // If stack is empty, add to current document
                        if stack.is_empty() {
                            current_document_expressions.push(spanned_dict);
                        } else {
                            // Otherwise, add to parent frame's entries (nested structure)
                            if let Some(StackFrame::Dict {
                                ref mut entries, ..
                            }) = stack.last_mut()
                            {
                                entries.push(Entry {
                                    key: None,
                                    value: spanned_dict,
                                });
                            }
                        }
                    }

                    // Other frame types will be handled in parser-core-b
                    StackFrame::Call { .. } => {
                        return Err(ParseError {
                            message: "call forms not yet implemented (parser-core-b)".to_string(),
                            span: Some(span),
                        });
                    }
                    StackFrame::Fn { .. } => {
                        return Err(ParseError {
                            message: "fn forms not yet implemented (parser-core-b)".to_string(),
                            span: Some(span),
                        });
                    }
                    StackFrame::TypeAlias { .. } => {
                        return Err(ParseError {
                            message: "type-alias forms not yet implemented (parser-core-b)"
                                .to_string(),
                            span: Some(span),
                        });
                    }
                    StackFrame::TypeAssert { .. } => {
                        return Err(ParseError {
                            message: "type-assert forms not yet implemented (parser-core-b)"
                                .to_string(),
                            span: Some(span),
                        });
                    }
                    StackFrame::BracketAccessKey { .. } => {
                        return Err(ParseError {
                            message: "bracket access not yet implemented (parser-core-b)"
                                .to_string(),
                            span: Some(span),
                        });
                    }
                }
            }

            Token::Colon => {
                // Key-value separator — handled during entry collection in parser-core-b
                return Err(ParseError {
                    message: "keyed entries not yet supported (parser-core-b)".to_string(),
                    span: Some(span),
                });
            }

            // Literals: collect as values
            Token::Int(n) => {
                let expr = Spanned::new(Expr::Int(*n), span);
                push_literal(&mut stack, &mut current_document_expressions, expr);
            }

            Token::Float(f) => {
                let expr = Spanned::new(Expr::Float(*f), span);
                push_literal(&mut stack, &mut current_document_expressions, expr);
            }

            Token::BoolLit(b) => {
                let expr = Spanned::new(Expr::Bool(*b), span);
                push_literal(&mut stack, &mut current_document_expressions, expr);
            }

            Token::QuotedString(s) => {
                let expr = Spanned::new(Expr::Str(s.clone()), span);
                push_literal(&mut stack, &mut current_document_expressions, expr);
            }

            Token::BareWord(s) => {
                let expr = Spanned::new(Expr::Str(s.clone()), span);
                push_literal(&mut stack, &mut current_document_expressions, expr);
            }

            Token::VarRef(name) => {
                let expr = Spanned::new(Expr::VarRef(name.clone()), span);
                push_literal(&mut stack, &mut current_document_expressions, expr);
            }

            // Other tokens: deferred to parser-core-b and later sprints
            Token::Comment(_) => {
                // Comment collection for formatter comes in parser-core-b
            }

            Token::Newline | Token::Semicolon => {
                // Whitespace/separators — ignored for now
            }

            Token::DocSeparator => {
                // Document separator — multi-document support in parser-core-b
                return Err(ParseError {
                    message: "document separators not yet supported (parser-core-b)".to_string(),
                    span: Some(span),
                });
            }

            Token::Dot => {
                return Err(ParseError {
                    message: "dot access not yet supported (parser-core-b)".to_string(),
                    span: Some(span),
                });
            }

            Token::Range => {
                return Err(ParseError {
                    message: "range operator not yet supported (parser-core-b)".to_string(),
                    span: Some(span),
                });
            }

            Token::At | Token::ImmediateAt => {
                return Err(ParseError {
                    message: "annotations not yet supported (parser-core-b)".to_string(),
                    span: Some(span),
                });
            }

            Token::Ellipsis => {
                return Err(ParseError {
                    message: "variadic/rest markers not yet supported (parser-core-b)".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: parse successfully and return the first expression from the first document.
    ///
    /// # SAFETY
    /// Assumes `parse2` returns exactly one document with exactly one expression.
    /// Valid only for single-expression test inputs. Do not use in multi-document
    /// or multi-expression contexts — the index will panic or return the wrong node.
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
    fn test_literal_int() {
        let expr = parse_expr("42");
        assert!(matches!(expr.node, Expr::Int(42)));
    }

    #[test]
    fn test_literal_float() {
        let expr = parse_expr("3.14");
        match expr.node {
            Expr::Float(f) => assert!((f - 3.14).abs() < 0.001),
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn test_literal_bool_true() {
        let expr = parse_expr("true");
        assert!(matches!(expr.node, Expr::Bool(true)));
    }

    #[test]
    fn test_literal_bool_false() {
        let expr = parse_expr("false");
        assert!(matches!(expr.node, Expr::Bool(false)));
    }

    #[test]
    fn test_literal_quoted_string() {
        let expr = parse_expr("\"hello\"");
        match expr.node {
            Expr::Str(s) => assert_eq!(s, "hello"),
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn test_literal_bare_word() {
        let expr = parse_expr("hello");
        match expr.node {
            Expr::Str(s) => assert_eq!(s, "hello"),
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn test_var_ref() {
        let expr = parse_expr("$x");
        match expr.node {
            Expr::VarRef(name) => assert_eq!(name, "x"),
            other => panic!("expected VarRef, got {other:?}"),
        }
    }

    #[test]
    fn test_dict_multiple_values() {
        let output = parse2("[1 2 3]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        match &doc.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 3);
                assert!(matches!(&entries[0].node.value.node, Expr::Int(1)));
                assert!(matches!(&entries[1].node.value.node, Expr::Int(2)));
                assert!(matches!(&entries[2].node.value.node, Expr::Int(3)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_nested_dict_one_level() {
        // [[1]] — outer dict with one entry that is itself a dict containing 1.
        // Verifies the parent-frame entry pushing at the CloseBracket handler.
        let output = parse2("[[1]]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        assert_eq!(doc.expressions.len(), 1);
        match &doc.expressions[0].node {
            Expr::Dict(outer_entries) => {
                assert_eq!(outer_entries.len(), 1);
                match &outer_entries[0].node.value.node {
                    Expr::Dict(inner_entries) => {
                        assert_eq!(inner_entries.len(), 1);
                        assert!(matches!(&inner_entries[0].node.value.node, Expr::Int(1)));
                    }
                    other => panic!("expected inner Dict, got {other:?}"),
                }
            }
            other => panic!("expected outer Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_nested_dict_two_levels() {
        // [[[42]]] — three levels deep.
        let output = parse2("[[[42]]]").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        assert_eq!(doc.expressions.len(), 1);
        match &doc.expressions[0].node {
            Expr::Dict(l1) => {
                assert_eq!(l1.len(), 1);
                match &l1[0].node.value.node {
                    Expr::Dict(l2) => {
                        assert_eq!(l2.len(), 1);
                        match &l2[0].node.value.node {
                            Expr::Dict(l3) => {
                                assert_eq!(l3.len(), 1);
                                assert!(matches!(&l3[0].node.value.node, Expr::Int(42)));
                            }
                            other => panic!("expected level-3 Dict, got {other:?}"),
                        }
                    }
                    other => panic!("expected level-2 Dict, got {other:?}"),
                }
            }
            other => panic!("expected level-1 Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_push_literal_top_level_vs_in_dict() {
        // Top-level literal: stack is empty, expression lands in current_document_expressions.
        let output = parse2("42").expect("parse failed");
        let doc = &output.file.node.documents[0].node;
        assert_eq!(doc.expressions.len(), 1);
        assert!(matches!(doc.expressions[0].node, Expr::Int(42)));

        // Inside a dict: stack is non-empty, literal is added to the parent frame's entries.
        let output2 = parse2("[42]").expect("parse failed");
        let doc2 = &output2.file.node.documents[0].node;
        assert_eq!(doc2.expressions.len(), 1);
        match &doc2.expressions[0].node {
            Expr::Dict(entries) => {
                assert_eq!(entries.len(), 1);
                assert!(matches!(&entries[0].node.value.node, Expr::Int(42)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_unmatched_closing_bracket() {
        let err = parse2("]").unwrap_err();
        assert_eq!(err.message, "unmatched closing bracket");
    }

    #[test]
    fn test_unclosed_bracket() {
        let err = parse2("[").unwrap_err();
        assert_eq!(err.message, "1 unclosed bracket(s)");
    }

    #[test]
    fn test_depth_limit_boundary_succeeds() {
        // Exactly MAX_PARSE_DEPTH (256) levels of nesting — must succeed.
        let mut input = String::new();
        for _ in 0..MAX_PARSE_DEPTH {
            input.push('[');
        }
        for _ in 0..MAX_PARSE_DEPTH {
            input.push(']');
        }
        parse2(&input).expect("256 levels of nesting should succeed");
    }

    #[test]
    fn test_depth_limit() {
        // Create deeply nested input
        let mut input = String::new();
        for _ in 0..MAX_PARSE_DEPTH {
            input.push('[');
        }
        input.push('['); // One more to exceed the limit
        for _ in 0..=MAX_PARSE_DEPTH {
            input.push(']');
        }

        let err = parse2(&input).unwrap_err();
        assert_eq!(
            err.message,
            format!("maximum nesting depth exceeded (limit: {MAX_PARSE_DEPTH})")
        );
    }

    #[test]
    fn test_deferred_dot_token() {
        // Token::Dot is emitted in access context (no whitespace after VarRef).
        // parse2("$a.b"): lexer emits VarRef("a"), Dot, BareWord("b").
        // After VarRef is pushed, Dot fires the explicit error arm.
        let err = parse2("$a.b").unwrap_err();
        assert_eq!(err.message, "dot access not yet supported (parser-core-b)");
    }

    #[test]
    fn test_deferred_range_token() {
        // Token::Range is emitted inside brackets (bracket_depth > 0).
        // parse2("[..]"): lexer emits OpenBracket, Range, CloseBracket.
        // After OpenBracket pushes a Dict frame, Range fires the explicit error arm.
        let err = parse2("[..]").unwrap_err();
        assert_eq!(
            err.message,
            "range operator not yet supported (parser-core-b)"
        );
    }

    #[test]
    fn test_deferred_at_token() {
        // Token::At fires the annotations match arm.
        // "@foo" tokenizes as At, BareWord("foo").
        let err = parse2("@foo").unwrap_err();
        assert_eq!(err.message, "annotations not yet supported (parser-core-b)");
    }

    #[test]
    fn test_deferred_ellipsis_token() {
        // Token::Ellipsis fires the variadic/rest match arm.
        // "..." is always lexed as Ellipsis (takes priority over range and dot).
        let err = parse2("...").unwrap_err();
        assert_eq!(
            err.message,
            "variadic/rest markers not yet supported (parser-core-b)"
        );
    }

    #[test]
    fn test_empty_input() {
        let output = parse2("").expect("parse failed");
        assert_eq!(output.file.node.documents.len(), 1);
        assert_eq!(output.file.node.documents[0].node.expressions.len(), 0);
    }

    #[test]
    fn test_parse_output_structure() {
        let output = parse2("42").expect("parse failed");
        assert_eq!(output.file.node.documents.len(), 1);
        assert!(output.leading_comments.is_empty());
        assert!(output.trailing_comments.is_empty());
    }

    #[test]
    fn test_stackframe_dict_construction() {
        // Test that StackFrame::Dict can be created
        let frame = StackFrame::Dict {
            entries: vec![],
            span_start: 0,
        };
        match frame {
            StackFrame::Dict { entries, .. } => assert_eq!(entries.len(), 0),
            _ => panic!("wrong frame type"),
        }
    }

    #[test]
    fn test_callarg_positional() {
        let arg = CallArg::Positional(Spanned::new(
            Expr::Int(42),
            Span {
                start: Position {
                    offset: 0,
                    line: 1,
                    column: 1,
                },
                end: Position {
                    offset: 2,
                    line: 1,
                    column: 3,
                },
            },
        ));
        match arg {
            CallArg::Positional(expr) => assert!(matches!(expr.node, Expr::Int(42))),
            _ => panic!("wrong arg type"),
        }
    }

    #[test]
    fn test_callarg_named() {
        let arg = CallArg::Named(
            "timeout".to_string(),
            Spanned::new(
                Expr::Int(60),
                Span {
                    start: Position {
                        offset: 0,
                        line: 1,
                        column: 1,
                    },
                    end: Position {
                        offset: 2,
                        line: 1,
                        column: 3,
                    },
                },
            ),
        );
        match arg {
            CallArg::Named(name, expr) => {
                assert_eq!(name, "timeout");
                assert!(matches!(expr.node, Expr::Int(60)));
            }
            _ => panic!("wrong arg type"),
        }
    }
}
