use crate::context::ServerContextSnapshot;

use super::hover_expand_request::{
    HoverExpandParams, HoverExpandResponse, compute_max_level_at_position, level_to_display_count,
};

use glua_code_analysis::RenderLevel;
use tokio_util::sync::CancellationToken;

pub async fn on_hover_expand_handler(
    context: ServerContextSnapshot,
    params: HoverExpandParams,
    cancel_token: CancellationToken,
) -> Option<HoverExpandResponse> {
    if cancel_token.is_cancelled() {
        return None;
    }

    let uri = params.text_document.uri;
    let position = params.position;
    let level = params.level.unwrap_or(0);

    let analysis = context.read_analysis(&cancel_token).await?;
    if cancel_token.is_cancelled() {
        return None;
    }

    let file_id = analysis.get_file_id(&uri)?;
    let semantic_model = analysis.compilation.get_semantic_model(file_id)?;
    if !semantic_model.get_emmyrc().hover.enable {
        return None;
    }

    // Compute max level for the symbol at this position.
    let max_level = compute_max_level_at_position(&semantic_model, position);

    // Map verbosity level to a display count and create the render level.
    let display_count = level_to_display_count(level);
    let render_level = RenderLevel::DetailedCount(display_count);

    // Reuse the existing hover pipeline with a custom render level.
    let hover = super::hover(&analysis, file_id, position, Some(render_level))?;

    Some(HoverExpandResponse {
        content: hover.contents,
        range: hover.range,
        max_level,
    })
}
