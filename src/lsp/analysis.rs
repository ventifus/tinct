//! LSP analysis: hover text and diagnostics.

use lsp_types::{Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, Location, Uri};

use crate::ast::{Expr, Span};
use crate::error::render_span_snippet;
use crate::lsp::convert::llt_span_to_lsp_range;
use crate::lsp::document::DocumentState;
use crate::parser::ParseError;
use crate::typecheck::{DocMap, SchemeMap, TypeMap};
use crate::types::{pretty_type_str, TypeError, TypeScheme};

/// Generate hover text for the entity at the given byte offset.
///
/// Returns `None` if no meaningful hover information is available.
///
/// Also searches direct includes' type maps for cross-file hover.
///
/// Note: Prelude hover fallback via prelude span map is not currently supported
/// after migration to the shared imports module. Prelude types are still seeded
/// via imports::build_type_env(), so hover should work for prelude functions if
/// the type checker inferred their types correctly.
pub fn hover_at(
    doc: &DocumentState,
    doc_url: &Uri,
    offset: usize,
    include_graph: &crate::lsp::document::IncludeGraph,
) -> Option<String> {
    let file = match &doc.ast {
        Ok(f) => f,
        Err(_) => return None,
    };

    // Walk the AST to find the node containing the offset.
    for document in &file.node.documents {
        for expr in &document.node.expressions {
            if let Some(text) = hover_at_expr(
                &expr.node,
                expr.span,
                offset,
                &doc.type_map,
                &doc.scheme_map,
                &doc.doc_map,
                &doc.text,
                include_graph,
                doc_url,
            ) {
                return Some(text);
            }
        }
    }

    None
}

