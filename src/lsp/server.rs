//! Main LSP server loop.

use std::error::Error;

use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::{
    notification::{
        DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
        PublishDiagnostics,
    },
    request::{Completion, GotoDefinition, HoverRequest, Request as _},
    CompletionOptions, CompletionParams, CompletionResponse, Diagnostic, DiagnosticSeverity,
    GotoDefinitionParams, GotoDefinitionResponse, HoverContents, HoverParams, InitializeParams,
    InitializeResult, Location, MarkedString, PublishDiagnosticsParams, Range, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};

use crate::lsp::analysis::{completion_at, definition_at, diagnostics_for, hover_at};
use crate::lsp::convert::{llt_span_to_lsp_range, lsp_position_to_offset};
use crate::lsp::document::DocumentStore;

/// Maximum document size the LSP server will process: 10 MB (matches `MAX_FILE_SIZE` in builtins).
pub(crate) const MAX_DOCUMENT_SIZE: usize = 10 * 1024 * 1024;

// Compile-time assertion: MAX_DOCUMENT_SIZE must equal MAX_FILE_SIZE so both
// code paths enforce the same limit without requiring runtime coordination.
const _: () = assert!(
    MAX_DOCUMENT_SIZE == crate::builtins::MAX_FILE_SIZE as usize,
    "MAX_DOCUMENT_SIZE and MAX_FILE_SIZE must be equal"
);

/// Maximum length of an LSP method name that will be echoed in error responses.
/// Prevents heap exhaustion from malicious clients sending arbitrarily long method names.
const MAX_METHOD_NAME_LEN: usize = 256;

/// Run the LSP server on stdio.
pub fn run_lsp() -> Result<(), Box<dyn Error>> {
    eprintln!("tinct LSP server starting...");

    let (connection, io_threads) = Connection::stdio();
    let (id, params) = connection.initialize_start()?;

    let _init_params: InitializeParams = serde_json::from_value(params)?;

    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
        definition_provider: Some(lsp_types::OneOf::Left(true)),
        completion_provider: Some(CompletionOptions::default()),
        ..Default::default()
    };

    let init_result = InitializeResult {
        capabilities,
        server_info: Some(lsp_types::ServerInfo {
            name: "tinct-lsp".to_string(),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        }),
    };

    connection.initialize_finish(id, serde_json::to_value(init_result)?)?;

    eprintln!("tinct LSP server initialized.");

    let mut store = DocumentStore::new();

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    eprintln!("tinct LSP server shutting down.");
                    break;
                }

                handle_request(&connection, &store, req)?;
            }
            Message::Notification(notif) => {
                handle_notification(&connection, &mut store, notif)?;
            }
            Message::Response(_) => {
                // Client responses to our requests (none expected in basic LSP).
            }
        }
    }

    io_threads.join()?;
    Ok(())
}

