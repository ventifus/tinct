//! Main LSP server loop.

use std::error::Error;

use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::{
    notification::{
        DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
        PublishDiagnostics,
    },
    request::{
        Completion, DocumentSymbolRequest, Formatting, GotoDefinition, HoverRequest,
        InlayHintRequest, References, Rename, Request as _, SignatureHelpRequest,
        WorkspaceSymbolRequest,
    },
    CompletionOptions, CompletionParams, CompletionResponse, Diagnostic, DiagnosticSeverity,
    DocumentFormattingParams, DocumentSymbol, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, HoverContents, HoverParams, InitializeParams, InitializeResult,
    InlayHint, InlayHintParams, Location, MarkedString, OneOf, Position, PublishDiagnosticsParams,
    Range, ReferenceParams, RenameParams, ServerCapabilities, SignatureHelp, SignatureHelpOptions,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, Uri, WorkspaceEdit,
    WorkspaceSymbolParams, WorkspaceSymbolResponse,
};

use crate::lsp::analysis::{
    completion_at, definition_at, diagnostics_for, document_symbols_at, hover_at, inlay_hints_for,
    references_at, rename_at, signature_help_at, workspace_symbols_for,
};
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
        document_symbol_provider: Some(OneOf::Left(true)),
        document_formatting_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        rename_provider: Some(OneOf::Left(true)),
        inlay_hint_provider: Some(OneOf::Left(true)),
        signature_help_provider: Some(SignatureHelpOptions {
            // Trigger on space and open-bracket (entering a new arg position).
            trigger_characters: Some(vec![" ".to_string(), "[".to_string()]),
            retrigger_characters: None,
            work_done_progress_options: Default::default(),
        }),
        workspace_symbol_provider: Some(OneOf::Left(true)),
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

    let mut store = DocumentStore::new().map_err(|e| -> Box<dyn Error> { e.into() })?;

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
                lsp_position_to_offset(&pos, &doc.text).and_then(|offset| {
                    hover_at(doc, &uri, offset, &store.include_graph, store.eval_ctx())
                })
            } else {
                // Document is not open: load from URI and analyze
                use crate::lsp::document::load_doc_from_uri;
                load_doc_from_uri(&uri).and_then(|doc| {
                    lsp_position_to_offset(&pos, &doc.text).and_then(|offset| {
                        hover_at(&doc, &uri, offset, &store.include_graph, store.eval_ctx())
                    })
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
                    definition_at(doc, &uri, offset, &store.include_graph, store.prelude_surface()).map(
                        |(target_uri, span)| {
                            // Determine source text for converting span to range:
                            // - Document-local: use doc.text
                            // - Included file: read from include_graph
                            // - Prelude: read from embedded source
                            let source_text: String = if target_uri == uri {
                                doc.text.clone()
                            } else if let Some(node) = store.include_graph.get(&target_uri) {
                                // Included file: read from include_graph
                                node.state.text.clone()
                            } else {
                                // Prelude: read from embedded source
                                include_str!("../../stdlib/prelude.llt").to_string()
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
                        definition_at(
                            &doc,
                            &uri,
                            offset,
                            &store.include_graph,
                            store.prelude_surface(),
                        )
                        .map(|(target_uri, span)| {
                            // For unopened documents, read target text from disk if needed
                            let source_text: String = if target_uri == uri {
                                doc.text.clone()
                            } else if let Some(node) = store.include_graph.get(&target_uri) {
                                // Cross-file definition: read from include_graph
                                node.state.text.clone()
                            } else if let Some(prelude_uri) =
                                crate::find_libdir_path().and_then(|p| {
                                    crate::lsp::convert::file_path_to_uri(&p.join("prelude.llt"))
                                })
                            {
                                if target_uri == prelude_uri {
                                    // Prelude: read from embedded source
                                    include_str!("../../stdlib/prelude.llt").to_string()
                                } else {
                                    // Other file: load from disk
                                    load_doc_from_uri(&target_uri)
                                        .map(|d| d.text)
                                        .unwrap_or_default()
                                }
                            } else {
                                // Fallback: load from disk
                                load_doc_from_uri(&target_uri)
                                    .map(|d| d.text)
                                    .unwrap_or_default()
                            };
                            Location {
                                uri: target_uri,
                                range: llt_span_to_lsp_range(&span, &source_text),
                            }
                        })
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
        DocumentSymbolRequest::METHOD => {
            let params: lsp_types::DocumentSymbolParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("invalid {}: {e}", DocumentSymbolRequest::METHOD);
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
            let uri = params.text_document.uri;

            let symbols: Vec<DocumentSymbol> = if let Some(doc) = store.get(&uri) {
                document_symbols_at(doc)
            } else {
                use crate::lsp::document::load_doc_from_uri;
                load_doc_from_uri(&uri)
                    .map(|doc| document_symbols_at(&doc))
                    .unwrap_or_default()
            };

            let result = serde_json::to_value(DocumentSymbolResponse::Nested(symbols))?;
            connection.sender.send(Message::Response(Response {
                id: req.id,
                result: Some(result),
                error: None,
            }))?;
        }
        Formatting::METHOD => {
            let params: DocumentFormattingParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("invalid {}: {e}", Formatting::METHOD);
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
            let uri = params.text_document.uri;

            // Get document text (from store if open, from disk otherwise).
            let source: Option<String> = if let Some(doc) = store.get(&uri) {
                Some(doc.text.clone())
            } else {
                use crate::lsp::document::load_doc_from_uri;
                load_doc_from_uri(&uri).map(|doc| doc.text)
            };

            let edits: Option<Vec<TextEdit>> = source.and_then(|text| {
                // Resolve the pretty formatter script from %libdir/cli/fmt/pretty.llt.
                // If the script cannot be found, return None (no edits — silently no-op).
                let script_path = crate::find_libdir_path()
                    .map(|p| p.join("cli").join("fmt").join("pretty.llt"))?;
                crate::formatter::format_source_tinct(&text, &script_path)
                    .ok()
                    .map(|formatted| {
                        // Single whole-document replace-all edit: start at (0,0), end past last char.
                        // Count newlines to find the last line number and last-line length.
                        let newline_count = text.bytes().filter(|&b| b == b'\n').count() as u32;
                        let last_line_start = text.rfind('\n').map_or(0, |i| i + 1);
                        let last_line_len = text[last_line_start..].len() as u32;
                        let end = Position {
                            line: newline_count,
                            character: last_line_len,
                        };
                        vec![TextEdit {
                            range: Range {
                                start: Position {
                                    line: 0,
                                    character: 0,
                                },
                                end,
                            },
                            new_text: formatted,
                        }]
                    })
            });

            let result = serde_json::to_value(edits)?;
            connection.sender.send(Message::Response(Response {
                id: req.id,
                result: Some(result),
                error: None,
            }))?;
        }
        References::METHOD => {
            let params: ReferenceParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("invalid {}: {e}", References::METHOD);
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

            let locations: Vec<Location> = if let Some(doc) = store.get(&uri) {
                lsp_position_to_offset(&pos, &doc.text)
                    .map(|offset| references_at(doc, &uri, offset))
                    .unwrap_or_default()
            } else {
                use crate::lsp::document::load_doc_from_uri;
                load_doc_from_uri(&uri)
                    .and_then(|doc| {
                        lsp_position_to_offset(&pos, &doc.text)
                            .map(|offset| references_at(&doc, &uri, offset))
                    })
                    .unwrap_or_default()
            };

            // LSP spec: respond with null (None) when there are no references, or the list.
            let result = if locations.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::to_value(locations)?
            };
            connection.sender.send(Message::Response(Response {
                id: req.id,
                result: Some(result),
                error: None,
            }))?;
        }
        Rename::METHOD => {
            let params: RenameParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("invalid {}: {e}", Rename::METHOD);
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
            let new_name = params.new_name;

            let workspace_edit: Option<WorkspaceEdit> = if let Some(doc) = store.get(&uri) {
                lsp_position_to_offset(&pos, &doc.text).and_then(|offset| {
                    rename_at(doc, offset, &new_name).map(|edits| {
                        #[allow(clippy::mutable_key_type)]
                        // Uri interior mutability is safe for HashMap keys
                        let mut changes = std::collections::HashMap::new();
                        changes.insert(uri.clone(), edits);
                        WorkspaceEdit {
                            changes: Some(changes),
                            document_changes: None,
                            change_annotations: None,
                        }
                    })
                })
            } else {
                use crate::lsp::document::load_doc_from_uri;
                load_doc_from_uri(&uri).and_then(|doc| {
                    lsp_position_to_offset(&pos, &doc.text).and_then(|offset| {
                        rename_at(&doc, offset, &new_name).map(|edits| {
                            #[allow(clippy::mutable_key_type)]
                            // Uri interior mutability is safe for HashMap keys
                            let mut changes = std::collections::HashMap::new();
                            changes.insert(uri.clone(), edits);
                            WorkspaceEdit {
                                changes: Some(changes),
                                document_changes: None,
                                change_annotations: None,
                            }
                        })
                    })
                })
            };

            let result = serde_json::to_value(workspace_edit)?;
            connection.sender.send(Message::Response(Response {
                id: req.id,
                result: Some(result),
                error: None,
            }))?;
        }
        InlayHintRequest::METHOD => {
            let params: InlayHintParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("invalid {}: {e}", InlayHintRequest::METHOD);
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
            let uri = params.text_document.uri;

            let hints: Vec<InlayHint> = if let Some(doc) = store.get(&uri) {
                inlay_hints_for(doc)
            } else {
                use crate::lsp::document::load_doc_from_uri;
                load_doc_from_uri(&uri)
                    .map(|doc| inlay_hints_for(&doc))
                    .unwrap_or_default()
            };

            let result = serde_json::to_value(hints)?;
            connection.sender.send(Message::Response(Response {
                id: req.id,
                result: Some(result),
                error: None,
            }))?;
        }
        SignatureHelpRequest::METHOD => {
            let params: lsp_types::SignatureHelpParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("invalid {}: {e}", SignatureHelpRequest::METHOD);
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

            let help: Option<SignatureHelp> = if let Some(doc) = store.get(&uri) {
                lsp_position_to_offset(&pos, &doc.text)
                    .and_then(|offset| signature_help_at(doc, offset))
            } else {
                use crate::lsp::document::load_doc_from_uri;
                load_doc_from_uri(&uri).and_then(|doc| {
                    lsp_position_to_offset(&pos, &doc.text)
                        .and_then(|offset| signature_help_at(&doc, offset))
                })
            };

            let result = serde_json::to_value(help)?;
            connection.sender.send(Message::Response(Response {
                id: req.id,
                result: Some(result),
                error: None,
            }))?;
        }
        WorkspaceSymbolRequest::METHOD => {
            let params: WorkspaceSymbolParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("invalid {}: {e}", WorkspaceSymbolRequest::METHOD);
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

            let query_lower = params.query.to_lowercase();

            // Collect symbols from all open documents.
            let mut symbols = Vec::new();
            for (uri, doc) in store.docs_iter() {
                symbols.extend(workspace_symbols_for(doc, uri, &query_lower));
            }

            // When no open documents, return null (empty result).
            let result = if symbols.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::to_value(WorkspaceSymbolResponse::Nested(symbols))?
            };
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
        .map(|doc| diagnostics_for(doc, uri, store.eval_ctx()))
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
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[x: 42]".to_string());
        let doc = store.get(&uri).unwrap();
        let hover = hover_at(doc, &uri, 4, &store.include_graph, store.eval_ctx()); // on '42'
        assert!(hover.is_some());
        assert!(hover.unwrap().contains("Int"));
    }

    #[test]
    fn test_handle_hover_no_document() {
        let store = DocumentStore::new().expect("DocumentStore::new in test");
        let uri = "file:///missing.llt".parse::<Uri>().unwrap();
        // If document doesn't exist, hover should return None.
        assert!(store.get(&uri).is_none());
    }

    #[test]
    fn test_diagnostics_published_on_parse_error() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[unterminated".to_string());
        let doc = store.get(&uri).unwrap();
        let diags = diagnostics_for(doc, &uri, store.eval_ctx());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn test_diagnostics_empty_for_valid_doc() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[x: 42]".to_string());
        let doc = store.get(&uri).unwrap();
        let diags = diagnostics_for(doc, &uri, store.eval_ctx());
        assert!(diags.is_empty());
    }

    #[test]
    fn test_document_update_replaces_content() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[x: 1]".to_string());
        store.update_document(uri.clone(), "[x: 2]".to_string());
        let doc = store.get(&uri).unwrap();
        assert_eq!(doc.text, "[x: 2]");
    }

    #[test]
    fn test_document_close_removes() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
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

    // --- rename_at tests (via analysis layer) ---

    #[test]
    fn test_rename_produces_workspace_edit() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[x: 1  y: $x]".to_string());
        let doc = store.get(&uri).unwrap();

        // Cursor on "$x" at offset 11
        let edits = rename_at(doc, 11, "newname");
        assert!(edits.is_some(), "should produce edits");
        let edits = edits.unwrap();
        assert!(!edits.is_empty());
        for edit in &edits {
            assert_eq!(edit.new_text, "newname");
        }
    }

    #[test]
    fn test_rename_invalid_name() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[x: 1  y: $x]".to_string());
        let doc = store.get(&uri).unwrap();
        // '@' is not a valid identifier character.
        let edits = rename_at(doc, 11, "in@valid");
        assert!(edits.is_none(), "invalid name should yield None");
    }

    // --- inlay_hints_for tests (via analysis layer) ---

    #[test]
    fn test_inlay_hints_emitted_for_typed_bindings() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[x: 42]".to_string());
        let doc = store.get(&uri).unwrap();
        let hints = inlay_hints_for(doc);
        // The binding "x: 42" should produce an inlay hint.
        assert!(
            !hints.is_empty(),
            "typed binding should get an inlay hint; got none"
        );
    }

    #[test]
    fn test_inlay_hints_none_for_parse_error() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[unterminated".to_string());
        let doc = store.get(&uri).unwrap();
        let hints = inlay_hints_for(doc);
        assert!(hints.is_empty());
    }

    // --- document_symbols_at tests ---

    #[test]
    fn test_document_symbols_simple() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[x: 1  y: 2]".to_string());
        let doc = store.get(&uri).unwrap();
        let syms = document_symbols_at(doc);
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].name, "x");
        assert_eq!(syms[1].name, "y");
    }

    #[test]
    fn test_document_symbols_annotated_key() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[x@Int: 42]".to_string());
        let doc = store.get(&uri).unwrap();
        let syms = document_symbols_at(doc);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "x");
    }

    #[test]
    fn test_document_symbols_empty_on_parse_error() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[unterminated".to_string());
        let doc = store.get(&uri).unwrap();
        let syms = document_symbols_at(doc);
        assert!(syms.is_empty());
    }

    #[test]
    fn test_document_symbols_non_dict_returns_empty() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        // A bare integer is not a dict — no symbols to extract.
        store.update_document(uri.clone(), "42".to_string());
        let doc = store.get(&uri).unwrap();
        let syms = document_symbols_at(doc);
        assert!(syms.is_empty());
    }

    // --- references_at tests ---

    #[test]
    fn test_references_at_finds_all_refs() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        // "[x: 1  y: $x  z: $x]"
        //  0         1         2
        //  0123456789012345678901
        //             ^ $x at 11, ^ $x at 18
        store.update_document(uri.clone(), "[x: 1  y: $x  z: $x]".to_string());
        let doc = store.get(&uri).unwrap();
        // Cursor on the first "$x" at offset 11
        let locs = references_at(doc, &uri, 11);
        assert_eq!(
            locs.len(),
            2,
            "should find both references to $x; got {locs:?}"
        );
        // All locations should reference the same document
        for loc in &locs {
            assert_eq!(loc.uri, uri);
        }
    }

    #[test]
    fn test_references_at_no_ref_at_offset() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[x: 1]".to_string());
        let doc = store.get(&uri).unwrap();
        // Offset 4 is on the integer literal '1', not a VarRef.
        let locs = references_at(doc, &uri, 4);
        assert!(locs.is_empty(), "int literal has no references");
    }

    #[test]
    fn test_references_at_parse_error_returns_empty() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let uri = "file:///test.llt".parse::<Uri>().unwrap();
        store.update_document(uri.clone(), "[unterminated".to_string());
        let doc = store.get(&uri).unwrap();
        let locs = references_at(doc, &uri, 1);
        assert!(locs.is_empty());
    }

    // --- document formatting tests ---

    #[test]
    fn test_formatting_produces_valid_text_edit() {
        // Verify that the Rust formatter (still used in formatter.rs unit tests) can
        // parse and reformat a simple document. The LSP path uses format_source_tinct,
        // but format_source is still available for unit testing.
        let source = "[x:1  y:2]";
        let result = crate::formatter::format_source(source);
        assert!(result.is_ok(), "formatter should succeed on valid source");
        let formatted = result.unwrap();
        // The formatter should produce something (possibly identical, possibly normalized).
        assert!(!formatted.is_empty());
    }

    #[test]
    fn test_formatting_end_position() {
        // Verify the end-position calculation used by the Formatting handler.
        let text = "line1\nline2\nline3";
        // 2 newlines → end.line = 2; "line3" is 5 chars → end.character = 5.
        let newline_count = text.bytes().filter(|&b| b == b'\n').count() as u32;
        let last_line_start = text.rfind('\n').map_or(0, |i| i + 1);
        let last_line_len = text[last_line_start..].len() as u32;
        assert_eq!(newline_count, 2);
        assert_eq!(last_line_len, 5);
    }
}
