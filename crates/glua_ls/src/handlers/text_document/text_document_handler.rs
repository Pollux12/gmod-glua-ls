use glua_code_analysis::{DiagnosticCode, Emmyrc, fetch_schema_urls, uri_to_file_path};
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
        return None;
    }

    let emmyrc = {
        let analysis = context.analysis().read().await;
        analysis.get_emmyrc()
    };
    let interval = emmyrc.diagnostics.diagnostic_interval.unwrap_or(500);
    let preparsed = preparse_document(text.clone(), emmyrc).await;

    if !supports_pull {
        let diagnostics = preparsed
            .as_ref()
            .map_or_else(Vec::new, |parsed| parsed.syntax_diagnostics.clone());
        context
            .client()
            .publish_diagnostics(PublishDiagnosticsParams {
                uri: uri.clone(),
                diagnostics,
                version: Some(version),
            });
    }

    let file_id = {
        let mut analysis = context.analysis().write().await;
        if let Some(preparsed) = preparsed {
            analysis.update_file_preparsed(
                uri.clone(),
                Some(text),
                preparsed.tree,
                preparsed.line_index,
                Some(version),
                true,
            )
        } else {
            analysis.update_file_by_uri(&uri, Some(text))
        }
    };

    // Schedule diagnostic task without holding any locks
    if !supports_pull {
        if let Some(file_id) = file_id {
            context
                .file_diagnostic()
                .add_diagnostic_task(file_id, interval, Some(context.debounced_analysis_arc()))
                .await;
        }
    }

    // Update open files list
    {
        let mut workspace = context.workspace_manager().write().await;
        workspace.current_open_files.insert(uri);
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

    // Cancel outstanding diagnostics immediately for this file so long-running
    // tasks do not continue after a newer edit arrives.
    let existing_file_id = {
        let analysis = context.analysis().read().await;
        analysis.get_file_id(&uri)
    };
    if let Some(file_id) = existing_file_id {
        context
            .file_diagnostic()
            .cancel_file_diagnostic(file_id)
            .await;
    }

    // Check if file should be filtered before acquiring locks
    // Follow lock order: workspace_manager (read) -> analysis (write)
    let should_process = if existing_file_id.is_some() {
        true
    } else {
        let workspace_manager = context.workspace_manager().read().await;
        workspace_manager.is_workspace_file(&uri)
    };

    if !should_process {
        return None;
    }

    let emmyrc = {
        let analysis = context.analysis().read().await;
        analysis.get_emmyrc()
    };
    let interval = emmyrc.diagnostics.diagnostic_interval.unwrap_or(500);
    let preparsed = preparse_document(text.clone(), emmyrc.clone()).await;
    let syntax_diagnostics = preparsed
        .as_ref()
        .map_or_else(Vec::new, |parsed| parsed.syntax_diagnostics.clone());

    let file_id = {
        let mut analysis = context.analysis().write().await;
        if let Some(preparsed) = preparsed {
            analysis.update_file_preparsed(
                uri.clone(),
                Some(text),
                preparsed.tree,
                preparsed.line_index,
                Some(version),
                false,
            )
        } else {
            analysis.update_file_text_only(&uri, text)
        }
    };

    if !supports_pull && file_id.is_some() {
        context
            .client()
            .publish_diagnostics(PublishDiagnosticsParams {
                uri: uri.clone(),
                diagnostics: syntax_diagnostics,
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
    let mut workspace = context.workspace_manager().write().await;
    workspace
        .current_open_files
        .remove(&params.text_document.uri);
    drop(workspace);
    let lsp_features = context.lsp_features();

    // Only remove from the index when the file no longer exists on disk
    // (e.g. it was deleted while open). Files that still exist on disk —
    // including library/annotation files opened via "Go to Definition" —
    // must stay in the index.
    if let Some(file_path) = uri_to_file_path(uri)
        && !file_path.exists()
    {
        let mut mut_analysis = context.analysis().write().await;
        mut_analysis.remove_file_by_uri(uri);
        drop(mut_analysis);

        if !lsp_features.supports_pull_diagnostic() {
            context
                .file_diagnostic()
                .clear_push_file_diagnostics(uri.clone());
        }
    }

    Some(())
}
