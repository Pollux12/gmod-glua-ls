mod add_completions;
mod completion_builder;
mod completion_data;
mod data;
mod providers;
mod resolve_completion;

pub use add_completions::get_index_alias_name;
use completion_builder::CompletionBuilder;
pub(crate) use completion_data::CompletionData;
#[cfg(test)]
pub(crate) use completion_data::CompletionDataType;
use glua_code_analysis::{EmmyLuaAnalysis, FileId};
use glua_parser::LuaAstNode;
use log::error;
use lsp_types::{
    ClientCapabilities, CompletionItem, CompletionOptions, CompletionOptionsCompletionItem,
    CompletionParams, CompletionResponse, CompletionTriggerKind, Position, ServerCapabilities,
};
use providers::add_completions;
use resolve_completion::resolve_completion;
use rowan::TokenAtOffset;
use tokio_util::sync::CancellationToken;

use crate::context::{ClientId, ServerContextSnapshot};

use super::RegisterCapabilities;

pub async fn on_completion_handler(
    context: ServerContextSnapshot,
    params: CompletionParams,
    cancel_token: CancellationToken,
) -> Option<CompletionResponse> {
    if cancel_token.is_cancelled() {
        return None;
    }

    let uri = params.text_document_position.text_document.uri;
    let position = params.text_document_position.position;

    // For completion, briefly wait for fresh data (up to 50ms) so the user
    // sees accurate results.  If the reindex takes longer, proceed with
    // whatever data is available — a slightly stale completion list is
    // better than a multi-second delay.
    {
        let fresh = tokio::select! {
            biased;
            _ = cancel_token.cancelled() => return None,
            result = context.debounced_analysis().wait_until_fresh(&cancel_token) => result,
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => false,
        };
        // If cancelled during wait, bail out
        if cancel_token.is_cancelled() {
            return None;
        }
        // `fresh` being false (timeout or cancel) is fine — we proceed
        let _ = fresh;
    }

    let analysis = context.read_analysis(&cancel_token).await?;

    if cancel_token.is_cancelled() {
        return None;
    }

    let file_id = analysis.get_file_id(&uri)?;
    let semantic_model = analysis.compilation.get_semantic_model(file_id)?;
    if !semantic_model.get_emmyrc().completion.enable {
        return None;
    }

    let result = completion(
        &analysis,
        file_id,
        position,
        params
            .context
            .map(|context| context.trigger_kind)
            .unwrap_or(CompletionTriggerKind::INVOKED),
        cancel_token,
    );

    result
}

pub fn completion(
    analysis: &EmmyLuaAnalysis,
    file_id: FileId,
    position: Position,
    trigger_kind: CompletionTriggerKind,
    cancel_token: CancellationToken,
) -> Option<CompletionResponse> {
    let semantic_model = analysis.compilation.get_semantic_model(file_id)?;
    if !semantic_model.get_emmyrc().completion.enable {
        return None;
    }

    let root = semantic_model.get_root();
    let position_offset = {
        let document = semantic_model.get_document();
        document.get_offset(position.line as usize, position.character as usize)?
    };

    if position_offset > root.syntax().text_range().end() {
        return None;
    }

    let token = match root.syntax().token_at_offset(position_offset) {
        TokenAtOffset::Single(token) => token,
        TokenAtOffset::Between(left, _) => left,
        TokenAtOffset::None => {
            return None;
        }
    };

    let mut builder = CompletionBuilder::new(
        token,
        semantic_model,
        cancel_token.clone(),
        trigger_kind,
        position_offset,
    );
    add_completions(&mut builder);
    if cancel_token.is_cancelled() {
        return None;
    }
    Some(CompletionResponse::Array(builder.get_completion_items()))
}

pub async fn on_completion_resolve_handler(
    context: ServerContextSnapshot,
    params: CompletionItem,
    cancel_token: CancellationToken,
) -> CompletionItem {
    let Some(wm) = context.read_workspace_manager(&cancel_token).await else {
        return params;
    };
    let client_id = wm.client_config.client_id;
    drop(wm);

    let Some(analysis) = context.read_analysis(&cancel_token).await else {
        return params;
    };

    completion_resolve(&analysis, params, client_id)
}

pub fn completion_resolve(
    analysis: &EmmyLuaAnalysis,
    params: CompletionItem,
    client_id: ClientId,
) -> CompletionItem {
    let mut completion_item = params;
    let db = analysis.compilation.get_db();
    if let Some(data) = completion_item.data.clone() {
        let completion_data = match serde_json::from_value::<CompletionData>(data.clone()) {
            Ok(data) => data,
            Err(err) => {
                error!("Failed to deserialize completion data: {:?}", err);
                return completion_item;
            }
        };
        let semantic_model = analysis
            .compilation
            .get_semantic_model(completion_data.field_id);
        if let Some(semantic_model) = semantic_model {
            resolve_completion(
                &analysis.compilation,
                &semantic_model,
                db,
                &mut completion_item,
                completion_data,
                client_id,
            );
        }
    }
    completion_item
}

pub struct CompletionCapabilities;

impl RegisterCapabilities for CompletionCapabilities {
    fn register_capabilities(server_capabilities: &mut ServerCapabilities, _: &ClientCapabilities) {
        server_capabilities.completion_provider = Some(CompletionOptions {
            resolve_provider: Some(true),
            trigger_characters: Some(
                ['.', ':', '(', '[', '"', '\'', ' ', '@', '\\', '/', '|']
                    .into_iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            work_done_progress_options: Default::default(),
            completion_item: Some(CompletionOptionsCompletionItem {
                label_details_support: Some(true),
            }),
            all_commit_characters: Default::default(),
        });
    }
}
