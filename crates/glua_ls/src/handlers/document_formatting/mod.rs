mod external_format;
mod format_diff;

use glua_code_analysis::{FormattingOptions, reformat_code};
use lsp_types::{
    ClientCapabilities, DocumentFormattingParams, OneOf, ServerCapabilities, TextEdit,
};
use tokio_util::sync::CancellationToken;

use crate::{
    context::ServerContextSnapshot, handlers::document_formatting::format_diff::format_diff,
};
pub use external_format::{FormattingRange, external_tool_format};

use super::RegisterCapabilities;

pub async fn on_formatting_handler(
    context: ServerContextSnapshot,
    params: DocumentFormattingParams,
    cancel_token: CancellationToken,
) -> Option<Vec<TextEdit>> {
    let uri = params.text_document.uri;
    if !context
        .wait_until_latest_document_version_applied(&uri, &cancel_token)
        .await
    {
        return None;
    }
    let client_id = context
        .read_workspace_manager(&cancel_token)
        .await?
        .client_config
        .client_id;

    // Extract everything we need from the analysis under a short-lived read
    // lock, then drop it BEFORE running the formatter.  This avoids blocking
    // didChange writes (and therefore all other readers) while an external
    // formatter process is running.
    let (text_owned, normalized_path, formatting_options, external_config, use_diff) = {
        let analysis = context.read_analysis(&cancel_token).await?;
        let emmyrc = analysis.get_emmyrc();

        let file_id = analysis.get_file_id(&uri)?;
        let syntax_tree = analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)?;

        if syntax_tree.has_syntax_errors() {
            return None;
        }

        let document = analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_document(&file_id)?;
        let text = document.get_text().to_owned();
        let file_path = document.get_file_path();
        let normalized = file_path.to_string_lossy().to_string().replace("\\", "/");
        let opts = FormattingOptions {
            indent_size: params.options.tab_size,
            use_tabs: !params.options.insert_spaces,
            insert_final_newline: params.options.insert_final_newline.unwrap_or(true),
            non_standard_symbol: !emmyrc.runtime.nonstandard_symbol.is_empty(),
        };
        let ext = emmyrc.format.external_tool.clone();
        let diff = emmyrc.format.use_diff;
        (text, normalized, opts, ext, diff)
    };
    // analysis read lock is now dropped

    let mut formatted_text = if let Some(external_config) = &external_config {
        external_tool_format(
            external_config,
            &text_owned,
            &normalized_path,
            None,
            formatting_options,
        )
        .await?
    } else {
        reformat_code(&text_owned, &normalized_path, formatting_options)
    };

    if client_id.is_intellij() || client_id.is_other() {
        formatted_text = formatted_text.replace("\r\n", "\n");
    }

    let replace_all_limit = 50;
    // Re-acquire read lock briefly for the diff computation which needs the document
    let text_edits = if use_diff {
        let analysis = context.read_analysis(&cancel_token).await?;
        let file_id = analysis.get_file_id(&uri)?;
        let document = analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_document(&file_id)?;
        format_diff(&text_owned, &formatted_text, &document, replace_all_limit)
    } else {
        // For replace-all, we just need the document range
        let analysis = context.read_analysis(&cancel_token).await?;
        let file_id = analysis.get_file_id(&uri)?;
        let document = analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_document(&file_id)?;
        let document_range = document.get_document_lsp_range();
        vec![TextEdit {
            range: document_range,
            new_text: formatted_text.to_string(),
        }]
    };

    Some(text_edits)
}

pub struct DocumentFormattingCapabilities;

impl RegisterCapabilities for DocumentFormattingCapabilities {
    fn register_capabilities(server_capabilities: &mut ServerCapabilities, _: &ClientCapabilities) {
        server_capabilities.document_formatting_provider = Some(OneOf::Left(true));
    }
}
