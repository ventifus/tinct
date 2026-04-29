//! LSP analysis: hover text and diagnostics.

use lsp_types::{Diagnostic, DiagnosticSeverity};

use crate::ast::{Expr, Span};
use crate::lsp::convert::llt_span_to_lsp_range;
use crate::lsp::document::DocumentState;
use crate::parser::ParseError;
use crate::typecheck::TypeMap;
use crate::types::TypeError;

/// Generate hover text for the entity at the given byte offset.
///
/// Returns `None` if no meaningful hover information is available.
pub fn hover_at(doc: &DocumentState, offset: usize) -> Option<String> {
    let file = match &doc.ast {
        Ok(f) => f,
        Err(_) => return None,
    };

    // Walk the AST to find the node containing the offset.
    for document in &file.node.documents {
        for expr in &document.node.expressions {
            if let Some(text) = hover_at_expr(&expr.node, expr.span, offset, &doc.type_map) {
                return Some(text);
            }
        }
    }

    None
}

/// Look up the inferred type for the given span in the type map.
/// Returns a formatted suffix like " (Int)" or empty string if not found.
fn type_suffix(span: Span, type_map: &TypeMap) -> String {
    let key = (span.start.offset, span.end.offset);
    match type_map.get(&key) {
        Some(ty) => format!(" ({ty})"),
        None => String::new(),
    }
}

/// Recursively search an expression tree for the node at the given offset.
fn hover_at_expr(expr: &Expr, span: Span, offset: usize, type_map: &TypeMap) -> Option<String> {
    if !span_contains(span, offset) {
        return None;
    }

    match expr {
        Expr::VarRef(name) => Some(format!("Variable: ${name}{}", type_suffix(span, type_map))),
        Expr::Int(n) => Some(format!("Int literal: {n}{}", type_suffix(span, type_map))),
        Expr::Float(f) => Some(format!("Float literal: {f}{}", type_suffix(span, type_map))),
        Expr::Bool(b) => Some(format!("Bool literal: {b}{}", type_suffix(span, type_map))),
        Expr::Str(s) => Some(format!(
            "String literal: {s:?}{}",
            type_suffix(span, type_map)
        )),

        Expr::DotAccess {
            expr: target,
            field,
        } => {
            // Check if hover is on the field name (assumes field starts after dot).
            hover_at_expr(&target.node, target.span, offset, type_map).or_else(|| {
                Some(format!(
                    "Field access: .{field}{}",
                    type_suffix(span, type_map)
                ))
            })
        }

        Expr::BracketAccess { expr: target, key } => {
            hover_at_expr(&target.node, target.span, offset, type_map)
                .or_else(|| hover_at_expr(&key.node, key.span, offset, type_map))
        }

        Expr::RangeAccess {
            expr: target,
            start,
            end,
        } => hover_at_expr(&target.node, target.span, offset, type_map)
            .or_else(|| {
                start
                    .as_ref()
                    .and_then(|s| hover_at_expr(&s.node, s.span, offset, type_map))
            })
            .or_else(|| {
                end.as_ref()
                    .and_then(|e| hover_at_expr(&e.node, e.span, offset, type_map))
            }),

        Expr::Dict(entries) => {
            for entry in entries {
                if let Some(ref key) = entry.node.key {
                    if let Some(text) = hover_at_expr(&key.node, key.span, offset, type_map) {
                        return Some(text);
                    }
                }
                if let Some(text) = hover_at_expr(
                    &entry.node.value.node,
                    entry.node.value.span,
                    offset,
                    type_map,
                ) {
                    return Some(text);
                }
            }
            None
        }

        Expr::Call {
            func,
            args,
            named_args,
        } => hover_at_expr(&func.node, func.span, offset, type_map)
            .or_else(|| {
                args.iter()
                    .find_map(|a| hover_at_expr(&a.node, a.span, offset, type_map))
            })
            .or_else(|| {
                named_args.iter().find_map(|na| {
                    hover_at_expr(&na.node.value.node, na.node.value.span, offset, type_map)
                })
            }),

        Expr::Fn { params, body, .. } => {
            // Check if hover is on a parameter name (approximate).
            for param in params {
                if span_contains(param.span, offset) {
                    return Some(format!("Parameter: {}", param.node.name));
                }
            }
            hover_at_expr(&body.node, body.span, offset, type_map)
        }

        Expr::TypeAlias(inner) => hover_at_expr(&inner.node, inner.span, offset, type_map),

        Expr::TypeAssert {
            expr: inner,
            annotation,
            ..
        } => {
            // Check inner expression first, then fall back to annotation text.
            hover_at_expr(&inner.node, inner.span, offset, type_map).or_else(|| {
                Some(format!(
                    "Type assertion: @{}{}",
                    annotation.node,
                    type_suffix(span, type_map)
                ))
            })
        }

        Expr::Annotated { name, annotation } => Some(format!(
            "Annotated: {}@{}{}",
            name,
            annotation.node,
            type_suffix(span, type_map)
        )),

        Expr::Rest(name) => Some(format!("Rest marker: {}", name.as_deref().unwrap_or("..."))),
    }
}

