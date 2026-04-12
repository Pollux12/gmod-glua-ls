mod build_function_hint;
mod build_inlay_hint;

use super::RegisterCapabilities;
use crate::context::{ClientId, ServerContextSnapshot};
use build_inlay_hint::build_inlay_hints;
pub use build_inlay_hint::{get_override_lsp_location, get_super_member_id};
use glua_code_analysis::{EmmyLuaAnalysis, FileId};
use lsp_types::{
    ClientCapabilities, InlayHint, InlayHintOptions, InlayHintParams, InlayHintServerCapabilities,
    OneOf, ServerCapabilities,
};
use tokio_util::sync::CancellationToken;

pub async fn on_inlay_hint_handler(
    context: ServerContextSnapshot,
    params: InlayHintParams,
    cancel_token: CancellationToken,
) -> Option<Vec<InlayHint>> {
    if cancel_token.is_cancelled() {
        return None;
    }

    let uri = params.text_document.uri;

    if !context
        .wait_until_latest_document_version_applied(&uri, &cancel_token)
        .await
    {
        return None;
    }

    // Wait for any pending reindex to finish so we serve fresh hints
    // computed against consistent tree + index data.
    if !context
        .debounced_analysis()
        .wait_until_fresh(&cancel_token)
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

        inlay_hint(
            &analysis,
            analysis.get_file_id(&uri)?,
            client_id,
            &cancel_token,
        )
    };

    result
}

pub fn inlay_hint(
    analysis: &EmmyLuaAnalysis,
    file_id: FileId,
    client_id: ClientId,
    cancel_token: &CancellationToken,
) -> Option<Vec<InlayHint>> {
    let semantic_model = analysis.compilation.get_semantic_model(file_id)?;
    build_inlay_hints(&semantic_model, client_id, cancel_token)
}

#[allow(unused_variables)]
pub async fn on_resolve_inlay_hint(
    context: ServerContextSnapshot,
    inlay_hint: InlayHint,
    cancel_token: CancellationToken,
) -> InlayHint {
    inlay_hint
}

pub struct InlayHintCapabilities;

impl RegisterCapabilities for InlayHintCapabilities {
    fn register_capabilities(server_capabilities: &mut ServerCapabilities, _: &ClientCapabilities) {
        server_capabilities.inlay_hint_provider = Some(OneOf::Right(
            InlayHintServerCapabilities::Options(InlayHintOptions {
                resolve_provider: Some(false),
                work_done_progress_options: Default::default(),
            }),
        ));
    }
}
