use glua_code_analysis::{
    DeferredVfsDrop, DiagnosticCode, Emmyrc, FileId, fetch_schema_urls, read_file_with_encoding,
    uri_to_file_path,
};
use glua_parser::{LineIndex, LuaParseError, LuaParseErrorKind, LuaParser, LuaSyntaxTree};
use lsp_types::{
    Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, NumberOrString, PublishDiagnosticsParams,
};
use rowan::{NodeCache, TextRange};
use std::sync::Arc;
use std::time::Duration;

use crate::context::{ServerContextSnapshot, WorkspaceDiagnosticLevel};

struct PreparsedDocument {
    tree: LuaSyntaxTree,
    line_index: LineIndex,
    syntax_diagnostics: Vec<Diagnostic>,
}

fn spawn_deferred_drop(deferred_drop: DeferredVfsDrop) {
    tokio::task::spawn_blocking(move || drop(deferred_drop));
}

fn should_drop_stale_version(
    context: &ServerContextSnapshot,
    uri: &lsp_types::Uri,
    version: i32,
) -> bool {
    context.has_newer_seen_document_version(uri, version)
}

async fn apply_document_update_without_queuing(
    context: &ServerContextSnapshot,
    uri: &lsp_types::Uri,
    text: String,
    version: i32,
    mut preparsed: Option<PreparsedDocument>,
    trigger_reindex: bool,
) -> Option<FileId> {
    if should_drop_stale_version(context, uri, version) {
        return None;
    }

    // Fair-queued `write().await`, not a `try_write` spin, which can starve
    // for seconds under a stream of readers.
    let mut analysis = context.analysis().write().await;

    // The lock wait is unbounded, so re-check staleness now that we hold it.
    if should_drop_stale_version(context, uri, version) {
        return None;
    }

    let (file_id, deferred_drop) = if let Some(preparsed) = preparsed.take() {
        if trigger_reindex {
            (
                analysis.update_file_preparsed(
                    uri.clone(),
                    Some(text),
                    preparsed.tree,
                    preparsed.line_index,
                    Some(version),
                    true,
                ),
                None,
            )
        } else {
            let (file_id, deferred_drop) = analysis.update_file_preparsed_deferred(
                uri.clone(),
                Some(text),
                preparsed.tree,
                preparsed.line_index,
                Some(version),
            )?;
            (Some(file_id), Some(deferred_drop))
        }
    } else if trigger_reindex {
        (analysis.update_file_by_uri(uri, Some(text)), None)
    } else {
        (analysis.update_file_text_only(uri, text), None)
    };

    // Text-only updates leave the index alone; the debounced reindex
    // invalidates under its own write lock.
    if file_id.is_some() && trigger_reindex {
        context
            .file_diagnostic()
            .invalidate_shared_diagnostic_data();
    }
    drop(analysis);

    if let Some(deferred_drop) = deferred_drop {
        spawn_deferred_drop(deferred_drop);
    }

    file_id
}

async fn check_schema_update(context: &ServerContextSnapshot) {
    let urls = {
        let read_analysis = context.analysis().read().await;
        if !read_analysis.check_schema_update() {
            return;
        }

        read_analysis.get_schemas_to_fetch()
    };

    if urls.is_empty() {
        return;
    }

    let url_contents = fetch_schema_urls(urls).await;

    let mut write_analysis = context.analysis().write().await;
    write_analysis.apply_fetched_schemas(url_contents);
    context
        .file_diagnostic()
        .invalidate_shared_diagnostic_data();
}

async fn preparse_document(text: String, emmyrc: Arc<Emmyrc>) -> Option<PreparsedDocument> {
    let emmyrc_for_parse = emmyrc.clone();
    let parsed = tokio::task::spawn_blocking(move || {
        let mut node_cache = NodeCache::default();
        let line_index = LineIndex::parse(&text);
        let parse_config = emmyrc_for_parse.get_parse_config(&mut node_cache);
        let tree = LuaParser::parse(&text, parse_config);
        let parse_errors = tree.get_errors().to_vec();
        (tree, line_index, parse_errors, text)
    })
    .await;

    let (tree, line_index, parse_errors, source_text) = match parsed {
        Ok(parsed) => parsed,
        Err(err) => {
            log::error!("failed to preparse text document: {}", err);
            return None;
        }
    };

    let syntax_diagnostics =
        build_syntax_diagnostics(&parse_errors, &line_index, &source_text, emmyrc.as_ref());
    Some(PreparsedDocument {
        tree,
        line_index,
        syntax_diagnostics,
    })
}

