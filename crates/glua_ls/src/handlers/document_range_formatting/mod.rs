mod external_range_format;

use glua_code_analysis::{FormattingOptions, range_format_code};
use lsp_types::{
    ClientCapabilities, DocumentRangeFormattingParams, OneOf, Position, Range, ServerCapabilities,
    TextEdit,
};
use tokio_util::sync::CancellationToken;

use crate::{
    context::ServerContextSnapshot,
    handlers::document_range_formatting::external_range_format::external_tool_range_format,
};

use super::RegisterCapabilities;

pub async fn on_range_formatting_handler(
    context: ServerContextSnapshot,
    params: DocumentRangeFormattingParams,
    cancel_token: CancellationToken,
) -> Option<Vec<TextEdit>> {
    let uri = params.text_document.uri;
    let request_range = params.range;
    let client_id = context
        .read_workspace_manager(&cancel_token)
        .await?
        .client_config
        .client_id;

    // Extract data under short-lived read lock, then drop before external await
    let (text_owned, normalized_path, formatting_options, external_tool) = {
        let analysis = context.read_analysis(&cancel_token).await?;
        let file_id = analysis.get_file_id(&uri)?;
        let syntax_tree = analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)?;

        if syntax_tree.has_syntax_errors() {
            return None;
        }
        let emmyrc = analysis.get_emmyrc();
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
        let ext = emmyrc.format.external_tool_range_format.clone();
        (text, normalized, opts, ext)
    };
    // analysis read lock is now dropped

    let formatted_result = if let Some(external_tool) = &external_tool {
        // Re-acquire briefly for document access needed by external_tool_range_format
        let analysis = context.read_analysis(&cancel_token).await?;
        let file_id = analysis.get_file_id(&uri)?;
        let document = analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_document(&file_id)?;
        external_tool_range_format(
            external_tool,
            &document,
            &request_range,
            &normalized_path,
            formatting_options,
        )
        .await?
    } else {
        range_format_code(
            &text_owned,
            &normalized_path,
            request_range.start.line as i32,
            0,
            request_range.end.line as i32 + 1,
            0,
            formatting_options,
        )?
    };

    let mut formatted_text = formatted_result.text;
    if client_id.is_intellij() || client_id.is_other() {
        formatted_text = formatted_text.replace("\r\n", "\n");
    }

    let text_edit = TextEdit {
        range: Range {
            start: Position {
                line: formatted_result.start_line as u32,
                character: formatted_result.start_col as u32,
            },
            end: Position {
                line: formatted_result.end_line as u32 + 1,
                character: 0,
            },
        },
        new_text: formatted_text,
    };

    Some(vec![text_edit])
}

pub struct DocumentRangeFormattingCapabilities;

impl RegisterCapabilities for DocumentRangeFormattingCapabilities {
    fn register_capabilities(server_capabilities: &mut ServerCapabilities, _: &ClientCapabilities) {
        server_capabilities.document_range_formatting_provider = Some(OneOf::Left(true));
    }
}