/// Check if a span contains a byte offset.
fn span_contains(span: Span, offset: usize) -> bool {
    offset >= span.start.offset && offset < span.end.offset
}

/// Convert document errors to LSP diagnostics.
pub fn diagnostics_for(doc: &DocumentState) -> Vec<Diagnostic> {
    let source = &doc.text;
    let mut diagnostics = Vec::new();

    // Parse errors -> Error severity
    if let Err(ref err) = doc.ast {
        diagnostics.push(parse_error_to_diagnostic(err, source));
    }

    // Type errors -> Warning severity (advisory)
    for err in &doc.type_errors {
        diagnostics.push(type_error_to_diagnostic(err, source));
    }

    // Eval errors -> Error severity
    for err in &doc.eval_errors {
        diagnostics.push(eval_error_to_diagnostic(err, source));
    }

    diagnostics
}

fn parse_error_to_diagnostic(err: &ParseError, source: &str) -> Diagnostic {
    // ParseError may or may not have a span; use a default range if none.
    let range = if let Some(span) = err.span {
        llt_span_to_lsp_range(&span, source)
    } else {
        // Default to line 0, character 0.
        lsp_types::Range {
            start: lsp_types::Position {
                line: 0,
                character: 0,
            },
            end: lsp_types::Position {
                line: 0,
                character: 0,
            },
        }
    };

    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: None,
        code_description: None,
        source: Some("tinct-parser".to_string()),
        message: err.message.clone(),
        related_information: None,
        tags: None,
        data: None,
    }
}

fn type_error_to_diagnostic(err: &TypeError, source: &str) -> Diagnostic {
    let range = llt_span_to_lsp_range(&err.span, source);

    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::WARNING),
        code: None,
        code_description: None,
        source: Some("tinct-typecheck".to_string()),
        message: err.message.clone(),
        related_information: None,
        tags: None,
        data: None,
    }
}