fn build_syntax_diagnostics(
    parse_errors: &[LuaParseError],
    line_index: &LineIndex,
    source_text: &str,
    emmyrc: &Emmyrc,
) -> Vec<Diagnostic> {
    parse_errors
        .iter()
        .map(|error| {
            let code = match error.kind {
                LuaParseErrorKind::SyntaxError => DiagnosticCode::SyntaxError,
                LuaParseErrorKind::DocError => DiagnosticCode::DocSyntaxError,
            };

            let severity = emmyrc
                .diagnostics
                .severity
                .get(&code)
                .copied()
                .map(Into::into)
                .unwrap_or(DiagnosticSeverity::ERROR);

            Diagnostic {
                message: error.message.clone(),
                range: parse_error_range_to_lsp_range(error.range, line_index, source_text),
                severity: Some(severity),
                code: Some(NumberOrString::String(code.get_name().to_string())),
                source: Some("GLuaLS".into()),
                ..Default::default()
            }
        })
        .collect()
}

fn parse_error_range_to_lsp_range(
    range: TextRange,
    line_index: &LineIndex,
    source_text: &str,
) -> lsp_types::Range {
    let (start_line, start_character) = line_index
        .get_line_col(range.start(), source_text)
        .unwrap_or((0, 0));
    let (end_line, end_character) = line_index
        .get_line_col(range.end(), source_text)
        .unwrap_or((start_line, start_character));

    lsp_types::Range {
        start: lsp_types::Position {
            line: start_line as u32,
            character: start_character as u32,
        },
        end: lsp_types::Position {
            line: end_line as u32,
            character: end_character as u32,
        },
    }
}

pub async fn on_did_open_text_document(
    context: ServerContextSnapshot,
    params: DidOpenTextDocumentParams,
) -> Option<()> {
    let uri = params.text_document.uri;
    let text = params.text_document.text;
    let version = params.text_document.version;
    let supports_pull = context.lsp_features().supports_pull_diagnostic();

    // Check if file should be filtered before acquiring locks
    // Follow lock order: workspace_manager (read) -> analysis (write)
    let should_process = {
        let analysis = context.analysis().read().await;
        let old_file_id = analysis.get_file_id(&uri);
        if old_file_id.is_some() {
            true
        } else {
            drop(analysis);
            let workspace_manager = context.workspace_manager().read().await;
            workspace_manager.is_workspace_file(&uri)
        }
    };

    if !should_process {
        context.mark_document_closed(&uri);
        return None;
    }

    if should_drop_stale_version(&context, &uri, version) {
        return Some(());
    }

    let emmyrc = {
        let analysis = context.analysis().read().await;
        analysis.get_emmyrc()
    };
    let interval = emmyrc.diagnostics.diagnostic_interval.unwrap_or(500);
    let preparsed = preparse_document(text.clone(), emmyrc).await;
    if should_drop_stale_version(&context, &uri, version) {
        return Some(());
    }

    let diagnostics = preparsed
        .as_ref()
        .map_or_else(Vec::new, |parsed| parsed.syntax_diagnostics.clone());

    let file_id =
        apply_document_update_without_queuing(&context, &uri, text, version, preparsed, true).await;
    if file_id.is_some() {
        context.note_document_applied_version(&uri, version);
        if context.lsp_features().supports_semantic_tokens_refresh() {
            context.client().refresh_semantic_tokens();
        }
    }

    if !supports_pull && file_id.is_some() {
        context
            .client()
            .publish_diagnostics(PublishDiagnosticsParams {
                uri: uri.clone(),
                diagnostics,
                version: Some(version),
            });
    }

    // Schedule diagnostic task without holding any locks
    if !supports_pull {
        if let Some(file_id) = file_id {
            context
                .file_diagnostic()
                .add_diagnostic_task(file_id, interval, Some(context.debounced_analysis_arc()))
                .await;
        }
    }

    Some(())
}

pub async fn on_did_save_text_document(
    context: ServerContextSnapshot,
    _: DidSaveTextDocumentParams,
) -> Option<()> {
    let emmyrc = context.analysis().read().await.get_emmyrc();
    if !emmyrc.workspace.enable_reindex {
        if context.lsp_features().supports_workspace_diagnostic() {
            context
                .file_diagnostic()
                .cancel_workspace_diagnostic()
                .await;

            {
                let workspace_manager = context.workspace_manager().write().await;
                workspace_manager.update_workspace_version(WorkspaceDiagnosticLevel::Slow, true);
            }

            check_schema_update(&context).await;
        }

        return Some(());
    }

    let mut duration = emmyrc.workspace.reindex_duration;
    // if duration is less than 1000ms, set it to 1000ms
    if duration < 1000 {
        duration = 1000;
    }
    {
        let workspace = context.workspace_manager().read().await;
        workspace
            .reindex_workspace(Duration::from_millis(duration))
            .await;
    }

    check_schema_update(&context).await;
    Some(())
}