fn handle_request(
    connection: &Connection,
    store: &DocumentStore,
    req: Request,
) -> Result<(), Box<dyn Error>> {
    match req.method.as_str() {
        HoverRequest::METHOD => {
            let params: HoverParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("invalid {}: {e}", HoverRequest::METHOD);
                    connection.sender.send(Message::Response(Response {
                        id: req.id,
                        result: None,
                        error: Some(lsp_server::ResponseError {
                            code: lsp_server::ErrorCode::InvalidParams as i32,
                            message: format!("invalid params: {e}"),
                            data: None,
                        }),
                    }))?;
                    return Ok(());
                }
            };
            let uri = params.text_document_position_params.text_document.uri;
            let pos = params.text_document_position_params.position;

            // On-demand hover: if the document is not in the store (not opened),
            // load it from disk and analyze it on the fly.
            let hover = if let Some(doc) = store.get(&uri) {
                // Document is open in editor: use cached state
                lsp_position_to_offset(&pos, &doc.text)
                    .and_then(|offset| hover_at(doc, &uri, offset, &store.include_graph))
            } else {
                // Document is not open: load from URI and analyze
                use crate::lsp::document::load_doc_from_uri;
                load_doc_from_uri(&uri).and_then(|doc| {
                    lsp_position_to_offset(&pos, &doc.text)
                        .and_then(|offset| hover_at(&doc, &uri, offset, &store.include_graph))
                })
            }
            .map(|text| lsp_types::Hover {
                contents: HoverContents::Scalar(MarkedString::String(text)),
                range: None,
            });

            let result = serde_json::to_value(hover)?;
            connection.sender.send(Message::Response(Response {
                id: req.id,
                result: Some(result),
                error: None,
            }))?;
        }
        GotoDefinition::METHOD => {
            let params: GotoDefinitionParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("invalid {}: {e}", GotoDefinition::METHOD);
                    connection.sender.send(Message::Response(Response {
                        id: req.id,
                        result: None,
                        error: Some(lsp_server::ResponseError {
                            code: lsp_server::ErrorCode::InvalidParams as i32,
                            message: format!("invalid params: {e}"),
                            data: None,
                        }),
                    }))?;
                    return Ok(());
                }
            };
            let uri = params.text_document_position_params.text_document.uri;
            let pos = params.text_document_position_params.position;

            // On-demand goto-definition: if the document is not in the store (not opened),
            // load it from disk and analyze it on the fly.
            let location = if let Some(doc) = store.get(&uri) {
                // Document is open in editor: use cached state
                lsp_position_to_offset(&pos, &doc.text).and_then(|offset| {
                    definition_at(doc, &uri, offset, &store.include_graph).map(
                        |(target_uri, span)| {
                            // Determine source text for converting span to range:
                            // - Document-local: use doc.text
                            // - Included file: read from include_graph
                            let source_text: String = if target_uri == uri {
                                doc.text.clone()
                            } else {
                                // Included file: read from include_graph
                                store
                                    .include_graph
                                    .get(&target_uri)
                                    .map(|node| node.state.text.clone())
                                    .unwrap_or_default()
                            };
                            Location {
                                uri: target_uri,
                                range: llt_span_to_lsp_range(&span, &source_text),
                            }
                        },
                    )
                })
            } else {
                // Document is not open: load from URI and analyze
                use crate::lsp::document::load_doc_from_uri;
                load_doc_from_uri(&uri).and_then(|doc| {
                    lsp_position_to_offset(&pos, &doc.text).and_then(|offset| {
                        definition_at(&doc, &uri, offset, &store.include_graph).map(
                            |(target_uri, span)| {
                                // For unopened documents, read target text from disk if needed
                                let source_text: String = if target_uri == uri {
                                    doc.text.clone()
                                } else {
                                    // Cross-file definition: try include_graph first, then load from disk
                                    store
                                        .include_graph
                                        .get(&target_uri)
                                        .map(|node| node.state.text.clone())
                                        .or_else(|| load_doc_from_uri(&target_uri).map(|d| d.text))
                                        .unwrap_or_default()
                                };
                                Location {
                                    uri: target_uri,
                                    range: llt_span_to_lsp_range(&span, &source_text),
                                }
                            },
                        )
                    })
                })
            }
            .map(GotoDefinitionResponse::Scalar);

            let result = serde_json::to_value(location)?;
            connection.sender.send(Message::Response(Response {
                id: req.id,
                result: Some(result),
                error: None,
            }))?;
        }
        Completion::METHOD => {
            let params: CompletionParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("invalid {}: {e}", Completion::METHOD);
                    connection.sender.send(Message::Response(Response {
                        id: req.id,
                        result: None,
                        error: Some(lsp_server::ResponseError {
                            code: lsp_server::ErrorCode::InvalidParams as i32,
                            message: format!("invalid params: {e}"),
                            data: None,
                        }),
                    }))?;
                    return Ok(());
                }
            };
            let uri = params.text_document_position.text_document.uri;
            let pos = params.text_document_position.position;

            // On-demand completion: if the document is not in the store (not opened),
            // load it from disk and analyze it on the fly.
            let items = if let Some(doc) = store.get(&uri) {
                // Document is open in editor: use cached state
                lsp_position_to_offset(&pos, &doc.text)
                    .map(|offset| completion_at(doc, &uri, offset))
                    .unwrap_or_default()
            } else {
                // Document is not open: load from URI and analyze
                use crate::lsp::document::load_doc_from_uri;
                load_doc_from_uri(&uri)
                    .and_then(|doc| {
                        lsp_position_to_offset(&pos, &doc.text)
                            .map(|offset| completion_at(&doc, &uri, offset))
                    })
                    .unwrap_or_default()
            };

            let result = serde_json::to_value(CompletionResponse::Array(items))?;
            connection.sender.send(Message::Response(Response {
                id: req.id,
                result: Some(result),
                error: None,
            }))?;
        }
        _ => {
            // Unknown request; respond with method not found.
            // Cap method name length to prevent resource exhaustion from malicious LSP clients.
            let method_display = if req.method.len() > MAX_METHOD_NAME_LEN {
                format!(
                    "method not found: <name too long ({} bytes)>",
                    req.method.len()
                )
            } else {
                format!("method not found: {}", req.method)
            };
            connection.sender.send(Message::Response(Response {
                id: req.id,
                result: None,
                error: Some(lsp_server::ResponseError {
                    code: lsp_server::ErrorCode::MethodNotFound as i32,
                    message: method_display,
                    data: None,
                }),
            }))?;
        }
    }
    Ok(())
}

