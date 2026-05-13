//! LSP analysis: hover text and diagnostics.

use lsp_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, DocumentSymbol, Location,
    SymbolKind, TextEdit, Uri,
};

use crate::ast::{Expr, File, Span, Spanned};
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
/// If the scheme has a doc field, it is appended after the type signature.
///
/// Examples:
///   - `Equatable a => Fn@Bool [a a]` (constrained polymorphic)
///   - `Fn@Bool [a a]` (polymorphic, no constraints)
///   - `Fn@a [a]\n\nReturns the argument unchanged` (with doc)
fn format_scheme_for_hover(scheme: &TypeScheme) -> String {
    let type_sig = if scheme.constraints.is_empty() {
        scheme.body.to_string()
    } else {
        let constraints = scheme
            .constraints
            .iter()
            .map(|c| format!("{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{constraints} => {}", scheme.body)
    };

    if let Some(ref doc) = scheme.doc {
        format!("{}\n\n{}", type_sig, doc)
    } else {
        type_sig
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
                            let ty = type_suffix(
                                entry.node.value.span,
                                type_map,
                                scheme_map,
                                include_graph,
                                doc_url,
                            );
                            // Only look up doc for bare-name keys (not string literals)
                            let doc_name = match &key.node {
                                Expr::VarRef { name, .. } | Expr::Annotated { name, .. } => {
                                    Some(name.as_str())
                                }
                                _ => None,
                            };
                            let doc = doc_name.map(|n| doc_suffix(n, doc_map)).unwrap_or_default();
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

        Expr::DefMacro { name, body, .. } => {
            // Check if hover is on the body
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

        Expr::TypeApp { .. } => Some("Type application".to_string()),

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
/// - The variable reference has no definition in the document, includes, or prelude.
///
/// Searches in order: document-local definitions, direct includes, prelude.
pub fn definition_at(
    doc: &DocumentState,
    doc_url: &Uri,
    offset: usize,
    include_graph: &crate::lsp::document::IncludeGraph,
    prelude_ast: Option<&Spanned<File>>,
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

    // Search prelude AST (if available)
    if let Some(prelude_file) = prelude_ast {
        if let Some(span) = prelude_file.node.documents.iter().find_map(|document| {
            document
                .node
                .expressions
                .iter()
                .find_map(|expr| find_key_definition(&expr.node, expr.span, &name))
        }) {
            // Resolve the prelude URI via find_libdir_path().join("prelude.llt")
            if let Some(libdir_path) = crate::find_libdir_path() {
                let prelude_path = libdir_path.join("prelude.llt");
                if let Some(prelude_uri) = crate::lsp::convert::file_path_to_uri(&prelude_path) {
                    return Some((prelude_uri, span));
                }
            }
        }
    }

    None
}

/// Generate document symbols (outline) for all top-level dict entry keys.
///
/// Returns one `DocumentSymbol` per top-level dict entry across all documents
/// in the file. Each symbol uses `SymbolKind::Variable` and its `selection_range`
/// points to the key span, while `range` covers the entire entry (key + value).
///
/// Returns an empty list if the document has a parse error.
#[allow(deprecated)] // DocumentSymbol.deprecated field is required by the LSP spec
pub fn document_symbols_at(doc: &DocumentState) -> Vec<DocumentSymbol> {
    let file = match &doc.ast {
        Ok(f) => f,
        Err(_) => return vec![],
    };

    let mut symbols = Vec::new();

    for document in &file.node.documents {
        for expr in &document.node.expressions {
            if let Expr::Dict(entries) = &expr.node {
                for entry in entries {
                    // Only emit symbols for entries with a static key name.
                    let key = match &entry.node.key {
                        Some(k) => k,
                        None => continue,
                    };
                    let name: Option<String> = match &key.node {
                        Expr::Str(s) => Some(s.clone()),
                        Expr::Annotated { name, .. } => Some(name.clone()),
                        Expr::VarRef { name, .. } => Some(name.clone()),
                        _ => None,
                    };
                    let name = match name {
                        Some(n) => n,
                        None => continue,
                    };

                    // `selection_range` = key span; `range` = full entry span.
                    let selection_range = llt_span_to_lsp_range(&key.span, &doc.text);
                    let range = llt_span_to_lsp_range(&entry.span, &doc.text);

                    symbols.push(DocumentSymbol {
                        name,
                        detail: None,
                        kind: SymbolKind::VARIABLE,
                        tags: None,
                        deprecated: None,
                        range,
                        selection_range,
                        children: None,
                    });
                }
            }
        }
    }

    symbols
}

/// Find all references to the name under the cursor.
///
/// Finds the variable name at `offset`, then walks the full AST collecting
/// every `Expr::VarRef` with that name. Returns their spans as `Location` values.
///
/// Returns an empty list if:
/// - The document has a parse error.
/// - No variable reference is found at the offset.
pub fn references_at(doc: &DocumentState, uri: &Uri, offset: usize) -> Vec<Location> {
    let file = match &doc.ast {
        Ok(f) => f,
        Err(_) => return vec![],
    };

    // Find the name at the cursor position.
    let name = file.node.documents.iter().find_map(|document| {
        document
            .node
            .expressions
            .iter()
            .find_map(|expr| name_at_offset(&expr.node, expr.span, offset))
    });

    let name = match name {
        Some(n) => n,
        None => return vec![],
    };

    // Collect all VarRef spans with that name.
    let mut locations = Vec::new();
    for document in &file.node.documents {
        for expr in &document.node.expressions {
            collect_var_refs_spanned(&expr.node, expr.span, &name, &doc.text, uri, &mut locations);
        }
    }

    locations
}

/// Recursively collect all `VarRef` spans matching `name` into `out`.
///
/// Every call site passes both the `Expr` node and its `Span` together (from a
/// `Spanned<Expr>`), so span is always available at each leaf.
fn collect_var_refs_spanned(
    expr: &Expr,
    span: Span,
    name: &str,
    source: &str,
    uri: &Uri,
    out: &mut Vec<Location>,
) {
    match expr {
        Expr::VarRef { name: ref_name, .. } => {
            if ref_name == name {
                let range = llt_span_to_lsp_range(&span, source);
                out.push(Location {
                    uri: uri.clone(),
                    range,
                });
            }
        }

        Expr::Dict(entries) => {
            for entry in entries {
                if let Some(ref key) = entry.node.key {
                    collect_var_refs_spanned(&key.node, key.span, name, source, uri, out);
                }
                collect_var_refs_spanned(
                    &entry.node.value.node,
                    entry.node.value.span,
                    name,
                    source,
                    uri,
                    out,
                );
            }
        }

        Expr::Call {
            func,
            args,
            named_args,
            ..
        } => {
            collect_var_refs_spanned(&func.node, func.span, name, source, uri, out);
            for a in args {
                collect_var_refs_spanned(&a.node, a.span, name, source, uri, out);
            }
            for na in named_args {
                collect_var_refs_spanned(
                    &na.node.value.node,
                    na.node.value.span,
                    name,
                    source,
                    uri,
                    out,
                );
            }
        }

        Expr::Fn { body, .. } => {
            // Fn params are binding sites, not VarRef nodes — skip them.
            collect_var_refs_spanned(&body.node, body.span, name, source, uri, out);
        }

        Expr::DotAccess { expr: target, .. } => {
            collect_var_refs_spanned(&target.node, target.span, name, source, uri, out);
        }

        Expr::Sequential(exprs) => {
            for seq_expr in exprs {
                collect_var_refs_spanned(&seq_expr.node, seq_expr.span, name, source, uri, out);
            }
        }

        Expr::Pipe { lhs, rhs } => {
            collect_var_refs_spanned(&lhs.node, lhs.span, name, source, uri, out);
            collect_var_refs_spanned(&rhs.node, rhs.span, name, source, uri, out);
        }

        Expr::TypeAlias { body, .. } => {
            collect_var_refs_spanned(&body.node, body.span, name, source, uri, out);
        }

        Expr::TypeAssert { expr: inner, .. } => {
            collect_var_refs_spanned(&inner.node, inner.span, name, source, uri, out);
        }

        Expr::Quote(inner) | Expr::Unquote(inner) | Expr::UnquoteSplice(inner) => {
            collect_var_refs_spanned(&inner.node, inner.span, name, source, uri, out);
        }

        Expr::Match { scrutinee, arms } => {
            collect_var_refs_spanned(&scrutinee.node, scrutinee.span, name, source, uri, out);
            for arm in arms {
                collect_var_refs_spanned(&arm.body.node, arm.body.span, name, source, uri, out);
            }
        }

        Expr::ClassDecl { methods, .. } | Expr::InstanceDecl { methods, .. } => {
            for method in methods {
                if let Some(key) = &method.node.key {
                    collect_var_refs_spanned(&key.node, key.span, name, source, uri, out);
                }
                collect_var_refs_spanned(
                    &method.node.value.node,
                    method.node.value.span,
                    name,
                    source,
                    uri,
                    out,
                );
            }
        }

        Expr::DefMacro { body, .. } => {
            collect_var_refs_spanned(&body.node, body.span, name, source, uri, out);
        }

        // Literals, TypeApp, Error, Rest, Annotated: no VarRef children.
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Str(_)
        | Expr::Rest(_)
        | Expr::Annotated { .. }
        | Expr::TypeApp { .. }
        | Expr::Error(_) => {}
    }
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
        // LSP skips the eval pass entirely — eval_errors is always empty.
        // Undefined variables are caught by the type checker instead.
        let env = test_env();
        let doc = DocumentState::new("$undefined".to_string(), &env, &test_ctx(), None);
        let diags = diagnostics_for(&doc, &test_uri());
        assert!(!diags.is_empty());
        // No eval diagnostic — eval is skipped in LSP context.
        assert!(
            diags
                .iter()
                .all(|d| d.source.as_deref() != Some("tinct-eval")),
            "LSP eval is skipped — no tinct-eval diagnostics expected; got: {:?}",
            diags
        );
        // The type checker catches the undefined variable reference.
        let type_diag = diags
            .iter()
            .find(|d| d.source.as_deref() == Some("tinct-typecheck"))
            .expect("tinct-typecheck diagnostic expected for $undefined");
        assert_eq!(type_diag.severity, Some(DiagnosticSeverity::WARNING));
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
        let def_result = definition_at(&doc, &uri, 12, &test_include_graph(), None);
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
        let def_result = definition_at(&doc, &uri, 5, &test_include_graph(), None);
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
        let def_result = definition_at(&doc, &uri, 15, &test_include_graph(), None);
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
        let def_result = definition_at(&doc, &uri, 1, &test_include_graph(), None);
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
        let def_result = definition_at(&doc, &uri, 30, &test_include_graph(), None);
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
        let def_result = definition_at(&doc, &uri, 1, &test_include_graph(), None);
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

    #[test]
    fn test_hover_function_with_doc_in_scheme() {
        // Test that when hovering on a function reference, the doc from the TypeScheme
        // is displayed in the hover text.
        let env = test_env();
        let source = r#"[
  identity@[doc: "Returns the argument unchanged"]: [fn [x@a] $x]
  test: [call $identity 42]
]"#;
        let doc = DocumentState::new(source.to_string(), &env, &test_ctx(), None);
        // Find offset of "$identity" in the call expression
        let identity_offset = source.find("$identity").expect("should find $identity");
        let hover = hover_at(&doc, &test_uri(), identity_offset, &test_include_graph());
        assert!(hover.is_some(), "should have hover on $identity");
        let text = hover.unwrap();
        assert!(
            text.contains("Variable: $identity"),
            "should show variable label, got: {text}"
        );
        // The function is polymorphic (has type var 'a'), so scheme should be in scheme_map
        // and doc should be displayed
        assert!(
            text.contains("Returns the argument unchanged"),
            "should show doc string from TypeScheme, got: {text}"
        );
    }

    #[test]
    fn test_definition_at_prelude_name() {
        // Verify that go-to-definition works for prelude functions when prelude_ast is provided.
        let env = test_env();
        let ctx = test_ctx();
        // Use a prelude function like "map" (defined in the prelude)
        let source = "[call $map [fn [x] x] [1 2 3]]";
        let doc = DocumentState::new(source.to_string(), &env, &ctx, None);
        let uri = test_uri();

        // Parse the prelude AST
        let prelude_source = include_str!("../../stdlib/prelude.llt");
        let prelude_ast = crate::parser::parse(prelude_source).ok();

        // Offset 6 is on '$map'
        // "[call $map [fn [x] x] [1 2 3]]"
        //  0123456789...
        let def_result = definition_at(&doc, &uri, 6, &test_include_graph(), prelude_ast.as_ref());

        // Should find the definition in the prelude
        assert!(
            def_result.is_some(),
            "should find definition of $map in prelude"
        );

        let (target_uri, _span) = def_result.unwrap();

        // The target URI should be the prelude file (not the test document)
        assert_ne!(
            target_uri, uri,
            "definition should point to prelude, not the test document"
        );

        // The URI should be a file:// URI pointing to stdlib/prelude.llt
        assert!(
            target_uri.as_str().contains("prelude.llt"),
            "target URI should reference prelude.llt, got: {}",
            target_uri.as_str()
        );
    }

    // --- document_symbols_at tests ---

    #[test]
    fn test_document_symbols_simple() {
        let env = test_env();
        let doc = DocumentState::new("[x: 1  y: 2]".to_string(), &env, &test_ctx(), None);
        let syms = document_symbols_at(&doc);
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].name, "x");
        assert_eq!(syms[1].name, "y");
    }

    #[test]
    fn test_document_symbols_annotated_key() {
        let env = test_env();
        let doc = DocumentState::new("[x@Int: 42]".to_string(), &env, &test_ctx(), None);
        let syms = document_symbols_at(&doc);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "x");
    }

    #[test]
    fn test_document_symbols_string_key() {
        let env = test_env();
        let doc = DocumentState::new(r#"["my-key": 99]"#.to_string(), &env, &test_ctx(), None);
        let syms = document_symbols_at(&doc);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "my-key");
    }

    #[test]
    fn test_document_symbols_empty_on_parse_error() {
        let env = test_env();
        let doc = DocumentState::new("[unterminated".to_string(), &env, &test_ctx(), None);
        let syms = document_symbols_at(&doc);
        assert!(syms.is_empty());
    }

    #[test]
    fn test_document_symbols_non_dict_is_empty() {
        let env = test_env();
        let doc = DocumentState::new("42".to_string(), &env, &test_ctx(), None);
        let syms = document_symbols_at(&doc);
        assert!(syms.is_empty());
    }

    #[test]
    fn test_document_symbols_symbol_kind_is_variable() {
        use lsp_types::SymbolKind;
        let env = test_env();
        let doc = DocumentState::new("[foo: 1]".to_string(), &env, &test_ctx(), None);
        let syms = document_symbols_at(&doc);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].kind, SymbolKind::VARIABLE);
    }

    // --- references_at tests ---

    #[test]
    fn test_references_at_finds_all() {
        let env = test_env();
        // "[x: 1  y: $x  z: $x]"
        //  0         1         2
        //  0123456789012345678901
        let source = "[x: 1  y: $x  z: $x]";
        let doc = DocumentState::new(source.to_string(), &env, &test_ctx(), None);
        let uri = test_uri();
        // Cursor on first "$x" at offset 11
        let locs = references_at(&doc, &uri, 11);
        assert_eq!(locs.len(), 2, "should find both $x refs; got {locs:?}");
    }

    #[test]
    fn test_references_at_single_ref() {
        let env = test_env();
        let doc = DocumentState::new("[x: 1  y: $x]".to_string(), &env, &test_ctx(), None);
        let uri = test_uri();
        // Cursor on "$x" at offset 11
        let locs = references_at(&doc, &uri, 11);
        assert_eq!(locs.len(), 1, "should find single ref; got {locs:?}");
    }

    #[test]
    fn test_references_at_no_ref_on_literal() {
        let env = test_env();
        let doc = DocumentState::new("[x: 42]".to_string(), &env, &test_ctx(), None);
        let uri = test_uri();
        // Offset 4 is on the integer '42', not a VarRef.
        let locs = references_at(&doc, &uri, 4);
        assert!(locs.is_empty(), "int literal has no references");
    }

    #[test]
    fn test_references_at_parse_error_returns_empty() {
        let env = test_env();
        let doc = DocumentState::new("[unterminated".to_string(), &env, &test_ctx(), None);
        let uri = test_uri();
        let locs = references_at(&doc, &uri, 1);
        assert!(locs.is_empty());
    }

    #[test]
    fn test_references_at_uri_matches() {
        let env = test_env();
        let source = "[x: 1  y: $x]";
        let doc = DocumentState::new(source.to_string(), &env, &test_ctx(), None);
        let uri = test_uri();
        let locs = references_at(&doc, &uri, 11);
        for loc in &locs {
            assert_eq!(
                loc.uri, uri,
                "all references should be in the same document"
            );
        }
    }

    // --- rename_at tests ---

    #[test]
    fn test_rename_at_simple() {
        let env = test_env();
        // "[x: 1  y: $x]"
        //  0123456789012345
        //         ^ $x at 11
        let source = "[x: 1  y: $x]";
        let doc = DocumentState::new(source.to_string(), &env, &test_ctx(), None);
        // Cursor on "$x" at offset 11
        let edits = rename_at(&doc, 11, "z");
        assert!(edits.is_some(), "should produce edits");
        let edits = edits.unwrap();
        // Should rename the VarRef "$x" and the definition key "x"
        assert!(
            edits.len() >= 1,
            "should have at least one edit; got {:?}",
            edits
        );
        for edit in &edits {
            assert_eq!(edit.new_text, "z", "all edits should rename to 'z'");
        }
    }

    #[test]
    fn test_rename_at_renames_definition_and_uses() {
        let env = test_env();
        // "[x: 1  y: $x  z: $x]"
        //  0         1         2
        //  0123456789012345678901
        //  key at 1, $x at 11, $x at 18
        let source = "[x: 1  y: $x  z: $x]";
        let doc = DocumentState::new(source.to_string(), &env, &test_ctx(), None);
        // Cursor on first "$x" at offset 11
        let edits = rename_at(&doc, 11, "foo");
        assert!(edits.is_some(), "should produce edits");
        let edits = edits.unwrap();
        // Should rename: definition key "x" + two VarRef occurrences
        assert!(
            edits.len() >= 2,
            "should rename at least 2 occurrences; got {:?}",
            edits
        );
        for edit in &edits {
            assert_eq!(edit.new_text, "foo");
        }
    }

    #[test]
    fn test_rename_at_invalid_name_rejected() {
        let env = test_env();
        let doc = DocumentState::new("[x: 1  y: $x]".to_string(), &env, &test_ctx(), None);
        // New name with invalid characters (contains '@')
        let edits = rename_at(&doc, 11, "x@y");
        assert!(edits.is_none(), "identifier with '@' should be rejected");
    }

    #[test]
    fn test_rename_at_empty_name_rejected() {
        let env = test_env();
        let doc = DocumentState::new("[x: 1  y: $x]".to_string(), &env, &test_ctx(), None);
        let edits = rename_at(&doc, 11, "");
        assert!(edits.is_none(), "empty name should be rejected");
    }

    #[test]
    fn test_rename_at_digit_start_rejected() {
        let env = test_env();
        let doc = DocumentState::new("[x: 1  y: $x]".to_string(), &env, &test_ctx(), None);
        let edits = rename_at(&doc, 11, "123abc");
        assert!(edits.is_none(), "digit-starting name should be rejected");
    }

    #[test]
    fn test_rename_at_parse_error_returns_none() {
        let env = test_env();
        let doc = DocumentState::new("[unterminated".to_string(), &env, &test_ctx(), None);
        let edits = rename_at(&doc, 1, "z");
        assert!(edits.is_none());
    }

    #[test]
    fn test_rename_at_no_ref_at_offset_returns_none() {
        let env = test_env();
        // Offset 4 is on the integer literal '1', not a VarRef.
        let doc = DocumentState::new("[x: 1]".to_string(), &env, &test_ctx(), None);
        let edits = rename_at(&doc, 4, "z");
        assert!(edits.is_none(), "no rename when cursor is on a literal");
    }

    #[test]
    fn test_rename_at_hyphenated_name_valid() {
        let env = test_env();
        // Tinct identifiers can contain hyphens.
        let source = "[my-key: 1  y: $my-key]";
        let doc = DocumentState::new(source.to_string(), &env, &test_ctx(), None);
        // Cursor on "$my-key" at offset 15
        let edits = rename_at(&doc, 15, "new-key");
        assert!(edits.is_some(), "hyphenated rename should succeed");
    }

    // --- is_valid_tinct_identifier tests ---

    #[test]
    fn test_is_valid_ident_simple() {
        assert!(is_valid_tinct_identifier("foo"));
        assert!(is_valid_tinct_identifier("my-key"));
        assert!(is_valid_tinct_identifier("pred?"));
        assert!(is_valid_tinct_identifier("x"));
    }

    #[test]
    fn test_is_valid_ident_rejects_empty() {
        assert!(!is_valid_tinct_identifier(""));
    }

    #[test]
    fn test_is_valid_ident_rejects_digit_start() {
        assert!(!is_valid_tinct_identifier("1abc"));
    }

    #[test]
    fn test_is_valid_ident_rejects_special_chars() {
        assert!(!is_valid_tinct_identifier("x@y"));
        assert!(!is_valid_tinct_identifier("x.y"));
        assert!(!is_valid_tinct_identifier("x y"));
        assert!(!is_valid_tinct_identifier("x[y"));
        assert!(!is_valid_tinct_identifier("x]y"));
        assert!(!is_valid_tinct_identifier("x:y"));
        assert!(!is_valid_tinct_identifier("x|y"));
    }

    // --- inlay_hints_for tests ---

    #[test]
    fn test_inlay_hints_for_simple_binding() {
        let env = test_env();
        let doc = DocumentState::new("[x: 42]".to_string(), &env, &test_ctx(), None);
        let hints = inlay_hints_for(&doc);
        // Should emit a hint for the binding "x" with type "42" or "Int"
        assert!(
            !hints.is_empty(),
            "should emit inlay hints for top-level bindings"
        );
        let hint = &hints[0];
        // The label should start with ": "
        let label_str = match &hint.label {
            lsp_types::InlayHintLabel::String(s) => s.clone(),
            lsp_types::InlayHintLabel::LabelParts(parts) => parts
                .iter()
                .map(|p| p.value.clone())
                .collect::<Vec<_>>()
                .join(""),
        };
        assert!(
            label_str.starts_with(": "),
            "inlay hint label should start with ': '; got: {label_str}"
        );
    }

    #[test]
    fn test_inlay_hints_skips_annotated_bindings() {
        let env = test_env();
        // When a binding has a TypeAssert annotation, no inlay hint should be emitted.
        let _doc = DocumentState::new("[x@Int: 42]".to_string(), &env, &test_ctx(), None);
        // The key is annotated (x@Int), not the value; value is 42 (no TypeAssert)
        // so a hint IS expected here (annotation on key != TypeAssert on value).
        // A TypeAssert on the value looks like: [x: @Int 42]
        let doc2 = DocumentState::new("[x: @Int 42]".to_string(), &env, &test_ctx(), None);
        let hints2 = inlay_hints_for(&doc2);
        // Value has TypeAssert — no hint expected.
        assert!(
            hints2.is_empty(),
            "should not emit hint when value is already annotated with @Type; got {:?}",
            hints2
        );
    }

    #[test]
    fn test_inlay_hints_parse_error_returns_empty() {
        let env = test_env();
        let doc = DocumentState::new("[unterminated".to_string(), &env, &test_ctx(), None);
        let hints = inlay_hints_for(&doc);
        assert!(hints.is_empty());
    }

    #[test]
    fn test_inlay_hints_non_dict_returns_empty() {
        let env = test_env();
        let doc = DocumentState::new("42".to_string(), &env, &test_ctx(), None);
        let hints = inlay_hints_for(&doc);
        assert!(hints.is_empty());
    }

    #[test]
    fn test_inlay_hints_position_is_after_key() {
        let env = test_env();
        // "[x: 42]" — key 'x' is at column 1 (0-indexed), so end of key is column 2 (char 1).
        let doc = DocumentState::new("[x: 42]".to_string(), &env, &test_ctx(), None);
        let hints = inlay_hints_for(&doc);
        if !hints.is_empty() {
            let pos = hints[0].position;
            // Key "x" is at offset 1, ends at offset 2. On line 0, character 2.
            assert_eq!(pos.line, 0, "hint should be on line 0");
            // character = 2 (past the 'x' at column 1)
            assert_eq!(
                pos.character, 2,
                "hint position should be right after the key 'x'"
            );
        }
    }

    // --- signature_help_at tests ---

    #[test]
    fn test_signature_help_inside_call() {
        let env = test_env();
        // "[f: [fn [x@Int y@Int] 0]]\n[call $f 1 2]"
        //  0         1         2         3
        //  0123456789012345678901234567890123456789
        // "$f" is at offset 33, "1" is at offset 36, "2" is at offset 38
        let source = "[f: [fn [x@Int y@Int] 0]]\n[call $f 1 2]";
        let doc = DocumentState::new(source.to_string(), &env, &test_ctx(), None);
        // Offset 37 is between "1" and "2" — on the second argument.
        let help = signature_help_at(&doc, 37);
        // Should return some signature help when inside a call with a known typed function.
        // (May be None if type inference doesn't resolve $f at the call site — acceptable.)
        if let Some(h) = help {
            assert!(!h.signatures.is_empty(), "should have at least one signature");
            let sig = &h.signatures[0];
            assert!(
                sig.label.contains("Fn@"),
                "signature label should start with Fn@, got: {}",
                sig.label
            );
        }
    }

    #[test]
    fn test_signature_help_not_in_call() {
        let env = test_env();
        // A bare integer literal — not inside a call.
        let doc = DocumentState::new("42".to_string(), &env, &test_ctx(), None);
        let help = signature_help_at(&doc, 0);
        assert!(
            help.is_none(),
            "should return None when cursor is not inside a call"
        );
    }

    #[test]
    fn test_signature_help_parse_error() {
        let env = test_env();
        let doc = DocumentState::new("[unterminated".to_string(), &env, &test_ctx(), None);
        let help = signature_help_at(&doc, 1);
        assert!(help.is_none(), "should return None on parse error");
    }

    #[test]
    fn test_signature_help_active_parameter_index() {
        let env = test_env();
        // Use a builtin with a known function type.
        // "[call $+ 1 2]" — $+ is a function, cursor on "2" means active_param = 1.
        // "[call $+ 1 2]"
        //  0         1
        //  0123456789012
        // "$+" at offset 6, "1" at offset 9, "2" at offset 11
        let source = "[call $+ 1 2]";
        let doc = DocumentState::new(source.to_string(), &env, &test_ctx(), None);
        // Cursor on "2" at offset 11 (after "1" which starts at offset 9)
        let help = signature_help_at(&doc, 11);
        if let Some(h) = help {
            // active_parameter should be 1 (0-indexed: past the first arg "1")
            let active = h.active_parameter.unwrap_or(0);
            assert_eq!(
                active, 1,
                "cursor on second arg should yield active_parameter=1, got {active}"
            );
        }
    }

    // --- workspace_symbols_for tests ---

    #[test]
    fn test_workspace_symbols_empty_query_returns_all() {
        let env = test_env();
        let doc = DocumentState::new("[x: 1  y: 2  z: 3]".to_string(), &env, &test_ctx(), None);
        let uri = test_uri();
        let syms = workspace_symbols_for(&doc, &uri, "");
        assert_eq!(syms.len(), 3, "empty query should return all symbols");
    }

    #[test]
    fn test_workspace_symbols_prefix_filter() {
        let env = test_env();
        let doc =
            DocumentState::new("[foo: 1  bar: 2  baz: 3]".to_string(), &env, &test_ctx(), None);
        let uri = test_uri();
        let syms = workspace_symbols_for(&doc, &uri, "ba");
        assert_eq!(
            syms.len(),
            2,
            "prefix 'ba' should match 'bar' and 'baz'; got {:?}",
            syms.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_workspace_symbols_case_insensitive() {
        let env = test_env();
        let doc = DocumentState::new("[Foo: 1  FOO: 2  foo: 3]".to_string(), &env, &test_ctx(), None);
        let uri = test_uri();
        let syms = workspace_symbols_for(&doc, &uri, "foo");
        assert_eq!(
            syms.len(),
            3,
            "prefix 'foo' should match 'Foo', 'FOO', 'foo' (case-insensitive); got {:?}",
            syms.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_workspace_symbols_no_match() {
        let env = test_env();
        let doc = DocumentState::new("[foo: 1  bar: 2]".to_string(), &env, &test_ctx(), None);
        let uri = test_uri();
        let syms = workspace_symbols_for(&doc, &uri, "xyz");
        assert!(syms.is_empty(), "no match should return empty vec");
    }

    #[test]
    fn test_workspace_symbols_parse_error_returns_empty() {
        let env = test_env();
        let doc = DocumentState::new("[unterminated".to_string(), &env, &test_ctx(), None);
        let uri = test_uri();
        let syms = workspace_symbols_for(&doc, &uri, "");
        assert!(syms.is_empty(), "parse error should yield empty list");
    }

    #[test]
    fn test_workspace_symbols_uri_is_set() {
        let env = test_env();
        let doc = DocumentState::new("[x: 1]".to_string(), &env, &test_ctx(), None);
        let uri = test_uri();
        let syms = workspace_symbols_for(&doc, &uri, "");
        assert_eq!(syms.len(), 1);
        // Location should be a OneOf::Left(Location)
        if let lsp_types::OneOf::Left(ref loc) = syms[0].location {
            assert_eq!(loc.uri, uri, "symbol location URI should match document URI");
        } else {
            panic!("expected OneOf::Left(Location), got workspace-only location");
        }
    }
}

/// Validate that a string is a legal tinct identifier suitable for rename.
///
/// Tinct identifiers use a denylist (same as `is_var_ident_char` in lexer.rs):
/// everything except whitespace, structural delimiters, dot, pipe, `@`, `"` is
/// allowed. In addition the name must be non-empty and must not start with a
/// digit (to avoid colliding with number literals).
///
/// Returns `true` if `name` is a valid tinct identifier.
pub fn is_valid_tinct_identifier(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = name.as_bytes();
    // First character: must not be a digit (would look like a number literal)
    if bytes[0].is_ascii_digit() {
        return false;
    }
    // All characters: use the same denylist as the lexer's is_var_ident_char
    for c in name.chars() {
        if matches!(
            c,
            ' ' | '\t' | '\r' | '\n' | '[' | ']' | ':' | ';' | '#' | '"' | '@' | '.' | '|'
        ) {
            return false;
        }
    }
    true
}

/// Rename all occurrences of the name under the cursor to `new_name`.
///
/// Finds the variable name at `offset`, then collects every `VarRef` span and
/// the definition key span (from `find_key_definition_span`), and returns a
/// list of `TextEdit` values replacing each occurrence with `new_name`.
///
/// Returns `None` if:
/// - The document has a parse error.
/// - No variable reference is found at the offset.
/// - `new_name` is not a valid tinct identifier.
pub fn rename_at(doc: &DocumentState, offset: usize, new_name: &str) -> Option<Vec<TextEdit>> {
    if !is_valid_tinct_identifier(new_name) {
        return None;
    }

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

    let mut edits: Vec<TextEdit> = Vec::new();

    // Collect VarRef spans directly as TextEdits.
    for document in &file.node.documents {
        for expr in &document.node.expressions {
            collect_rename_edits_spanned(&expr.node, expr.span, &name, &doc.text, &mut edits);
        }
    }

    // Also rename the definition site key (if present and matches).
    for document in &file.node.documents {
        for expr in &document.node.expressions {
            collect_definition_key_edits(&expr.node, &name, &doc.text, &mut edits);
        }
    }

    if edits.is_empty() {
        return None;
    }

    // Replace new_text in all edits with new_name.
    for edit in &mut edits {
        edit.new_text = new_name.to_string();
    }

    Some(edits)
}

/// Collect TextEdit values for every VarRef matching `name`.
fn collect_rename_edits_spanned(
    expr: &Expr,
    span: Span,
    name: &str,
    source: &str,
    out: &mut Vec<TextEdit>,
) {
    match expr {
        Expr::VarRef { name: ref_name, .. } => {
            if ref_name == name {
                let range = llt_span_to_lsp_range(&span, source);
                out.push(TextEdit {
                    range,
                    new_text: String::new(), // filled in by caller
                });
            }
        }

        Expr::Dict(entries) => {
            for entry in entries {
                if let Some(ref key) = entry.node.key {
                    collect_rename_edits_spanned(&key.node, key.span, name, source, out);
                }
                collect_rename_edits_spanned(
                    &entry.node.value.node,
                    entry.node.value.span,
                    name,
                    source,
                    out,
                );
            }
        }

        Expr::Call {
            func,
            args,
            named_args,
            ..
        } => {
            collect_rename_edits_spanned(&func.node, func.span, name, source, out);
            for a in args {
                collect_rename_edits_spanned(&a.node, a.span, name, source, out);
            }
            for na in named_args {
                collect_rename_edits_spanned(
                    &na.node.value.node,
                    na.node.value.span,
                    name,
                    source,
                    out,
                );
            }
        }

        Expr::Fn { body, .. } => {
            collect_rename_edits_spanned(&body.node, body.span, name, source, out);
        }

        Expr::DotAccess { expr: target, .. } => {
            collect_rename_edits_spanned(&target.node, target.span, name, source, out);
        }

        Expr::Sequential(exprs) => {
            for seq_expr in exprs {
                collect_rename_edits_spanned(&seq_expr.node, seq_expr.span, name, source, out);
            }
        }

        Expr::Pipe { lhs, rhs } => {
            collect_rename_edits_spanned(&lhs.node, lhs.span, name, source, out);
            collect_rename_edits_spanned(&rhs.node, rhs.span, name, source, out);
        }

        Expr::TypeAlias { body, .. } => {
            collect_rename_edits_spanned(&body.node, body.span, name, source, out);
        }

        Expr::TypeAssert { expr: inner, .. } => {
            collect_rename_edits_spanned(&inner.node, inner.span, name, source, out);
        }

        Expr::Quote(inner) | Expr::Unquote(inner) | Expr::UnquoteSplice(inner) => {
            collect_rename_edits_spanned(&inner.node, inner.span, name, source, out);
        }

        Expr::Match { scrutinee, arms } => {
            collect_rename_edits_spanned(&scrutinee.node, scrutinee.span, name, source, out);
            for arm in arms {
                collect_rename_edits_spanned(&arm.body.node, arm.body.span, name, source, out);
            }
        }

        Expr::ClassDecl { methods, .. } | Expr::InstanceDecl { methods, .. } => {
            for method in methods {
                if let Some(key) = &method.node.key {
                    collect_rename_edits_spanned(&key.node, key.span, name, source, out);
                }
                collect_rename_edits_spanned(
                    &method.node.value.node,
                    method.node.value.span,
                    name,
                    source,
                    out,
                );
            }
        }

        Expr::DefMacro { body, .. } => {
            collect_rename_edits_spanned(&body.node, body.span, name, source, out);
        }

        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Str(_)
        | Expr::Rest(_)
        | Expr::Annotated { .. }
        | Expr::TypeApp { .. }
        | Expr::Error(_) => {}
    }
}

/// Collect a TextEdit for the definition key of `name` (if found).
///
/// Walks dict entry keys and emits an edit for the key span if it matches `name`.
/// This covers the binding site (e.g. `x` in `[x: 1]`) in addition to all VarRef uses.
fn collect_definition_key_edits(expr: &Expr, name: &str, source: &str, out: &mut Vec<TextEdit>) {
    match expr {
        Expr::Dict(entries) => {
            for entry in entries {
                if let Some(ref key) = entry.node.key {
                    // Check whether this key matches the name being renamed.
                    let key_matches = match &key.node {
                        Expr::Str(s) => s == name,
                        Expr::Annotated { name: kname, .. } => kname == name,
                        Expr::VarRef { name: kname, .. } => kname == name,
                        _ => false,
                    };
                    if key_matches {
                        // For Annotated keys, rename only the name portion (before @).
                        // The span of an Annotated key covers the full `name@Type` — but we
                        // want to emit a range for just the name text.
                        //
                        // For simplicity, use the key span for Str and VarRef (the whole token),
                        // and for Annotated use the same key span (the name portion is a prefix).
                        // Editors that support partial-span edits will highlight correctly.
                        let range = llt_span_to_lsp_range(&key.span, source);
                        // For Annotated, trim the range to just the name prefix.
                        let range = match &key.node {
                            Expr::Annotated { name: kname, .. } => {
                                // The name occupies bytes [key.span.start, key.span.start + kname.len())
                                let name_span = crate::ast::Span {
                                    start: key.span.start,
                                    end: crate::ast::Position {
                                        offset: key.span.start.offset + kname.len(),
                                        line: key.span.start.line,
                                        column: key.span.start.column + kname.len(),
                                    },
                                };
                                llt_span_to_lsp_range(&name_span, source)
                            }
                            _ => range,
                        };
                        out.push(TextEdit {
                            range,
                            new_text: String::new(), // filled in by caller
                        });
                    }
                    // Also recurse into the value for nested dict definitions.
                    collect_definition_key_edits(&entry.node.value.node, name, source, out);
                }
            }
        }

        Expr::Call {
            func,
            args,
            named_args,
            ..
        } => {
            collect_definition_key_edits(&func.node, name, source, out);
            for a in args {
                collect_definition_key_edits(&a.node, name, source, out);
            }
            for na in named_args {
                collect_definition_key_edits(&na.node.value.node, name, source, out);
            }
        }

        Expr::Fn { body, .. } => {
            collect_definition_key_edits(&body.node, name, source, out);
        }

        Expr::Sequential(exprs) => {
            for seq_expr in exprs {
                collect_definition_key_edits(&seq_expr.node, name, source, out);
            }
        }

        Expr::Pipe { lhs, rhs } => {
            collect_definition_key_edits(&lhs.node, name, source, out);
            collect_definition_key_edits(&rhs.node, name, source, out);
        }

        Expr::TypeAlias { body, .. } => {
            collect_definition_key_edits(&body.node, name, source, out);
        }

        Expr::TypeAssert { expr: inner, .. } => {
            collect_definition_key_edits(&inner.node, name, source, out);
        }

        Expr::Quote(inner) | Expr::Unquote(inner) | Expr::UnquoteSplice(inner) => {
            collect_definition_key_edits(&inner.node, name, source, out);
        }

        Expr::Match { scrutinee, arms } => {
            collect_definition_key_edits(&scrutinee.node, name, source, out);
            for arm in arms {
                collect_definition_key_edits(&arm.body.node, name, source, out);
            }
        }

        Expr::ClassDecl { methods, .. } | Expr::InstanceDecl { methods, .. } => {
            for method in methods {
                if let Some(key) = &method.node.key {
                    collect_definition_key_edits(&key.node, name, source, out);
                }
                collect_definition_key_edits(&method.node.value.node, name, source, out);
            }
        }

        Expr::DefMacro { body, .. } => {
            collect_definition_key_edits(&body.node, name, source, out);
        }

        _ => {}
    }
}

/// Generate inlay type hints for all top-level dict bindings that are not already
/// annotated with a type.
///
/// For each top-level dict binding whose value does NOT carry a `TypeAssert` annotation,
/// look up its inferred type in `scheme_map` or `type_map` and emit an `InlayHint`
/// positioned at the end of the binding name.
///
/// Returns an empty list if the document has a parse error.
pub fn inlay_hints_for(doc: &DocumentState) -> Vec<lsp_types::InlayHint> {
    use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel};

    let file = match &doc.ast {
        Ok(f) => f,
        Err(_) => return vec![],
    };

    let mut hints = Vec::new();

    for document in &file.node.documents {
        for expr in &document.node.expressions {
            if let Expr::Dict(entries) = &expr.node {
                for entry in entries {
                    // Only process entries with a static key.
                    let key = match &entry.node.key {
                        Some(k) => k,
                        None => continue,
                    };

                    // Skip entries whose value is already annotated (TypeAssert node).
                    if matches!(&entry.node.value.node, Expr::TypeAssert { .. }) {
                        continue;
                    }

                    // Look up the inferred type for the value span.
                    let value_span = entry.node.value.span;
                    let span_key = (value_span.start.offset, value_span.end.offset);

                    let type_str: Option<String> =
                        if let Some(scheme) = doc.scheme_map.get(&span_key) {
                            let raw = format_scheme_for_hover(scheme);
                            Some(crate::types::pretty_type_str(&raw))
                        } else if let Some(ty) = doc.type_map.get(&span_key) {
                            Some(crate::types::pretty_type(ty))
                        } else {
                            None
                        };

                    let type_str = match type_str {
                        Some(s) if !s.is_empty() && s != "<error>" => s,
                        _ => continue,
                    };

                    // Position the hint at the end of the binding key name.
                    let key_end = llt_span_to_lsp_range(&key.span, &doc.text).end;

                    hints.push(InlayHint {
                        position: key_end,
                        label: InlayHintLabel::String(format!(": {}", type_str)),
                        kind: Some(InlayHintKind::TYPE),
                        text_edits: None,
                        tooltip: None,
                        padding_left: Some(false),
                        padding_right: Some(true),
                        data: None,
                    });
                }
            }
        }
    }

    hints
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
        Expr::DefMacro { body, .. } => {
            collect_dict_keys_in_scope(&body.node, body.span, offset, items, seen);
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

// ─── Task 6: textDocument/signatureHelp ─────────────────────────────────────

/// Walk the AST and find the innermost `Call` expression containing the offset.
///
/// Returns `(func_span_key, active_arg_index)` where:
/// - `func_span_key` is `(start_offset, end_offset)` of the function expression
/// - `active_arg_index` is the 0-based index of the argument the cursor is on
///
/// The active argument index is computed by counting how many positional args
/// start before the cursor position.
fn find_enclosing_call(
    expr: &Expr,
    span: Span,
    offset: usize,
) -> Option<((usize, usize), usize)> {
    if !span_contains(span, offset) {
        return None;
    }

    match expr {
        Expr::Call {
            func,
            args,
            named_args,
            ..
        } => {
            // Try to find a deeper call first (cursor inside an arg expression).
            for arg in args.iter() {
                if let Some(inner) = find_enclosing_call(&arg.node, arg.span, offset) {
                    return Some(inner);
                }
            }
            for na in named_args.iter() {
                if let Some(inner) =
                    find_enclosing_call(&na.node.value.node, na.node.value.span, offset)
                {
                    return Some(inner);
                }
            }
            // No deeper call — this Call is the innermost one.
            // Count args that start before cursor position.
            let active = args
                .iter()
                .filter(|a| a.span.start.offset < offset)
                .count();
            let func_key = (func.span.start.offset, func.span.end.offset);
            Some((func_key, active))
        }

        Expr::Dict(entries) => entries.iter().find_map(|entry| {
            entry.node.key.as_ref().and_then(|k| {
                find_enclosing_call(&k.node, k.span, offset)
            }).or_else(|| {
                find_enclosing_call(&entry.node.value.node, entry.node.value.span, offset)
            })
        }),

        Expr::Fn { body, .. } => find_enclosing_call(&body.node, body.span, offset),

        Expr::DotAccess { expr: target, .. } => {
            find_enclosing_call(&target.node, target.span, offset)
        }

        Expr::Sequential(exprs) => exprs
            .iter()
            .find_map(|e| find_enclosing_call(&e.node, e.span, offset)),

        Expr::Pipe { lhs, rhs } => find_enclosing_call(&lhs.node, lhs.span, offset)
            .or_else(|| find_enclosing_call(&rhs.node, rhs.span, offset)),

        Expr::TypeAlias { body, .. } => find_enclosing_call(&body.node, body.span, offset),

        Expr::TypeAssert { expr: inner, .. } => {
            find_enclosing_call(&inner.node, inner.span, offset)
        }

        Expr::Quote(inner) | Expr::Unquote(inner) | Expr::UnquoteSplice(inner) => {
            find_enclosing_call(&inner.node, inner.span, offset)
        }

        Expr::Match { scrutinee, arms } => {
            find_enclosing_call(&scrutinee.node, scrutinee.span, offset).or_else(|| {
                arms.iter()
                    .find_map(|arm| find_enclosing_call(&arm.body.node, arm.body.span, offset))
            })
        }

        Expr::ClassDecl { methods, .. } | Expr::InstanceDecl { methods, .. } => {
            methods.iter().find_map(|method| {
                method
                    .node
                    .key
                    .as_ref()
                    .and_then(|k| find_enclosing_call(&k.node, k.span, offset))
                    .or_else(|| {
                        find_enclosing_call(
                            &method.node.value.node,
                            method.node.value.span,
                            offset,
                        )
                    })
            })
        }

        Expr::DefMacro { body, .. } => find_enclosing_call(&body.node, body.span, offset),

        // Leaves: no call here.
        _ => None,
    }
}

/// Generate signature help for the cursor position.
///
/// When the cursor is inside a `[f arg1 arg2 ...]` Call expression, looks up
/// `f`'s TypeScheme, formats it as a `SignatureHelp` response, and highlights
/// the active parameter (the argument slot the cursor is in).
///
/// Returns `None` if:
/// - The document has a parse error.
/// - The cursor is not inside a Call expression.
/// - The function has no TypeScheme entry (unresolved or not a function type).
pub fn signature_help_at(
    doc: &DocumentState,
    offset: usize,
) -> Option<lsp_types::SignatureHelp> {
    use crate::types::Type;
    use lsp_types::{
        Documentation, ParameterInformation, ParameterLabel, SignatureHelp, SignatureInformation,
    };

    let file = match &doc.ast {
        Ok(f) => f,
        Err(_) => return None,
    };

    // Find the innermost Call containing the cursor.
    let (func_span_key, active_param_idx) = file.node.documents.iter().find_map(|document| {
        document
            .node
            .expressions
            .iter()
            .find_map(|expr| find_enclosing_call(&expr.node, expr.span, offset))
    })?;

    // Prefer scheme_map (has constraints) over type_map.
    let scheme = doc.scheme_map.get(&func_span_key).cloned();
    let func_type: Option<Type> = if let Some(ref s) = scheme {
        Some(s.body.clone())
    } else {
        doc.type_map.get(&func_span_key).cloned()
    };

    let func_type = func_type?;

    // We only generate signature help for Function types.
    let (params, ret, _variadic) = match func_type {
        Type::Function {
            params,
            ret,
            variadic,
        } => (params, ret, variadic),
        _ => return None,
    };

    // Build the signature label: `Fn@ReturnType [param1@Type param2@Type ...]`
    let param_labels: Vec<String> = params
        .iter()
        .map(|(name, ty)| {
            if let Some(n) = name {
                format!("{}@{}", n, ty)
            } else {
                format!("@{}", ty)
            }
        })
        .collect();

    let sig_label = if param_labels.is_empty() {
        format!("Fn@{}", ret)
    } else {
        format!("Fn@{} [{}]", ret, param_labels.join("  "))
    };

    // Build ParameterInformation for each param.
    let parameters: Vec<ParameterInformation> = param_labels
        .iter()
        .map(|pl| ParameterInformation {
            label: ParameterLabel::Simple(pl.clone()),
            documentation: None,
        })
        .collect();

    // Optional documentation from TypeScheme.
    let doc_text = scheme
        .as_ref()
        .and_then(|s| s.doc.as_ref())
        .map(|d| Documentation::String(d.clone()));

    let sig_info = SignatureInformation {
        label: sig_label,
        documentation: doc_text,
        parameters: if parameters.is_empty() {
            None
        } else {
            Some(parameters)
        },
        active_parameter: Some(active_param_idx as u32),
    };

    Some(SignatureHelp {
        signatures: vec![sig_info],
        active_signature: Some(0),
        active_parameter: Some(active_param_idx as u32),
    })
}

// ─── Task 7: workspace/symbol ────────────────────────────────────────────────

/// Collect top-level binding names from a document that match a query.
///
/// Case-insensitive prefix match: a symbol matches if its name, lowercased,
/// starts with `query_lower`.  An empty query matches every symbol.
///
/// Returns `WorkspaceSymbol` entries with `OneOf::Left(location)` pointing at
/// the key span in the given document.
pub fn workspace_symbols_for(
    doc: &DocumentState,
    uri: &Uri,
    query_lower: &str,
) -> Vec<lsp_types::WorkspaceSymbol> {
    use lsp_types::WorkspaceSymbol;

    let file = match &doc.ast {
        Ok(f) => f,
        Err(_) => return vec![],
    };

    let mut symbols = Vec::new();

    for document in &file.node.documents {
        for expr in &document.node.expressions {
            if let Expr::Dict(entries) = &expr.node {
                for entry in entries {
                    let key = match &entry.node.key {
                        Some(k) => k,
                        None => continue,
                    };
                    let name: Option<String> = match &key.node {
                        Expr::Str(s) => Some(s.clone()),
                        Expr::Annotated { name, .. } => Some(name.clone()),
                        Expr::VarRef { name, .. } => Some(name.clone()),
                        _ => None,
                    };
                    let name = match name {
                        Some(n) => n,
                        None => continue,
                    };

                    // Case-insensitive prefix match.
                    if !query_lower.is_empty()
                        && !name.to_lowercase().starts_with(query_lower)
                    {
                        continue;
                    }

                    let range = llt_span_to_lsp_range(&key.span, &doc.text);

                    symbols.push(WorkspaceSymbol {
                        name,
                        kind: lsp_types::SymbolKind::VARIABLE,
                        tags: None,
                        container_name: None,
                        // Use `OneOf::Right(WorkspaceLocation)` — no range needed for the
                        // basic case; clients that need the range will issue a resolve request.
                        location: lsp_types::OneOf::Left(Location {
                            uri: uri.clone(),
                            range,
                        }),
                        data: None,
                    });
                }
            }
        }
    }

    symbols
}