pub async fn on_did_change_text_document(
    context: ServerContextSnapshot,
    params: DidChangeTextDocumentParams,
) -> Option<()> {
    let uri = params.text_document.uri;
    let text = params.content_changes.first()?.text.clone();
    let version = params.text_document.version;
    let supports_pull = context.lsp_features().supports_pull_diagnostic();

    // Single read-lock acquisition: get file_id + emmyrc + should_process
    let (existing_file_id, emmyrc, should_process) = {
        let analysis = context.analysis().read().await;
        let file_id = analysis.get_file_id(&uri);
        let emmyrc = analysis.get_emmyrc();
        if file_id.is_some() {
            (file_id, emmyrc, true)
        } else {
            drop(analysis);
            let workspace_manager = context.workspace_manager().read().await;
            let should = workspace_manager.is_workspace_file(&uri);
            (file_id, emmyrc, should)
        }
    };

    // Cancel outstanding diagnostics immediately for this file
    if let Some(file_id) = existing_file_id {
        context
            .file_diagnostic()
            .cancel_file_diagnostic(file_id)
            .await;
    }

    if !should_process {
        context.mark_document_closed(&uri);
        return None;
    }

    if should_drop_stale_version(&context, &uri, version) {
        return Some(());
    }

    let interval = emmyrc.diagnostics.diagnostic_interval.unwrap_or(500);
    let preparsed = preparse_document(text.clone(), emmyrc.clone()).await;
    let syntax_diagnostics = preparsed
        .as_ref()
        .map_or_else(Vec::new, |parsed| parsed.syntax_diagnostics.clone());
    if should_drop_stale_version(&context, &uri, version) {
        return Some(());
    }

    let file_id =
        apply_document_update_without_queuing(&context, &uri, text, version, preparsed, false)
            .await;
    if file_id.is_some() {
        context.note_document_applied_version(&uri, version);
    }

    if should_drop_stale_version(&context, &uri, version) {
        return Some(());
    }

    if !supports_pull && file_id.is_some() {
        let diagnostics = context
            .file_diagnostic()
            .cached_file_diagnostics(&uri)
            .await
            .unwrap_or(syntax_diagnostics);
        context
            .client()
            .publish_diagnostics(PublishDiagnosticsParams {
                uri: uri.clone(),
                diagnostics,
                version: Some(version),
            });
    }

    // Schedule debounced reindex — rapid edits into a single reindex
    if let Some(file_id) = file_id {
        context.debounced_analysis().schedule(file_id).await;
    }

    // Handle reindex without holding locks
    if emmyrc.workspace.enable_reindex {
        let workspace = context.workspace_manager().read().await;
        workspace.extend_reindex_delay().await;
    }

    // Schedule diagnostic task
    if !supports_pull {
        if let Some(file_id) = file_id {
            context
                .file_diagnostic()
                .add_diagnostic_task(file_id, interval, Some(context.debounced_analysis_arc()))
                .await;
        }
    }

    Some(())
}