/// Format a TypeScheme for LSP hover display.
///
/// Shows constraints and the body type without the `∀` quantifier, since the
/// quantifier is an implementation detail (the caller already sees fresh type vars
/// like `a`, `b` from the `pretty_type_str` renaming pass).
///
/// Examples:
///   - `Equatable a => Fn@Bool [a a]` (constrained polymorphic)
///   - `Fn@Bool [a a]` (polymorphic, no constraints)
fn format_scheme_for_hover(scheme: &TypeScheme) -> String {
    if scheme.constraints.is_empty() {
        scheme.body.to_string()
    } else {
        let constraints = scheme
            .constraints
            .iter()
            .map(|c| format!("{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{constraints} => {}", scheme.body)
    }
}

/// Look up the inferred type for the given span in the type map.
/// Returns a formatted suffix like " (Int)" or empty string if not found.
///
/// Prefers the TypeScheme display (with constraints) for polymorphic VarRef sites.
/// Falls back to direct includes' type maps if the document type map has no entry.
///
/// Note: Prelude type map fallback is not currently supported after migration to
/// the shared imports module. Prelude types should still appear in hover if the
/// type checker inferred them correctly (via imports::build_type_env seeding).
fn type_suffix(
    span: Span,
    type_map: &TypeMap,
    scheme_map: &SchemeMap,
    include_graph: &crate::lsp::document::IncludeGraph,
    doc_url: &Uri,
) -> String {
    let key = (span.start.offset, span.end.offset);

    // Prefer TypeScheme display (has constraints) for polymorphic VarRef sites.
    // This shows e.g. "Equatable a => Fn@Bool [a a]" instead of the instantiated
    // "Fn@Bool [_t42 _t42]" which would be renamed to "Fn@Bool [a a]" but without constraints.
    if let Some(scheme) = scheme_map.get(&key) {
        let raw = format_scheme_for_hover(scheme);
        return format!(" ({})", pretty_type_str(&raw));
    }

    // Try document type map next
    if let Some(ty) = type_map.get(&key) {
        return format!(" ({})", crate::types::pretty_type(ty));
    }

    // Try direct includes' type maps
    if let Some(node) = include_graph.get(doc_url) {
        for include_url in &node.includes {
            if let Some(include_node) = include_graph.get(include_url) {
                if let Some(ty) = include_node.state.type_map.get(&key) {
                    return format!(" ({})", crate::types::pretty_type(ty));
                }
            }
        }
    }

    String::new()
}

/// Look up the documentation string for a given variable/parameter name.
/// Returns a formatted suffix like "\n\nDoc string here" or empty string if not found.
fn doc_suffix(name: &str, doc_map: &DocMap) -> String {
    if let Some(doc) = doc_map.get(name) {
        format!("\n\n{}", doc)
    } else {
        String::new()
    }
}

/// Recursively search an expression tree for the node at the given offset.
fn hover_at_expr(
    expr: &Expr,
    span: Span,
    offset: usize,
    type_map: &TypeMap,
    scheme_map: &SchemeMap,
    doc_map: &DocMap,
    source: &str,
    include_graph: &crate::lsp::document::IncludeGraph,
    doc_url: &Uri,
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
                "Variable: {display}{}{}",
                type_suffix(span, type_map, scheme_map, include_graph, doc_url),
                doc_suffix(name, doc_map)
            ))
        }
        Expr::Int(n) => Some(format!(
            "Int literal: {n}{}",
            type_suffix(span, type_map, scheme_map, include_graph, doc_url)
        )),
        Expr::Float(f) => Some(format!(
            "Float literal: {f}{}",
            type_suffix(span, type_map, scheme_map, include_graph, doc_url)
        )),
        Expr::Bool(b) => Some(format!(
            "Bool literal: {b}{}",
            type_suffix(span, type_map, scheme_map, include_graph, doc_url)
        )),
        Expr::Str(s) => Some(format!(
            "String literal: {s:?}{}",
            type_suffix(span, type_map, scheme_map, include_graph, doc_url)
        )),

        Expr::DotAccess {
            expr: target,
            field,
        } => {
            // Check if hover is on the field name (assumes field starts after dot).
            hover_at_expr(
                &target.node,
                target.span,
                offset,
                type_map,
                scheme_map,
                doc_map,
                source,
                include_graph,
                doc_url,
            )
            .or_else(|| {
                Some(format!(
                    "Field access: .{field}{}",
                    type_suffix(span, type_map, scheme_map, include_graph, doc_url)
                ))
            })
        }

        Expr::Dict(entries) => {
            for entry in entries {
                if let Some(ref key) = entry.node.key {
                    if span_contains(key.span, offset) {
                        // Cursor is on a binding key — show "name (type)\n\ndoc" so
                        // the user sees both the binding name and its bound type.
                        // Extract the display name from the key, covering all key forms.
                        let display_name: Option<String> = match &key.node {
                            Expr::VarRef { name, .. } => Some(name.clone()),
                            // `name@[doc: "..."]` or `name@Type` key annotation
                            Expr::Annotated { name, .. } => Some(name.clone()),
                            // String literal keys: `"response->ok":` or hyphenated names
                            Expr::Str(s) => Some(s.clone()),
                            _ => None,
                        };
                        if let Some(display) = display_name {
                            let ty =
                                type_suffix(entry.node.value.span, type_map, scheme_map, include_graph, doc_url);
                            // Only look up doc for bare-name keys (not string literals)
                            let doc_name = match &key.node {
                                Expr::VarRef { name, .. } | Expr::Annotated { name, .. } => {
                                    Some(name.as_str())
                                }
                                _ => None,
                            };
                            let doc = doc_name
                                .map(|n| doc_suffix(n, doc_map))
                                .unwrap_or_default();
                            return Some(format!("{display}{ty}{doc}"));
                        }
                        // Dynamic key expression — fall back to key hover.
                        if let Some(text) = hover_at_expr(
                            &key.node,
                            key.span,
                            offset,
                            type_map,
                            scheme_map,
                            doc_map,
                            source,
                            include_graph,
                            doc_url,
                        ) {
                            return Some(text);
                        }
                    }
                }
                if let Some(text) = hover_at_expr(
                    &entry.node.value.node,
                    entry.node.value.span,
                    offset,
                    type_map,
                    scheme_map,
                    doc_map,
                    source,
                    include_graph,
                    doc_url,
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
        } => hover_at_expr(
            &func.node,
            func.span,
            offset,
            type_map,
            scheme_map,
            doc_map,
            source,
            include_graph,
            doc_url,
        )
        .or_else(|| {
            args.iter().find_map(|a| {
                hover_at_expr(
                    &a.node,
                    a.span,
                    offset,
                    type_map,
                    scheme_map,
                    doc_map,
                    source,
                    include_graph,
                    doc_url,
                )
            })
        })
        .or_else(|| {
            named_args.iter().find_map(|na| {
                hover_at_expr(
                    &na.node.value.node,
                    na.node.value.span,
                    offset,
                    type_map,
                    scheme_map,
                    doc_map,
                    source,
                    include_graph,
                    doc_url,
                )
            })
        }),

        Expr::Fn { params, body, .. } => {
            // Check if hover is on a parameter name (approximate).
            for param in params {
                if span_contains(param.span, offset) {
                    return Some(format!(
                        "Parameter: {}{}",
                        param.node.name,
                        doc_suffix(&param.node.name, doc_map)
                    ));
                }
            }
            hover_at_expr(
                &body.node,
                body.span,
                offset,
                type_map,
                scheme_map,
                doc_map,
                source,
                include_graph,
                doc_url,
            )
        }

        Expr::TypeAlias { body, .. } => hover_at_expr(
            &body.node,
            body.span,
            offset,
            type_map,
            scheme_map,
            doc_map,
            source,
            include_graph,
            doc_url,
        ),

        Expr::Quote(inner) | Expr::Unquote(inner) | Expr::UnquoteSplice(inner) => hover_at_expr(
            &inner.node,
            inner.span,
            offset,
            type_map,
            scheme_map,
            doc_map,
            source,
            include_graph,
            doc_url,
        ),

        Expr::DefMacro { name, transformer } => {
            // Check if hover is on the transformer
            hover_at_expr(
                &transformer.node,
                transformer.span,
                offset,
                type_map,
                scheme_map,
                doc_map,
                source,
                include_graph,
                doc_url,
            )
            .or_else(|| Some(format!("Macro definition: {}", name)))
        }

        Expr::TypeAssert {
            expr: inner,
            annotation,
            ..
        } => {
            // Check inner expression first, then fall back to annotation text.
            hover_at_expr(
                &inner.node,
                inner.span,
                offset,
                type_map,
                scheme_map,
                doc_map,
                source,
                include_graph,
                doc_url,
            )
            .or_else(|| {
                Some(format!(
                    "Type assertion: @{}{}",
                    annotation.node,
                    type_suffix(span, type_map, scheme_map, include_graph, doc_url)
                ))
            })
        }

        Expr::Annotated { name, annotation } => Some(format!(
            "Annotated: {}@{}{}",
            name,
            annotation.node,
            type_suffix(span, type_map, scheme_map, include_graph, doc_url)
        )),

        Expr::Rest(name) => Some(format!("Rest marker: {}", name.as_deref().unwrap_or("..."))),

        Expr::Sequential(exprs) => {
            for seq_expr in exprs {
                if let Some(text) = hover_at_expr(
                    &seq_expr.node,
                    seq_expr.span,
                    offset,
                    type_map,
                    scheme_map,
                    doc_map,
                    source,
                    include_graph,
                    doc_url,
                ) {
                    return Some(text);
                }
            }
            None
        }

        Expr::Pipe { lhs, rhs } => hover_at_expr(
            &lhs.node,
            lhs.span,
            offset,
            type_map,
            scheme_map,
            doc_map,
            source,
            include_graph,
            doc_url,
        )
        .or_else(|| {
            hover_at_expr(
                &rhs.node,
                rhs.span,
                offset,
                type_map,
                scheme_map,
                doc_map,
                source,
                include_graph,
                doc_url,
            )
        }),

        Expr::Match { scrutinee, arms } => hover_at_expr(
            &scrutinee.node,
            scrutinee.span,
            offset,
            type_map,
            scheme_map,
            doc_map,
            source,
            include_graph,
            doc_url,
        )
        .or_else(|| {
            for arm in arms {
                if let Some(text) = hover_at_expr(
                    &arm.body.node,
                    arm.body.span,
                    offset,
                    type_map,
                    scheme_map,
                    doc_map,
                    source,
                    include_graph,
                    doc_url,
                ) {
                    return Some(text);
                }
            }
            None
        }),

        Expr::ClassDecl { methods, .. } => {
            for method in methods {
                if let Some(key) = &method.node.key {
                    if let Some(text) = hover_at_expr(
                        &key.node,
                        key.span,
                        offset,
                        type_map,
                        scheme_map,
                        doc_map,
                        source,
                        include_graph,
                        doc_url,
                    ) {
                        return Some(text);
                    }
                }
                if let Some(text) = hover_at_expr(
                    &method.node.value.node,
                    method.node.value.span,
                    offset,
                    type_map,
                    scheme_map,
                    doc_map,
                    source,
                    include_graph,
                    doc_url,
                ) {
                    return Some(text);
                }
            }
            None
        }

        Expr::InstanceDecl {
            instance_type,
            methods,
            ..
        } => hover_at_expr(
            &instance_type.node,
            instance_type.span,
            offset,
            type_map,
            scheme_map,
            doc_map,
            source,
            include_graph,
            doc_url,
        )
        .or_else(|| {
            for method in methods {
                if let Some(key) = &method.node.key {
                    if let Some(text) = hover_at_expr(
                        &key.node,
                        key.span,
                        offset,
                        type_map,
                        scheme_map,
                        doc_map,
                        source,
                        include_graph,
                        doc_url,
                    ) {
                        return Some(text);
                    }
                }
                if let Some(text) = hover_at_expr(
                    &method.node.value.node,
                    method.node.value.span,
                    offset,
                    type_map,
                    scheme_map,
                    doc_map,
                    source,
                    include_graph,
                    doc_url,
                ) {
                    return Some(text);
                }
            }
            None
        }),

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

/// Extract the static name from a dict entry key expression.
///
/// Returns `Some(name)` for string literals and annotated keys (`x@Type`),
/// `None` for all other key forms (including integer literals and variable
/// references, which are not static definition targets).
pub(crate) fn key_name(key_expr: &Expr) -> Option<&str> {
    match key_expr {
        Expr::Str(s) => Some(s.as_str()),
        Expr::Annotated { name, .. } => Some(name.as_str()),
        _ => None,
    }
}

/// Find the name of the innermost `VarRef` at the given offset.
///
/// Returns `None` if no `VarRef` is found at the offset, or if the offset
/// points to a literal, error node, or other non-reference expression.
fn name_at_offset(expr: &Expr, span: Span, offset: usize) -> Option<String> {
    if !span_contains(span, offset) {
        return None;
    }

    match expr {
        Expr::VarRef { name, .. } => Some(name.clone()),

        Expr::Dict(entries) => {
            for entry in entries {
                if let Some(ref key) = entry.node.key {
                    if let Some(name) = name_at_offset(&key.node, key.span, offset) {
                        return Some(name);
                    }
                }
                if let Some(name) =
                    name_at_offset(&entry.node.value.node, entry.node.value.span, offset)
                {
                    return Some(name);
                }
            }
            None
        }

        Expr::Call {
            func,
            args,
            named_args,
            ..
        } => name_at_offset(&func.node, func.span, offset)
            .or_else(|| {
                args.iter()
                    .find_map(|a| name_at_offset(&a.node, a.span, offset))
            })
            .or_else(|| {
                named_args
                    .iter()
                    .find_map(|na| name_at_offset(&na.node.value.node, na.node.value.span, offset))
            }),

        Expr::Fn { body, .. } => name_at_offset(&body.node, body.span, offset),

        Expr::DotAccess { expr: target, .. } => name_at_offset(&target.node, target.span, offset),

        Expr::Sequential(exprs) => exprs
            .iter()
            .find_map(|seq_expr| name_at_offset(&seq_expr.node, seq_expr.span, offset)),

        Expr::Pipe { lhs, rhs } => name_at_offset(&lhs.node, lhs.span, offset)
            .or_else(|| name_at_offset(&rhs.node, rhs.span, offset)),

        Expr::TypeAlias { body, .. } => name_at_offset(&body.node, body.span, offset),

        Expr::TypeAssert { expr: inner, .. } => name_at_offset(&inner.node, inner.span, offset),

        Expr::Quote(inner) | Expr::Unquote(inner) | Expr::UnquoteSplice(inner) => {
            name_at_offset(&inner.node, inner.span, offset)
        }

        // Literals, Error, Rest, Annotated, Fn params: no VarRef to extract.
        _ => None,
    }
}

/// Find the definition site of a name in the expression tree.
///
/// Searches for the first dict entry whose key matches the given name.
/// Returns the span of the key expression (not the value).
///
/// Depth-first search: first match wins.
fn find_key_definition(expr: &Expr, _span: Span, name: &str) -> Option<Span> {
    match expr {
        Expr::Dict(entries) => {
            for entry in entries {
                if let Some(ref key) = entry.node.key {
                    if key_name(&key.node) == Some(name) {
                        return Some(key.span);
                    }
                }
                // Recurse into the value.
                if let Some(def_span) =
                    find_key_definition(&entry.node.value.node, entry.node.value.span, name)
                {
                    return Some(def_span);
                }
            }
            None
        }

        Expr::Call {
            func,
            args,
            named_args,
            ..
        } => find_key_definition(&func.node, func.span, name)
            .or_else(|| {
                args.iter()
                    .find_map(|a| find_key_definition(&a.node, a.span, name))
            })
            .or_else(|| {
                named_args.iter().find_map(|na| {
                    find_key_definition(&na.node.value.node, na.node.value.span, name)
                })
            }),

        Expr::Fn { body, .. } => find_key_definition(&body.node, body.span, name),

        Expr::DotAccess { expr: target, .. } => {
            find_key_definition(&target.node, target.span, name)
        }

        Expr::Sequential(exprs) => exprs
            .iter()
            .find_map(|seq_expr| find_key_definition(&seq_expr.node, seq_expr.span, name)),

        Expr::Pipe { lhs, rhs } => find_key_definition(&lhs.node, lhs.span, name)
            .or_else(|| find_key_definition(&rhs.node, rhs.span, name)),

        Expr::TypeAlias { body, .. } => find_key_definition(&body.node, body.span, name),

        Expr::TypeAssert { expr: inner, .. } => find_key_definition(&inner.node, inner.span, name),

        Expr::Quote(inner) | Expr::Unquote(inner) | Expr::UnquoteSplice(inner) => {
            find_key_definition(&inner.node, inner.span, name)
        }

        // Literals, VarRef, Error, Rest, Annotated: no definitions here.
        _ => None,
    }
}

/// Find the definition span for the variable at the given offset.
///
/// Returns `Some((url, span))` where:
/// - `url` is the document URI (same as input doc for document-local definitions,
///   the prelude file URI for prelude definitions, or an included file URI for
///   cross-file definitions)
/// - `span` is the definition site span
///
/// Returns `None` if:
/// - The document has a parse error.
/// - No variable reference is found at the offset.
/// - The variable reference has no definition in the document or includes.
///
/// Note: Prelude go-to-definition is not currently supported after the migration
/// to the shared imports module. This could be re-added by extending imports.rs
/// to provide a prelude span map.
pub fn definition_at(
    doc: &DocumentState,
    doc_url: &Uri,
    offset: usize,
    include_graph: &crate::lsp::document::IncludeGraph,
) -> Option<(Uri, Span)> {
    let file = match &doc.ast {
        Ok(f) => f,
        Err(_) => return None,
    };

    // Find the name at the cursor position.
    let name = file.node.documents.iter().find_map(|document| {
        document
            .node
            .expressions
            .iter()
            .find_map(|expr| name_at_offset(&expr.node, expr.span, offset))
    })?;

    // Search for the definition of that name in the document.
    if let Some(span) = file.node.documents.iter().find_map(|document| {
        document
            .node
            .expressions
            .iter()
            .find_map(|expr| find_key_definition(&expr.node, expr.span, &name))
    }) {
        return Some((doc_url.clone(), span));
    }

    // Search direct includes
    if let Some(node) = include_graph.get(doc_url) {
        for include_url in &node.includes {
            if let Some(include_node) = include_graph.get(include_url) {
                if let Ok(ref include_file) = include_node.state.ast {
                    if let Some(span) = include_file.node.documents.iter().find_map(|document| {
                        document
                            .node
                            .expressions
                            .iter()
                            .find_map(|expr| find_key_definition(&expr.node, expr.span, &name))
                    }) {
                        return Some((include_url.clone(), span));
                    }
                }
            }
        }
    }

    // TODO: Add prelude go-to-definition by extending imports.rs to provide
    // a prelude span map if needed.

    None
}

/// Convert document errors to LSP diagnostics.
///
/// `uri` is the document's URI, used to construct `DiagnosticRelatedInformation`
/// locations that point back into the same file.
pub fn diagnostics_for(doc: &DocumentState, uri: &Uri) -> Vec<Diagnostic> {
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

fn eval_error_to_diagnostic(err: &crate::error::EvalError, source: &str, uri: &Uri) -> Diagnostic {
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

    /// Helper: create an empty include graph for tests.
    fn test_include_graph() -> crate::lsp::document::IncludeGraph {
        std::collections::HashMap::new()
    }

    /// Canonical test URI for diagnostics_for() calls.
    fn test_uri() -> Uri {
        "file:///test.llt".parse::<Uri>().unwrap()
    }

    #[test]
    fn test_hover_int_literal() {
        let env = test_env();
        let doc = DocumentState::new("42".to_string(), &env, &test_ctx(), None);
        let hover = hover_at(&doc, &test_uri(), 0, &test_include_graph());
        assert_eq!(hover.as_deref(), Some("Int literal: 42 (42)"));
    }

    #[test]
    fn test_hover_var_ref() {
        let env = test_env();
        // $x is undefined, so no type is inferred -- just syntactic info.
        let doc = DocumentState::new("$x".to_string(), &env, &test_ctx(), None);
        let hover = hover_at(&doc, &test_uri(), 1, &test_include_graph());
        assert!(hover.is_some());
        assert!(hover.unwrap().contains("Variable: $x"));
    }

    #[test]
    fn test_hover_var_ref_with_type() {
        let env = test_env();
        // $x is defined in scope, so hover should show its type.
        let doc = DocumentState::new("[x: 42]\n[y: $x]".to_string(), &env, &test_ctx(), None);
        // Offset 12 is inside "$x" in the second expression "[y: $x]"
        // "[x: 42]\n[y: $x]"
        //  0123456 7 89012345
        let hover = hover_at(&doc, &test_uri(), 12, &test_include_graph());
        assert!(hover.is_some());
        let text = hover.unwrap();
        assert!(text.contains("Variable: $x"), "got: {text}");
        assert!(text.contains("(42)"), "should show type, got: {text}");
    }

    #[test]
    fn test_hover_string_literal() {
        let env = test_env();
        let doc = DocumentState::new("\"hello\"".to_string(), &env, &test_ctx(), None);
        let hover = hover_at(&doc, &test_uri(), 2, &test_include_graph());
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
        let doc = DocumentState::new("[x: 1]".to_string(), &env, &test_ctx(), None);
        // Hover on whitespace between entries.
        let hover = hover_at(&doc, &test_uri(), 100, &test_include_graph());
        assert!(hover.is_none());
    }

    #[test]
    fn test_diagnostics_parse_error() {
        let env = test_env();
        let doc = DocumentState::new("[unterminated".to_string(), &env, &test_ctx(), None);
        let diags = diagnostics_for(&doc, &test_uri());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diags[0].source, Some("tinct-parser".to_string()));
    }

    #[test]
    fn test_diagnostics_type_error() {
        let env = test_env();
        let doc = DocumentState::new("[@Number hello]".to_string(), &env, &test_ctx(), None);
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
        let doc = DocumentState::new("$undefined".to_string(), &env, &test_ctx(), None);
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
        let doc = DocumentState::new("[x: 42]".to_string(), &env, &test_ctx(), None);
        let diags = diagnostics_for(&doc, &test_uri());
        assert!(diags.is_empty());
    }

    #[test]
    fn test_hover_dict_entry_key() {
        let env = test_env();
        let doc = DocumentState::new("[x: 42]".to_string(), &env, &test_ctx(), None);
        let hover = hover_at(&doc, &test_uri(), 1, &test_include_graph()); // on 'x'
        assert!(hover.is_some());
    }

    #[test]
    fn test_hover_dict_entry_value() {
        let env = test_env();
        let doc = DocumentState::new("[x: 42]".to_string(), &env, &test_ctx(), None);
        let hover = hover_at(&doc, &test_uri(), 4, &test_include_graph()); // on '42'
        assert!(hover.is_some());
        let text = hover.unwrap();
        assert!(text.contains("Int literal"), "got: {text}");
        assert!(text.contains("(42)"), "should show type, got: {text}");
    }

    #[test]
    fn test_hover_nested_dict() {
        let env = test_env();
        let doc = DocumentState::new("[a: [b: 1]]".to_string(), &env, &test_ctx(), None);
        let hover = hover_at(&doc, &test_uri(), 8, &test_include_graph()); // on '1'
        assert!(hover.is_some());
        let text = hover.unwrap();
        assert!(text.contains("Int literal"), "got: {text}");
    }

    #[test]
    fn test_hover_function_param() {
        let env = test_env();
        let doc = DocumentState::new("[fn [x] $x]".to_string(), &env, &test_ctx(), None);
        let hover = hover_at(&doc, &test_uri(), 5, &test_include_graph()); // on 'x' in param list
        assert!(hover.is_some());
        assert!(hover.unwrap().contains("Parameter"));
    }

    #[test]
    fn test_hover_call_expression() {
        let env = test_env();
        let doc = DocumentState::new("[call $+ 1 2]".to_string(), &env, &test_ctx(), None);
        let hover = hover_at(&doc, &test_uri(), 6, &test_include_graph()); // on '$+'
        assert!(hover.is_some());
        assert!(hover.unwrap().contains("Variable: $+"));
    }

    #[test]
    fn test_hover_float_literal() {
        let env = test_env();
        let doc = DocumentState::new("3.14".to_string(), &env, &test_ctx(), None);
        let hover = hover_at(&doc, &test_uri(), 0, &test_include_graph());
        assert_eq!(hover.as_deref(), Some("Float literal: 3.14 (Float)"));
    }

    #[test]
    fn test_hover_bool_literal() {
        let env = test_env();
        let doc = DocumentState::new("true".to_string(), &env, &test_ctx(), None);
        let hover = hover_at(&doc, &test_uri(), 0, &test_include_graph());
        assert_eq!(hover.as_deref(), Some("Bool literal: true (Bool)"));
    }

    #[test]
    fn test_hover_type_not_shown_on_error() {
        let env = test_env();
        // $undefined has type <error> when inference fails -- LSP hover shows the sentinel
        // so users can see that the expression has a type error rather than seeing Any.
        let doc = DocumentState::new("$undefined".to_string(), &env, &test_ctx(), None);
        let hover = hover_at(&doc, &test_uri(), 1, &test_include_graph());
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

    // --- Go To Definition tests ---

    #[test]
    fn test_definition_at_simple() {
        let env = test_env();
        let doc = DocumentState::new("[x: 42  y: $x]".to_string(), &env, &test_ctx(), None);
        let uri = test_uri();
        // Offset 12 is on '$x' in the second entry.
        // "[x: 42  y: $x]"
        //  0123456789012345
        let def_result = definition_at(&doc, &uri, 12, &test_include_graph());
        assert!(def_result.is_some(), "should find definition of $x");
        let (_url, span) = def_result.unwrap();
        // Key "x" is at offset 1, one character long.
        assert_eq!(span.start.offset, 1);
        assert_eq!(span.end.offset, 2);
    }

    #[test]
    fn test_definition_at_mutually_recursive() {
        let env = test_env();
        let doc = DocumentState::new("[a: $b  b: $a]".to_string(), &env, &test_ctx(), None);
        let uri = test_uri();
        // Offset 5 is on '$b' in the first entry.
        // "[a: $b  b: $a]"
        //  01234567890123
        let def_result = definition_at(&doc, &uri, 5, &test_include_graph());
        assert!(def_result.is_some(), "should find definition of $b");
        let (_url, span) = def_result.unwrap();
        // Key "b" is at offset 8, one character long.
        assert_eq!(span.start.offset, 8);
        assert_eq!(span.end.offset, 9);
    }

    #[test]
    fn test_definition_at_annotated_key() {
        let env = test_env();
        let doc = DocumentState::new("[x@Int: 1  y: $x]".to_string(), &env, &test_ctx(), None);
        let uri = test_uri();
        // Offset 15 is on '$x' in the second entry.
        // "[x@Int: 1  y: $x]"
        //  01234567890123456
        let def_result = definition_at(&doc, &uri, 15, &test_include_graph());
        assert!(def_result.is_some(), "should find definition of $x");
        let (_url, span) = def_result.unwrap();
        // Key "x@Int" starts at offset 1, ends at offset 6 (the annotated key).
        assert_eq!(span.start.offset, 1);
        assert_eq!(span.end.offset, 6);
    }

    #[test]
    fn test_definition_at_no_match() {
        let env = test_env();
        let doc = DocumentState::new("$undefined".to_string(), &env, &test_ctx(), None);
        let uri = test_uri();
        // Offset 1 is on '$undefined', which has no definition in the document.
        let def_result = definition_at(&doc, &uri, 1, &test_include_graph());
        assert!(
            def_result.is_none(),
            "should return None for undefined variable"
        );
    }

    #[test]
    fn test_definition_at_nested_dict() {
        let env = test_env();
        let doc = DocumentState::new(
            "[outer: [inner: 42]  use: $inner]".to_string(),
            &env,
            &test_ctx(),
            None,
        );
        let uri = test_uri();
        // Offset 30 is on '$inner' in the second entry.
        // "[outer: [inner: 42]  use: $inner]"
        //  0123456789012345678901234567890123
        let def_result = definition_at(&doc, &uri, 30, &test_include_graph());
        assert!(def_result.is_some(), "should find definition of $inner");
        let (_url, span) = def_result.unwrap();
        // Key "inner" is at offset 9, 5 characters long.
        assert_eq!(span.start.offset, 9);
        assert_eq!(span.end.offset, 14);
    }

    #[test]
    fn test_definition_at_parse_error() {
        let env = test_env();
        let doc = DocumentState::new("[unterminated".to_string(), &env, &test_ctx(), None);
        let uri = test_uri();
        let def_result = definition_at(&doc, &uri, 1, &test_include_graph());
        assert!(def_result.is_none(), "should return None when parse fails");
    }

    #[test]
    fn test_hover_prelude_name() {
        // After migration to shared imports module, prelude types are seeded via
        // imports::build_type_env(). This test verifies that hover still works
        // for prelude functions via the document's type_map.
        let env = test_env();
        let ctx = test_ctx();
        let doc = DocumentState::new(
            "[call $map [fn [x] x] [1 2 3]]".to_string(),
            &env,
            &ctx,
            None, // base_dir=None still seeds prelude types via imports::build_type_env
        );
        // Offset 6 is on '$map'
        // "[call $map [fn [x] x] [1 2 3]]"
        //  0123456789...
        let hover = hover_at(&doc, &test_uri(), 6, &test_include_graph());
        assert!(hover.is_some(), "should find hover for $map");
        let text = hover.unwrap();
        assert!(
            !text.contains("<error>"),
            "hover should not show <error> for prelude name; got: {text}"
        );
        assert!(
            text.contains("(") && text.contains(")"),
            "hover should show type signature for prelude name; got: {text}"
        );
    }

    // Note: test_definition_at_prelude_name was removed during migration to shared
    // imports module. Prelude go-to-definition is not currently supported but could
    // be re-added by extending imports.rs to provide a prelude span map.

    #[test]
    fn test_hover_shows_doc() {
        let env = test_env();
        let source = r#"[fn [x@[type: String doc: "the name"]] $x]"#;
        let doc = DocumentState::new(source.to_string(), &env, &test_ctx(), None);

        // Hover on "$x" in the function body (starts at offset 39)
        // "[fn [x@[type: String doc: "the name"]] $x]"
        //  0         1         2         3         4
        //  0123456789012345678901234567890123456789012
        //                                         ^-- $ at 39, x at 40, ] at 41
        let hover = hover_at(&doc, &test_uri(), 39, &test_include_graph());
        assert!(hover.is_some(), "hover should be present");
        let text = hover.unwrap();
        assert!(
            text.contains("Variable: $x"),
            "should show variable name, got: {text}"
        );
        assert!(
            text.contains("the name"),
            "should show doc string, got: {text}"
        );
    }

    #[test]
    fn test_hover_no_doc() {
        let env = test_env();
        let source = r#"[fn [x@String] $x]"#;
        let doc = DocumentState::new(source.to_string(), &env, &test_ctx(), None);

        // Hover on "$x" in the function body (starts at offset 15)
        // "[fn [x@String] $x]"
        //  012345678901234567
        let hover = hover_at(&doc, &test_uri(), 15, &test_include_graph());
        assert!(hover.is_some(), "hover should be present");
        let text = hover.unwrap();
        assert!(
            text.contains("Variable: $x"),
            "should show variable name, got: {text}"
        );
        assert!(
            !text.contains("\n\n"),
            "should not have doc separator when no doc, got: {text}"
        );
    }

    #[test]
    fn test_hover_doc_and_default() {
        let env = test_env();
        let source = r#"[fn [x@[type: Number default: 0 doc: "count"]] $x]"#;
        let doc = DocumentState::new(source.to_string(), &env, &test_ctx(), None);

        // Hover on "$x" in the function body (starts at offset 48)
        // "[fn [x@[type: Number default: 0 doc: "count"]] $x]"
        //  0         1         2         3         4
        //  012345678901234567890123456789012345678901234567890
        let hover = hover_at(&doc, &test_uri(), 48, &test_include_graph());
        assert!(hover.is_some(), "hover should be present");
        let text = hover.unwrap();
        assert!(
            text.contains("Variable: $x"),
            "should show variable name, got: {text}"
        );
        assert!(
            text.contains("count"),
            "should show doc string, got: {text}"
        );
        // The type inference should work regardless of default value presence
    }

    #[test]
    fn test_hover_param_with_doc() {
        let env = test_env();
        let source = r#"[fn [x@[type: String doc: "the name"]] $x]"#;
        let doc = DocumentState::new(source.to_string(), &env, &test_ctx(), None);

        // Hover on parameter "x" itself (around offset 5)
        // "[fn [x@[type: String doc: "the name"]] $x]"
        //  012345
        let hover = hover_at(&doc, &test_uri(), 5, &test_include_graph());
        assert!(hover.is_some(), "hover should be present on param");
        let text = hover.unwrap();
        assert!(
            text.contains("Parameter: x"),
            "should show parameter label, got: {text}"
        );
        assert!(
            text.contains("the name"),
            "should show doc string on param hover, got: {text}"
        );
    }

    #[test]
    fn test_hover_function_param_names_in_type() {
        // Task 1: parameter names should appear in function type display.
        // A typed function [fn [x@Int y@Int] ...] stored in a dict key $f
        // should show "x: Int y: Int" in the hover type.
        let env = test_env();
        // Use two-document pipeline: define f, then reference it.
        // "[f: [fn [x@Int y@Int] 0]]" = 26 chars (0..25), \n at 26
        // "[call $f 1 2]"  starts at 27
        //  "$f" is at offset 33 ('$') and 34 ('f')
        let source = "[f: [fn [x@Int y@Int] 0]]\n[call $f 1 2]";
        let doc = DocumentState::new(source.to_string(), &env, &test_ctx(), None);
        // "[f: [fn [x@Int y@Int] 0]]\n[call $f 1 2]"
        //  0         1         2         3
        //  0123456789012345678901234567890123456789
        //                                   ^ 33 = '$f'
        let hover = hover_at(&doc, &test_uri(), 33, &test_include_graph());
        assert!(hover.is_some(), "should have hover on $f");
        let text = hover.unwrap();
        assert!(
            text.contains("Variable: $f"),
            "should show variable name, got: {text}"
        );
        // The type should contain parameter names x and y
        assert!(
            text.contains("x:") && text.contains("y:"),
            "hover should show parameter names in function type, got: {text}"
        );
    }

    #[test]
    fn test_hover_builtin_shows_constraint() {
        // Task 2: hover on a constrained builtin should show the constraint.
        // `builtin-eq` has scheme `Equatable a => Fn@Bool [a a]` and is NOT redefined
        // by the prelude (unlike `=` which the prelude wraps as a concrete function).
        //
        // "[call $builtin-eq 1 2]"
        //  0123456789...
        //       ^ 6 = '$' of '$builtin-eq'
        let env = test_env();
        let source = "[call $builtin-eq 1 2]";
        let doc = DocumentState::new(source.to_string(), &env, &test_ctx(), None);
        // Offset 6 is on '$builtin-eq'
        let hover = hover_at(&doc, &test_uri(), 6, &test_include_graph());
        assert!(hover.is_some(), "should have hover on $builtin-eq");
        let text = hover.unwrap();
        // Should show the "Equatable" constraint in the type display
        assert!(
            text.contains("Equatable"),
            "hover should show Equatable constraint for $builtin-eq, got: {text}"
        );
        assert!(
            text.contains("Bool"),
            "hover should show Bool return type for $builtin-eq, got: {text}"
        );
    }
}

/// Generate completion items for the given byte offset in the document.
///
/// Returns a list of completion items including:
/// - Dict entry keys visible at the cursor position (from ALL containing scopes)
/// - Builtin function names from `standard_builtins()`
/// - Prelude function names from the stdlib environment
///
/// # Implementation notes
///
/// - Dict keys are extracted from all enclosing dict scopes, not just the innermost one
/// - Builtins are cached in a static/lazy list to avoid recomputing on every request
/// - Prelude names are extracted from the shared prelude environment
pub fn completion_at(
    doc: &DocumentState,
    _uri: &Uri,
    offset: usize,
) -> Vec<lsp_types::CompletionItem> {
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    let mut items = Vec::new();

    // Add dict entry keys from all visible scopes
    if let Ok(ref file) = doc.ast {
        for document in &file.node.documents {
            for expr in &document.node.expressions {
                collect_dict_keys_in_scope(&expr.node, expr.span, offset, &mut items, &mut seen);
            }
        }
    }

    // Add builtin function names
    for item in builtin_completions() {
        if seen.insert(item.label.clone()) {
            items.push(item.clone());
        }
    }

    // Add prelude function names (cached globally)
    for item in prelude_completions() {
        if seen.insert(item.label.clone()) {
            items.push(item.clone());
        }
    }

    items
}

/// Collect dict entry keys that are visible at the given offset.
///
/// Walks the expression tree and extracts string literal keys from all
/// dict scopes that contain the cursor position.
fn collect_dict_keys_in_scope(
    expr: &Expr,
    span: Span,
    offset: usize,
    items: &mut Vec<lsp_types::CompletionItem>,
    seen: &mut std::collections::HashSet<String>,
) {
    use lsp_types::{CompletionItem, CompletionItemKind};

    if !span_contains(span, offset) {
        return;
    }

    match expr {
        Expr::Dict(entries) => {
            // Add all keys from this dict
            for entry in entries {
                if let Some(ref key) = entry.node.key {
                    if let Some(name) = key_name(&key.node) {
                        if seen.insert(name.to_string()) {
                            items.push(CompletionItem {
                                label: name.to_string(),
                                kind: Some(CompletionItemKind::VARIABLE),
                                ..Default::default()
                            });
                        }
                    }
                }
                // Recurse into nested dicts in the value
                collect_dict_keys_in_scope(
                    &entry.node.value.node,
                    entry.node.value.span,
                    offset,
                    items,
                    seen,
                );
            }
        }
        Expr::Call {
            func,
            args,
            named_args,
            ..
        } => {
            collect_dict_keys_in_scope(&func.node, func.span, offset, items, seen);
            for arg in args {
                collect_dict_keys_in_scope(&arg.node, arg.span, offset, items, seen);
            }
            for na in named_args {
                collect_dict_keys_in_scope(
                    &na.node.value.node,
                    na.node.value.span,
                    offset,
                    items,
                    seen,
                );
            }
        }
        Expr::Fn { body, .. } => {
            collect_dict_keys_in_scope(&body.node, body.span, offset, items, seen);
        }
        Expr::DotAccess { expr: target, .. } => {
            collect_dict_keys_in_scope(&target.node, target.span, offset, items, seen);
        }
        Expr::Sequential(exprs) => {
            for seq_expr in exprs {
                collect_dict_keys_in_scope(&seq_expr.node, seq_expr.span, offset, items, seen);
            }
        }
        Expr::Pipe { lhs, rhs } => {
            collect_dict_keys_in_scope(&lhs.node, lhs.span, offset, items, seen);
            collect_dict_keys_in_scope(&rhs.node, rhs.span, offset, items, seen);
        }
        Expr::TypeAlias { body, .. } => {
            collect_dict_keys_in_scope(&body.node, body.span, offset, items, seen);
        }
        Expr::TypeAssert { expr: inner, .. } => {
            collect_dict_keys_in_scope(&inner.node, inner.span, offset, items, seen);
        }
        Expr::Quote(inner) | Expr::Unquote(inner) | Expr::UnquoteSplice(inner) => {
            collect_dict_keys_in_scope(&inner.node, inner.span, offset, items, seen);
        }
        Expr::Match { scrutinee, arms } => {
            collect_dict_keys_in_scope(&scrutinee.node, scrutinee.span, offset, items, seen);
            for arm in arms {
                collect_dict_keys_in_scope(&arm.body.node, arm.body.span, offset, items, seen);
            }
        }
        Expr::ClassDecl { methods, .. } => {
            for method in methods {
                if let Some(key) = &method.node.key {
                    collect_dict_keys_in_scope(&key.node, key.span, offset, items, seen);
                }
                collect_dict_keys_in_scope(
                    &method.node.value.node,
                    method.node.value.span,
                    offset,
                    items,
                    seen,
                );
            }
        }
        Expr::InstanceDecl {
            instance_type,
            methods,
            ..
        } => {
            collect_dict_keys_in_scope(
                &instance_type.node,
                instance_type.span,
                offset,
                items,
                seen,
            );
            for method in methods {
                if let Some(key) = &method.node.key {
                    collect_dict_keys_in_scope(&key.node, key.span, offset, items, seen);
                }
                collect_dict_keys_in_scope(
                    &method.node.value.node,
                    method.node.value.span,
                    offset,
                    items,
                    seen,
                );
            }
        }
        Expr::DefMacro { transformer, .. } => {
            collect_dict_keys_in_scope(&transformer.node, transformer.span, offset, items, seen);
        }
        _ => {}
    }
}

/// Return a static reference to builtin function completions.
///
/// Uses a lazy static to avoid recomputing the list on every completion request.
fn builtin_completions() -> &'static [lsp_types::CompletionItem] {
    use lsp_types::{CompletionItem, CompletionItemKind};
    use std::sync::OnceLock;

    static BUILTIN_ITEMS: OnceLock<Vec<CompletionItem>> = OnceLock::new();

    BUILTIN_ITEMS.get_or_init(|| {
        crate::builtins::standard_builtins()
            .iter()
            .map(|def| CompletionItem {
                label: def.name.to_string(),
                kind: Some(CompletionItemKind::FUNCTION),
                ..Default::default()
            })
            .collect()
    })
}

/// Return a static reference to prelude function completions.
///
/// Uses a lazy static to avoid recomputing the list on every completion request.
/// Extracts all dict entry names from the prelude source by parsing it.
fn prelude_completions() -> &'static [lsp_types::CompletionItem] {
    use lsp_types::CompletionItem;
    use std::collections::HashSet;
    use std::sync::OnceLock;

    static PRELUDE_ITEMS: OnceLock<Vec<CompletionItem>> = OnceLock::new();

    PRELUDE_ITEMS.get_or_init(|| {
        let prelude_source = include_str!("../../stdlib/prelude.llt");
        let mut items = Vec::new();
        let mut seen = HashSet::new();

        // Build builtin set once for O(1) lookups
        let builtin_names: HashSet<&str> = crate::builtins::standard_builtins()
            .iter()
            .map(|def| def.name)
            .collect();

        // Parse the prelude source and extract all dict entry names
        if let Ok(file) = crate::parser::parse(prelude_source) {
            for document in &file.node.documents {
                for expr in &document.node.expressions {
                    extract_names_from_expr(&expr.node, &mut items, &mut seen, &builtin_names);
                }
            }
        }

        items
    })
}

/// Extract completion items from an expression tree (for prelude names).
fn extract_names_from_expr(
    expr: &Expr,
    items: &mut Vec<lsp_types::CompletionItem>,
    seen: &mut std::collections::HashSet<String>,
    builtin_names: &std::collections::HashSet<&str>,
) {
    use lsp_types::{CompletionItem, CompletionItemKind};

    match expr {
        Expr::Dict(entries) => {
            for entry in entries {
                if let Some(ref key) = entry.node.key {
                    if let Some(name) = key_name(&key.node) {
                        // Skip if already seen or is a builtin
                        if builtin_names.contains(name) {
                            continue;
                        }
                        if seen.insert(name.to_string()) {
                            items.push(CompletionItem {
                                label: name.to_string(),
                                kind: Some(CompletionItemKind::FUNCTION),
                                ..Default::default()
                            });
                        }
                    }
                }
            }
        }
        Expr::Sequential(exprs) => {
            for seq_expr in exprs {
                extract_names_from_expr(&seq_expr.node, items, seen, builtin_names);
            }
        }
        _ => {}
    }
}
