//! LSP analysis: hover text and diagnostics.

use lsp_types::{Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, Location, Url};

use crate::ast::{Expr, Span};
use crate::error::render_span_snippet;
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
            if let Some(text) =
                hover_at_expr(&expr.node, expr.span, offset, &doc.type_map, &doc.text)
            {
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
fn hover_at_expr(
    expr: &Expr,
    span: Span,
    offset: usize,
    type_map: &TypeMap,
    source: &str,
) -> Option<String> {
    if !span_contains(span, offset) {
        return None;
    }

    match expr {
        Expr::VarRef { name, .. } => {
            // Source-sniff: emit `$name` for EscapedRef tokens (first byte is `$`),
            // plain name for bare identifiers and `%`-prefixed refs (% is in name).
            let is_escaped = source
                .as_bytes()
                .get(span.start.offset)
                .map_or(false, |&b| b == b'$');
            let display = if is_escaped {
                format!("${name}")
            } else {
                name.clone()
            };
            Some(format!(
                "Variable: {display}{}",
                type_suffix(span, type_map)
            ))
        }
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
            hover_at_expr(&target.node, target.span, offset, type_map, source).or_else(|| {
                Some(format!(
                    "Field access: .{field}{}",
                    type_suffix(span, type_map)
                ))
            })
        }

        Expr::Dict(entries) => {
            for entry in entries {
                if let Some(ref key) = entry.node.key {
                    if let Some(text) = hover_at_expr(&key.node, key.span, offset, type_map, source)
                    {
                        return Some(text);
                    }
                }
                if let Some(text) = hover_at_expr(
                    &entry.node.value.node,
                    entry.node.value.span,
                    offset,
                    type_map,
                    source,
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
            implied: _,
        } => hover_at_expr(&func.node, func.span, offset, type_map, source)
            .or_else(|| {
                args.iter()
                    .find_map(|a| hover_at_expr(&a.node, a.span, offset, type_map, source))
            })
            .or_else(|| {
                named_args.iter().find_map(|na| {
                    hover_at_expr(
                        &na.node.value.node,
                        na.node.value.span,
                        offset,
                        type_map,
                        source,
                    )
                })
            }),

        Expr::Fn { params, body, .. } => {
            // Check if hover is on a parameter name (approximate).
            for param in params {
                if span_contains(param.span, offset) {
                    return Some(format!("Parameter: {}", param.node.name));
                }
            }
            hover_at_expr(&body.node, body.span, offset, type_map, source)
        }

        Expr::TypeAlias(inner) => hover_at_expr(&inner.node, inner.span, offset, type_map, source),

        Expr::TypeAssert {
            expr: inner,
            annotation,
            ..
        } => {
            // Check inner expression first, then fall back to annotation text.
            hover_at_expr(&inner.node, inner.span, offset, type_map, source).or_else(|| {
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

        Expr::Pipe { lhs, rhs } => hover_at_expr(&lhs.node, lhs.span, offset, type_map, source)
            .or_else(|| hover_at_expr(&rhs.node, rhs.span, offset, type_map, source)),

        Expr::Error(span) => Some(format!(
            "Parse error at {}:{}",
            span.start.line, span.start.column
        )),
    }
}

/// Check if a span contains a byte offset.
fn span_contains(span: Span, offset: usize) -> bool {
    offset >= span.start.offset && offset < span.end.offset
}

/// Convert document errors to LSP diagnostics.
///
/// `uri` is the document's URI, used to construct `DiagnosticRelatedInformation`
/// locations that point back into the same file.
pub fn diagnostics_for(doc: &DocumentState, uri: &Url) -> Vec<Diagnostic> {
    let source = &doc.text;
    let mut diagnostics = Vec::new();

    // Fatal parse error (lexer failure or unclosed brackets) -> Error severity
    if let Err(ref err) = doc.ast {
        diagnostics.push(parse_error_to_diagnostic(err, source));
    }

    // Recovered parse errors (bracket-level recovery) -> Error severity
    for err in &doc.parse_errors {
        diagnostics.push(parse_error_to_diagnostic(err, source));
    }

    // Type errors -> Warning severity (advisory)
    for err in &doc.type_errors {
        diagnostics.push(type_error_to_diagnostic(err, source));
    }

    // Eval errors -> Error severity
    for err in &doc.eval_errors {
        diagnostics.push(eval_error_to_diagnostic(err, source, uri));
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

    // ParseError carries at most one span (the error site), so related_information
    // is always None — there is no separate definition/use site pair.
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

    // TypeError carries one span (the annotation site), so related_information
    // is always None — the type checker does not yet track separate definition/use sites.
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

fn eval_error_to_diagnostic(err: &crate::error::EvalError, source: &str, uri: &Url) -> Diagnostic {
    let range = llt_span_to_lsp_range(&err.definition_span, source);

    // Collect related_information from:
    //  1. The materialization span (where the thunk was forced), if different from definition.
    //  2. Each stack frame (the call chain leading to the error).
    //
    // This lets editor users click through the full lazy-evaluation call chain:
    //   primary diagnostic → definition site
    //   related[0]         → materialization site ("forced here")
    //   related[1..]       → stack frames ("called from …")
    let related_information = {
        let mut related: Vec<DiagnosticRelatedInformation> = Vec::new();

        // Materialization span: where the lazy value was forced.
        if let Some(mat_span) = err.materialization_span {
            if mat_span != err.definition_span {
                let mat_range = llt_span_to_lsp_range(&mat_span, source);
                let snippet = render_span_snippet(source, mat_span)
                    .map(|s| format!("\n{s}"))
                    .unwrap_or_default();
                related.push(DiagnosticRelatedInformation {
                    location: Location {
                        uri: uri.clone(),
                        range: mat_range,
                    },
                    message: format!("forced here{snippet}"),
                });
            }
        }

        // Stack frames: the call chain that triggered materialization.
        for frame in &err.stack {
            // Skip synthetic (Span::origin()) frames — they are stdlib/builtin internals
            // that pollute user-facing traces.
            if frame.span.start.offset == 0 && frame.span.end.offset == 0 {
                continue;
            }
            let frame_range = llt_span_to_lsp_range(&frame.span, source);
            let snippet = render_span_snippet(source, frame.span)
                .map(|s| format!("\n{s}"))
                .unwrap_or_default();
            related.push(DiagnosticRelatedInformation {
                location: Location {
                    uri: uri.clone(),
                    range: frame_range,
                },
                message: format!("called from {}{snippet}", frame.label),
            });
        }

        if related.is_empty() {
            None
        } else {
            Some(related)
        }
    };

    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(lsp_types::NumberOrString::String(
            err.kind.code().to_string(),
        )),
        code_description: None,
        source: Some("tinct-eval".to_string()),
        message: err.message(),
        related_information,
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
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        crate::eval::EvalContext::new(base_dir, test_env(), true)
    }

    /// Canonical test URI for diagnostics_for() calls.
    fn test_uri() -> Url {
        Url::parse("file:///test.llt").unwrap()
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
        let diags = diagnostics_for(&doc, &test_uri());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diags[0].source, Some("tinct-parser".to_string()));
    }

    #[test]
    fn test_diagnostics_type_error() {
        let env = test_env();
        let doc = DocumentState::new("[@Number hello]".to_string(), &env, &test_ctx());
        let diags = diagnostics_for(&doc, &test_uri());
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
        let diags = diagnostics_for(&doc, &test_uri());
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
        let diags = diagnostics_for(&doc, &test_uri());
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
        // $undefined has type <error> when inference fails -- LSP hover shows the sentinel
        // so users can see that the expression has a type error rather than seeing Any.
        let doc = DocumentState::new("$undefined".to_string(), &env, &test_ctx());
        let hover = hover_at(&doc, 1);
        assert!(hover.is_some());
        let text = hover.unwrap();
        assert_eq!(text, "Variable: $undefined (<error>)");
    }

    /// Task 7: verify that `eval_error_to_diagnostic` populates `related_information`
    /// when the eval error has a materialization span distinct from the definition span.
    ///
    /// We build an `EvalError` directly with both spans set and verify the resulting
    /// `Diagnostic` has non-None `related_information` containing the "forced here" entry.
    #[test]
    fn test_eval_error_to_diagnostic_related_information() {
        use crate::ast::{Position, Span};
        use crate::error::EvalError;

        let def_span = Span {
            start: Position {
                offset: 4,
                line: 1,
                column: 5,
            },
            end: Position {
                offset: 7,
                line: 1,
                column: 8,
            },
        };
        let mat_span = Span {
            start: Position {
                offset: 10,
                line: 2,
                column: 1,
            },
            end: Position {
                offset: 15,
                line: 2,
                column: 6,
            },
        };

        let err =
            EvalError::key_not_found("foo", vec![], def_span).with_materialization_span(mat_span);

        let source = "[x: 1]\n[y: $z]";
        let uri = test_uri();
        let diag = eval_error_to_diagnostic(&err, source, &uri);

        // related_information must be Some: materialization span is different from definition span.
        let related = diag.related_information.as_ref().expect(
            "eval_error_to_diagnostic should populate related_information when \
             materialization_span differs from definition_span",
        );
        assert!(
            !related.is_empty(),
            "related_information should be non-empty"
        );

        // First entry is the materialization site.
        let mat_entry = &related[0];
        assert!(
            mat_entry.message.contains("forced here"),
            "first related entry should say 'forced here', got: {}",
            mat_entry.message
        );

        // The location URI must match the document URI.
        assert_eq!(
            mat_entry.location.uri, uri,
            "related_information location URI must match the document URI"
        );
    }

    /// Verify that when an eval error has no materialization span (immediate error,
    /// definition == use site), `related_information` is None.
    #[test]
    fn test_eval_error_to_diagnostic_no_related_when_no_mat_span() {
        use crate::ast::{Position, Span};
        use crate::error::EvalError;

        let def_span = Span {
            start: Position {
                offset: 0,
                line: 1,
                column: 1,
            },
            end: Position {
                offset: 10,
                line: 1,
                column: 11,
            },
        };

        // No materialization span set, no stack frames.
        let err = EvalError::key_not_found("bar", vec![], def_span);

        let source = "$undefined_x";
        let diag = eval_error_to_diagnostic(&err, source, &test_uri());

        // No extra spans → related_information should be None.
        assert!(
            diag.related_information.is_none(),
            "related_information should be None when error has no mat_span and no stack frames"
        );
    }
}
