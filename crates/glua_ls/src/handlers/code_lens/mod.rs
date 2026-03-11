mod build_code_lens;
mod resolve_code_lens;

use build_code_lens::build_code_lens;
use glua_code_analysis::{LuaDeclId, LuaMemberId};
use lsp_types::{
    ClientCapabilities, CodeLens, CodeLensOptions, CodeLensParams, ServerCapabilities, Uri,
};
use resolve_code_lens::resolve_code_lens;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::context::ServerContextSnapshot;

use super::RegisterCapabilities;

pub async fn on_code_lens_handler(
    context: ServerContextSnapshot,
    params: CodeLensParams,
    cancel_token: CancellationToken,
) -> Option<Vec<CodeLens>> {
    if cancel_token.is_cancelled() {
        return None;
    }

    // Wait for pending reindex work so VS Code keeps the current lenses visible
    // instead of clearing them during the dirty window, which causes layout flicker.
    if !context
        .debounced_analysis()
        .wait_until_fresh(&cancel_token)
        .await
    {
        return None;
    }

    let uri = params.text_document.uri;
    let analysis = context.read_analysis(&cancel_token).await?;
    let file_id = analysis.get_file_id(&uri)?;
    let semantic_model = analysis.compilation.get_semantic_model(file_id)?;

    if !semantic_model.get_emmyrc().code_lens.enable {
        return None;
    }

    build_code_lens(&semantic_model)
}

pub async fn on_resolve_code_lens_handler(
    context: ServerContextSnapshot,
    code_lens: CodeLens,
    cancel_token: CancellationToken,
) -> CodeLens {
    let client_id = {
        let Some(wm) = context.read_workspace_manager(&cancel_token).await else {
            return code_lens;
        };
        wm.client_config.client_id
    };
    let Some(analysis) = context.read_analysis(&cancel_token).await else {
        return code_lens;
    };
    let compilation = &analysis.compilation;

    resolve_code_lens(compilation, code_lens.clone(), client_id, &cancel_token).unwrap_or(code_lens)
}

#[derive(Debug, Serialize, Deserialize)]
pub enum CodeLensData {
    Member(LuaMemberId),
    DeclId(LuaDeclId),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CodeLensResolveData {
    #[serde(default)]
    pub uri: Option<Uri>,
    pub payload: CodeLensData,
}

pub struct CodeLensCapabilities;

impl RegisterCapabilities for CodeLensCapabilities {
    fn register_capabilities(server_capabilities: &mut ServerCapabilities, _: &ClientCapabilities) {
        server_capabilities.code_lens_provider = Some(CodeLensOptions {
            resolve_provider: Some(true),
        });
    }
}