fn eval_error_to_diagnostic(err: &crate::error::EvalError, source: &str) -> Diagnostic {
    let range = llt_span_to_lsp_range(&err.definition_span, source);

    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(lsp_types::NumberOrString::String(
            err.kind.code().to_string(),
        )),
        code_description: None,
        source: Some("tinct-eval".to_string()),
        message: err.message(),
        related_information: None,
        tags: None,
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::builtins::create_stdlib_env;
    use crate::value::Environment;

    /// Helper: create a stdlib env for tests.
    fn test_env() -> Rc<RefCell<Environment>> {
        create_stdlib_env().unwrap()
    }

    /// Helper: create an EvalContext for tests.
    fn test_ctx() -> Rc<crate::eval::EvalContext> {
        crate::eval::EvalContext::new(std::path::PathBuf::from("."), test_env(), true)
    }

    #[test]
    fn test_hover_int_literal() {
        let env = test_env();
        let doc = DocumentState::new("42".to_string(), &env, &test_ctx());
        let hover = hover_at(&doc, 0);
        assert_eq!(hover.as_deref(), Some("Int literal: 42 (42)"));
    }

    #[test]
    fn test_hover_var_ref() {
        let env = test_env();
        // $x is undefined, so no type is inferred -- just syntactic info.
        let doc = DocumentState::new("$x".to_string(), &env, &test_ctx());
        let hover = hover_at(&doc, 1);
        assert!(hover.is_some());
        assert!(hover.unwrap().contains("Variable: $x"));
    }

    #[test]
    fn test_hover_var_ref_with_type() {
        let env = test_env();
        // $x is defined in scope, so hover should show its type.
        let doc = DocumentState::new("[x: 42]\n[y: $x]".to_string(), &env, &test_ctx());
        // Offset 12 is inside "$x" in the second expression "[y: $x]"
        // "[x: 42]\n[y: $x]"
        //  0123456 7 89012345
        let hover = hover_at(&doc, 12);
        assert!(hover.is_some());
        let text = hover.unwrap();
        assert!(text.contains("Variable: $x"), "got: {text}");
        assert!(text.contains("(42)"), "should show type, got: {text}");
    }

    #[test]
    fn test_hover_string_literal() {
        let env = test_env();
        let doc = DocumentState::new("\"hello\"".to_string(), &env, &test_ctx());
        let hover = hover_at(&doc, 2);
        assert!(hover.is_some());
        let text = hover.unwrap();
        assert!(text.contains("String literal"), "got: {text}");
        assert!(
            text.contains("(\"hello\")"),
            "should show type, got: {text}"
        );
    }

    #[test]
    fn test_hover_no_match() {
        let env = test_env();
        let doc = DocumentState::new("[x: 1]".to_string(), &env, &test_ctx());
        // Hover on whitespace between entries.
        let hover = hover_at(&doc, 100);
        assert!(hover.is_none());
    }

    #[test]
    fn test_diagnostics_parse_error() {
        let env = test_env();
        let doc = DocumentState::new("[unterminated".to_string(), &env, &test_ctx());
        let diags = diagnostics_for(&doc);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diags[0].source, Some("tinct-parser".to_string()));
    }

    #[test]
    fn test_diagnostics_type_error() {
        let env = test_env();
        let doc = DocumentState::new("[@Number hello]".to_string(), &env, &test_ctx());
        let diags = diagnostics_for(&doc);
        assert!(!diags.is_empty());
        let type_diag = diags
            .iter()
            .find(|d| d.source.as_deref() == Some("tinct-typecheck"))
            .unwrap();
        assert_eq!(type_diag.severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn test_diagnostics_eval_error() {
        let env = test_env();
        let doc = DocumentState::new("$undefined".to_string(), &env, &test_ctx());
        let diags = diagnostics_for(&doc);
        assert!(!diags.is_empty());
        let eval_diag = diags
            .iter()
            .find(|d| d.source.as_deref() == Some("tinct-eval"))
            .unwrap();
        assert_eq!(eval_diag.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            eval_diag.code,
            Some(lsp_types::NumberOrString::String("E002".to_string())),
            "eval diagnostic should include error code E002 (UndefinedVariable)"
        );
    }

    #[test]
    fn test_diagnostics_valid_source() {
        let env = test_env();
        let doc = DocumentState::new("[x: 42]".to_string(), &env, &test_ctx());
        let diags = diagnostics_for(&doc);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_hover_dict_entry_key() {
        let env = test_env();
        let doc = DocumentState::new("[x: 42]".to_string(), &env, &test_ctx());
        let hover = hover_at(&doc, 1); // on 'x'
        assert!(hover.is_some());
    }

    #[test]
    fn test_hover_dict_entry_value() {
        let env = test_env();
        let doc = DocumentState::new("[x: 42]".to_string(), &env, &test_ctx());
        let hover = hover_at(&doc, 4); // on '42'
        assert!(hover.is_some());
        let text = hover.unwrap();
        assert!(text.contains("Int literal"), "got: {text}");
        assert!(text.contains("(42)"), "should show type, got: {text}");
    }

    #[test]
    fn test_hover_nested_dict() {
        let env = test_env();
        let doc = DocumentState::new("[a: [b: 1]]".to_string(), &env, &test_ctx());
        let hover = hover_at(&doc, 8); // on '1'
        assert!(hover.is_some());
        let text = hover.unwrap();
        assert!(text.contains("Int literal"), "got: {text}");
    }

    #[test]
    fn test_hover_function_param() {
        let env = test_env();
        let doc = DocumentState::new("[fn [x] $x]".to_string(), &env, &test_ctx());
        let hover = hover_at(&doc, 5); // on 'x' in param list
        assert!(hover.is_some());
        assert!(hover.unwrap().contains("Parameter"));
    }

    #[test]
    fn test_hover_call_expression() {
        let env = test_env();
        let doc = DocumentState::new("[call $+ 1 2]".to_string(), &env, &test_ctx());
        let hover = hover_at(&doc, 6); // on '$+'
        assert!(hover.is_some());
        assert!(hover.unwrap().contains("Variable: $+"));
    }

    #[test]
    fn test_hover_float_literal() {
        let env = test_env();
        let doc = DocumentState::new("3.14".to_string(), &env, &test_ctx());
        let hover = hover_at(&doc, 0);
        assert_eq!(hover.as_deref(), Some("Float literal: 3.14 (Float)"));
    }

    #[test]
    fn test_hover_bool_literal() {
        let env = test_env();
        let doc = DocumentState::new("true".to_string(), &env, &test_ctx());
        let hover = hover_at(&doc, 0);
        assert_eq!(hover.as_deref(), Some("Bool literal: true (Bool)"));
    }

    #[test]
    fn test_hover_type_not_shown_on_error() {
        let env = test_env();
        // $undefined has no type -- hover should show syntactic info only.
        let doc = DocumentState::new("$undefined".to_string(), &env, &test_ctx());
        let hover = hover_at(&doc, 1);
        assert!(hover.is_some());
        let text = hover.unwrap();
        assert_eq!(text, "Variable: $undefined");
    }
}
