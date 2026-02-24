mod build_semantic_tokens;
mod function_string_highlight;
mod language_injector;
mod semantic_token_builder;

use crate::context::{ClientId, ServerContextSnapshot};
use build_semantic_tokens::build_semantic_tokens;
use emmylua_code_analysis::{EmmyLuaAnalysis, FileId};
use lsp_types::{
    ClientCapabilities, SemanticTokens, SemanticTokensFullOptions, SemanticTokensLegend,
    SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities,
};
#[allow(unused)]
pub use semantic_token_builder::{
    CustomSemanticTokenType, SEMANTIC_TOKEN_MODIFIERS, SEMANTIC_TOKEN_TYPES,
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
    let analysis = context.analysis().read().await;

    if cancel_token.is_cancelled() {
        return None;
    }

    let file_id = analysis.get_file_id(&uri)?;

    let workspace_manager = context.workspace_manager().read().await;
    let client_id = workspace_manager.client_config.client_id;
    let _ = workspace_manager;

    semantic_token(
        &analysis,
        file_id,
        context.lsp_features().supports_multiline_tokens(),
        client_id,
        &cancel_token,
    )
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