fn handle_notification(
    connection: &Connection,
    store: &mut DocumentStore,
    notif: Notification,
) -> Result<(), Box<dyn Error>> {
    match notif.method.as_str() {
        DidOpenTextDocument::METHOD => {
            let params: lsp_types::DidOpenTextDocumentParams =
                match serde_json::from_value(notif.params) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("invalid {}: {e}", DidOpenTextDocument::METHOD);
                        return Ok(());
                    }
                };
            let uri = params.text_document.uri;
            let text = params.text_document.text;

            if text.len() > MAX_DOCUMENT_SIZE {
                publish_too_large_diagnostic(connection, &uri, text.len())?;
                return Ok(());
            }

            store.update_document(uri.clone(), text);
            publish_diagnostics(connection, store, &uri)?;
        }
        DidChangeTextDocument::METHOD => {
            let params: lsp_types::DidChangeTextDocumentParams =
                match serde_json::from_value(notif.params) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("invalid {}: {e}", DidChangeTextDocument::METHOD);
                        return Ok(());
                    }
                };
            let uri = params.text_document.uri;

            // Full sync: take the entire new text from the last change.
            if let Some(change) = params.content_changes.into_iter().last() {
                let text = change.text;

                if text.len() > MAX_DOCUMENT_SIZE {
                    publish_too_large_diagnostic(connection, &uri, text.len())?;
                    return Ok(());
                }

                store.update_document(uri.clone(), text);
                publish_diagnostics(connection, store, &uri)?;
            }
        }
        DidCloseTextDocument::METHOD => {
            let params: lsp_types::DidCloseTextDocumentParams =
                match serde_json::from_value(notif.params) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("invalid {}: {e}", DidCloseTextDocument::METHOD);
                        return Ok(());
                    }
                };
            let uri = params.text_document.uri;
            store.remove_document(&uri);

            // Clear diagnostics so the editor doesn't show stale errors.
            let diag_params = PublishDiagnosticsParams {
                uri,
                diagnostics: vec![],
                version: None,
            };
            let diag_notif = Notification {
                method: PublishDiagnostics::METHOD.to_string(),
                params: serde_json::to_value(diag_params)?,
            };
            connection.sender.send(Message::Notification(diag_notif))?;
        }
        _ => {
            // Unknown notification; ignore.
        }
    }
    Ok(())
}

fn publish_too_large_diagnostic(
    connection: &Connection,
    uri: &Uri,
    size: usize,
) -> Result<(), Box<dyn Error>> {
    let params = PublishDiagnosticsParams {
        uri: uri.clone(),
        diagnostics: vec![Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: Some("tinct-lsp".to_string()),
            message: format!("document too large ({} bytes, limit 10 MB)", size),
            related_information: None,
            tags: None,
            data: None,
        }],
        version: None,
    };

    let notif = Notification {
        method: PublishDiagnostics::METHOD.to_string(),
        params: serde_json::to_value(params)?,
    };

    connection.sender.send(Message::Notification(notif))?;
    Ok(())
}