pub async fn on_did_close_document(
    context: ServerContextSnapshot,
    params: DidCloseTextDocumentParams,
) -> Option<()> {
    let uri = &params.text_document.uri;
    let lsp_features = context.lsp_features();

    // A closed document has no reader for its cached replay report.
    if lsp_features.supports_pull_diagnostic() {
        context
            .file_diagnostic()
            .forget_cached_file_diagnostics(uri)
            .await;
    }

    let (encoding, interval) = {
        let analysis = context.analysis().read().await;
        let emmyrc = analysis.get_emmyrc();
        (
            emmyrc.workspace.encoding.clone(),
            emmyrc.diagnostics.diagnostic_interval.unwrap_or(500),
        )
    };

    // Only remove from the index when the file no longer exists on disk
    // (e.g. it was deleted while open). Files that still exist on disk —
    // including library/annotation files opened via "Go to Definition" —
    // must stay in the index, but their in-memory contents need to revert
    // to the on-disk state once the editor buffer closes.
    if let Some(file_path) = uri_to_file_path(uri) {
        if file_path.exists() {
            if let Some(text) = read_file_with_encoding(&file_path, &encoding) {
                if !context.is_document_closed(uri) {
                    return Some(());
                }

                let file_id = {
                    let mut analysis = context.analysis().write().await;
                    if !context.is_document_closed(uri) {
                        return Some(());
                    }
                    let file_id = analysis.update_file_by_uri(uri, Some(text));
                    if file_id.is_some() {
                        context
                            .file_diagnostic()
                            .invalidate_shared_diagnostic_data();
                    }
                    file_id
                };

                if !lsp_features.supports_pull_diagnostic()
                    && let Some(file_id) = file_id
                {
                    if !context.is_document_closed(uri) {
                        return Some(());
                    }
                    context
                        .file_diagnostic()
                        .add_diagnostic_task(
                            file_id,
                            interval,
                            Some(context.debounced_analysis_arc()),
                        )
                        .await;
                }
            }
        } else {
            if !context.is_document_closed(uri) {
                return Some(());
            }
            let mut mut_analysis = context.analysis().write().await;
            if !context.is_document_closed(uri) {
                return Some(());
            }
            mut_analysis.remove_file_by_uri(uri);
            context
                .file_diagnostic()
                .invalidate_shared_diagnostic_data();
            drop(mut_analysis);

            if !lsp_features.supports_pull_diagnostic() {
                context
                    .file_diagnostic()
                    .clear_push_file_diagnostics(uri.clone())
                    .await;
            }
        }
    }

    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ServerContext;
    use googletest::prelude::*;
    use lsp_server::Connection;
    use lsp_types::{
        ClientCapabilities, SemanticTokensWorkspaceClientCapabilities, TextDocumentItem, Uri,
        WorkspaceClientCapabilities,
    };
    use std::str::FromStr;
    use std::time::Duration;

    #[gtest]
    fn test_on_did_open_text_document_requests_semantic_tokens_refresh() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (proxy_connection, peer_connection) = Connection::memory();
        let capabilities = ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                semantic_tokens: Some(SemanticTokensWorkspaceClientCapabilities {
                    refresh_support: Some(true),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        runtime.block_on(async {
            let context = ServerContext::new(proxy_connection, capabilities);
            let snapshot = context.snapshot();
            let uri = Uri::from_str("file:///test.lua").unwrap();

            // Manually add the file to analysis so `should_process` becomes true without dealing with paths
            snapshot
                .analysis()
                .write()
                .await
                .update_file_by_uri(&uri, Some("local x = 1".to_string()));

            on_did_open_text_document(
                snapshot.clone(),
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: uri.clone(),
                        language_id: "lua".to_string(),
                        version: 1,
                        text: "local x = 1".to_string(),
                    },
                },
            )
            .await;

            let mut found_refresh = false;
            for _ in 0..5 {
                if let Ok(message) = peer_connection
                    .receiver
                    .recv_timeout(Duration::from_secs(1))
                {
                    if let lsp_server::Message::Request(request) = message {
                        if request.method == "workspace/semanticTokens/refresh" {
                            found_refresh = true;
                            break;
                        }
                    }
                }
            }

            verify_that!(found_refresh, eq(true))?;

            Ok(())
        })
    }

    #[gtest]
    fn test_on_did_open_text_document_does_not_request_refresh_for_stale_version() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (proxy_connection, peer_connection) = Connection::memory();
        let capabilities = ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                semantic_tokens: Some(SemanticTokensWorkspaceClientCapabilities {
                    refresh_support: Some(true),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        runtime.block_on(async {
            let context = ServerContext::new(proxy_connection, capabilities);
            let snapshot = context.snapshot();
            let uri = Uri::from_str("file:///test.lua").unwrap();

            snapshot
                .analysis()
                .write()
                .await
                .update_file_by_uri(&uri, Some("local x = 1".to_string()));

            // Mark a newer version as seen so the version 1 is considered stale
            snapshot.note_document_seen_version(&uri, 2);

            on_did_open_text_document(
                snapshot.clone(),
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: uri.clone(),
                        language_id: "lua".to_string(),
                        version: 1,
                        text: "local x = 1".to_string(),
                    },
                },
            )
            .await;

            let mut found_refresh = false;
            for _ in 0..5 {
                if let Ok(message) = peer_connection
                    .receiver
                    .recv_timeout(Duration::from_millis(50))
                {
                    if let lsp_server::Message::Request(request) = message {
                        if request.method == "workspace/semanticTokens/refresh" {
                            found_refresh = true;
                            break;
                        }
                    }
                }
            }

            verify_that!(found_refresh, eq(false))?;

            Ok(())
        })
    }
}
