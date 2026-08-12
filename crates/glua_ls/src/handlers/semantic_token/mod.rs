mod build_semantic_tokens;
mod escape_sequence_highlight;
mod function_string_highlight;
mod language_injector;
mod semantic_token_builder;

use crate::context::{ClientId, ServerContextSnapshot};
use build_semantic_tokens::build_semantic_tokens;
use glua_code_analysis::{EmmyLuaAnalysis, FileId};
use lsp_types::{
    ClientCapabilities, SemanticTokens, SemanticTokensFullOptions, SemanticTokensLegend,
    SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities,
};
#[allow(unused)]
pub use semantic_token_builder::{
    CustomSemanticTokenModifier, CustomSemanticTokenType, SEMANTIC_TOKEN_MODIFIERS,
    SEMANTIC_TOKEN_TYPES,
};
use tokio_util::sync::CancellationToken;

use super::RegisterCapabilities;

pub async fn on_semantic_token_handler(
    context: ServerContextSnapshot,
    params: SemanticTokensParams,
    cancel_token: CancellationToken,
) -> Option<SemanticTokensResult> {
    if cancel_token.is_cancelled() {
        return None;
    }

    let uri = params.text_document.uri;

    // Wait for the parse, but not for the reindex.
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

    let result = {
        // While we hold this read lock, no writes (VFS updates, reindex)
        // can proceed, so tree and index are guaranteed consistent.
        let analysis = context.read_analysis(&cancel_token).await?;

        if cancel_token.is_cancelled() {
            return None;
        }

        let file_id = analysis.get_file_id(&uri)?;

        semantic_token(
            &analysis,
            file_id,
            context.lsp_features().supports_multiline_tokens(),
            client_id,
            &cancel_token,
        )
    };

    result
}

pub fn semantic_token(
    analysis: &EmmyLuaAnalysis,
    file_id: FileId,
    supports_multiline_tokens: bool,
    client_id: ClientId,
    cancel_token: &CancellationToken,
) -> Option<SemanticTokensResult> {
    let semantic_model = analysis.compilation.get_semantic_model(file_id)?;
    let emmyrc = semantic_model.get_emmyrc();
    if !emmyrc.semantic_tokens.enable {
        return None;
    }

    let result = build_semantic_tokens(
        &semantic_model,
        supports_multiline_tokens,
        client_id,
        emmyrc,
        cancel_token,
    )?;

    Some(SemanticTokensResult::Tokens(SemanticTokens {
        result_id: None,
        data: result,
    }))
}

#[cfg(test)]
mod tests {
    use super::on_semantic_token_handler;
    use crate::context::ServerContext;
    use googletest::prelude::*;
    use lsp_server::Connection;
    use lsp_types::{
        ClientCapabilities, PartialResultParams, SemanticTokensParams, TextDocumentIdentifier, Uri,
        WorkDoneProgressParams,
    };
    use std::str::FromStr;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    /// The handler must not build tokens against a tree that predates the
    /// newest `didChange`. Token offsets are relative and carry no version,
    /// so a set built from a superseded tree paints every token after the
    /// edit onto the wrong word.
    #[gtest]
    fn does_not_answer_until_the_newest_document_version_is_applied() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        runtime.block_on(async {
            let (conn, _peer) = Connection::memory();
            let context = ServerContext::new(conn, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let uri = Uri::from_str("file:///semantic_token_gate.lua").expect("uri should parse");

            snapshot
                .analysis()
                .write()
                .await
                .update_file_by_uri(&uri, Some("local greeting = 1".to_string()));

            // Seen but not yet applied: exactly the window between a didChange
            // notification and the coalescer applying its preparsed tree.
            snapshot.note_document_seen_version(&uri, 2).await;

            let handler_snapshot = snapshot.clone();
            let params = SemanticTokensParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            };
            let handler = tokio::spawn(async move {
                on_semantic_token_handler(handler_snapshot, params, CancellationToken::new()).await
            });

            tokio::time::sleep(Duration::from_millis(10)).await;
            verify_that!(handler.is_finished(), eq(false))?;

            snapshot.note_document_applied_version(&uri, 2).await;

            tokio::time::timeout(Duration::from_secs(1), handler)
                .await
                .expect("handler should answer once the newest version is applied")
                .expect("handler should join successfully");
            Ok(())
        })
    }
}

pub struct SemanticTokenCapabilities;

impl RegisterCapabilities for SemanticTokenCapabilities {
    fn register_capabilities(
        server_capabilities: &mut ServerCapabilities,
        _client_capabilities: &ClientCapabilities,
    ) {
        server_capabilities.semantic_tokens_provider = Some(
            SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                legend: SemanticTokensLegend {
                    token_modifiers: SEMANTIC_TOKEN_MODIFIERS.to_vec(),
                    token_types: SEMANTIC_TOKEN_TYPES.to_vec(),
                },
                full: Some(SemanticTokensFullOptions::Bool(true)),
                ..Default::default()
            }),
        );
    }
}