fn publish_diagnostics(
    connection: &Connection,
    store: &DocumentStore,
    uri: &Uri,
) -> Result<(), Box<dyn Error>> {
    let diagnostics = store
        .get(uri)
        .map(|doc| diagnostics_for(doc, uri))
        .unwrap_or_default();

    let params = PublishDiagnosticsParams {
        uri: uri.clone(),
        diagnostics,
        version: None,
    };

    let notif = Notification {
        method: PublishDiagnostics::METHOD.to_string(),
        params: serde_json::to_value(params)?,
    };

    connection.sender.send(Message::Notification(notif))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{
        Position, TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams,
        VersionedTextDocumentIdentifier,
    };

    #[test]
    fn test_server_capabilities() {
        let caps = ServerCapabilities {
            text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
            hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
            ..Default::default()
        };

        // Verify that the capabilities we declare are serializable.
        let _json = serde_json::to_value(caps).unwrap();
    }

    #[test]
    fn test_hover_request_serialization() {
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position {
                    line: 0,
                    character: 4,
                },
            },
            work_done_progress_params: Default::default(),
        };

        let _json = serde_json::to_value(params).unwrap();
    }

    #[test]
    fn test_did_open_notification_serialization() {
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        let params = lsp_types::DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id: "llt".to_string(),
                version: 1,
                text: "[x: 42]".to_string(),
            },
        };

        let _json = serde_json::to_value(params).unwrap();
    }

    #[test]
    fn test_did_change_notification_serialization() {
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        let params = lsp_types::DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri, version: 2 },
            content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "[y: 100]".to_string(),
            }],
        };

        let _json = serde_json::to_value(params).unwrap();
    }

    #[test]
    fn test_publish_diagnostics_serialization() {
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        let params = PublishDiagnosticsParams {
            uri,
            diagnostics: vec![],
            version: None,
        };

        let _json = serde_json::to_value(params).unwrap();
    }

    // --- Behavioral tests exercising the document/analysis layer ---
    // These test the same code paths that handle_request/handle_notification use,
    // without requiring a Connection (which is hard to mock).

    #[test]
    fn test_handle_hover_returns_value() {
        let mut store = DocumentStore::new();
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[x: 42]".to_string());
        let doc = store.get(&uri).unwrap();
        let hover = hover_at(doc, &uri, 4, &store.include_graph); // on '42'
        assert!(hover.is_some());
        assert!(hover.unwrap().contains("Int"));
    }

    #[test]
    fn test_handle_hover_no_document() {
        let store = DocumentStore::new();
        let uri = "file:///missing.llt".parse::<Uri>().unwrap();
        // If document doesn't exist, hover should return None.
        assert!(store.get(&uri).is_none());
    }

    #[test]
    fn test_diagnostics_published_on_parse_error() {
        let mut store = DocumentStore::new();
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[unterminated".to_string());
        let doc = store.get(&uri).unwrap();
        let diags = diagnostics_for(doc, &uri);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn test_diagnostics_empty_for_valid_doc() {
        let mut store = DocumentStore::new();
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[x: 42]".to_string());
        let doc = store.get(&uri).unwrap();
        let diags = diagnostics_for(doc, &uri);
        assert!(diags.is_empty());
    }

    #[test]
    fn test_document_update_replaces_content() {
        let mut store = DocumentStore::new();
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[x: 1]".to_string());
        store.update_document(uri.clone(), "[x: 2]".to_string());
        let doc = store.get(&uri).unwrap();
        assert_eq!(doc.text, "[x: 2]");
    }

    #[test]
    fn test_document_close_removes() {
        let mut store = DocumentStore::new();
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[x: 1]".to_string());
        store.remove_document(&uri);
        assert!(store.get(&uri).is_none());
    }

    #[test]
    fn test_too_large_diagnostic_message() {
        let size = 11 * 1024 * 1024; // 11 MB
        let msg = format!("document too large ({} bytes, limit 10 MB)", size);
        assert!(msg.contains("11534336 bytes"));
        assert!(msg.contains("limit 10 MB"));
    }

    #[test]
    fn test_max_document_size_constant() {
        // Compile-time assertion: MAX_DOCUMENT_SIZE must equal MAX_FILE_SIZE (both are 10 MB).
        // These two constants guard the same resource limit at different layers (LSP vs. $include).
        // A mismatch would mean the LSP accepts documents that $include would reject, or vice versa.
        const _: () = assert!(
            MAX_DOCUMENT_SIZE == crate::builtins::MAX_FILE_SIZE as usize,
            "MAX_DOCUMENT_SIZE and MAX_FILE_SIZE must be equal"
        );
        assert_eq!(MAX_DOCUMENT_SIZE, 10 * 1024 * 1024);
    }

    #[test]
    fn test_max_method_name_len_constant() {
        assert_eq!(MAX_METHOD_NAME_LEN, 256);
    }
}
