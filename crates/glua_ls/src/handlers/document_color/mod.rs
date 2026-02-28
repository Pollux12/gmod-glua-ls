mod build_color;

use build_color::{build_colors, convert_color_to_hex};
use glua_parser::LuaAstNode;
use lsp_types::{
    ClientCapabilities, Color, ColorInformation, ColorPresentation, ColorPresentationParams,
    ColorProviderCapability, DocumentColorParams, ServerCapabilities, TextEdit,
};
use tokio_util::sync::CancellationToken;

use crate::context::ServerContextSnapshot;

use super::RegisterCapabilities;

/// Converts a color picker result back to a GMod `Color(r, g, b[, a])` constructor string.
/// Preserves the original 3-arg vs 4-arg form based on the source text.
fn convert_color_to_gmod(color: Color, original_text: &str) -> String {
    let r = (color.red * 255.0).round() as u8;
    let g = (color.green * 255.0).round() as u8;
    let b = (color.blue * 255.0).round() as u8;
    let comma_count = original_text.chars().filter(|&c| c == ',').count();
    if comma_count >= 3 {
        let a = (color.alpha * 255.0).round() as u8;
        format!("Color({r}, {g}, {b}, {a})")
    } else {
        format!("Color({r}, {g}, {b})")
    }
}

pub async fn on_document_color(
    context: ServerContextSnapshot,
    params: DocumentColorParams,
    cancel_token: CancellationToken,
) -> Vec<ColorInformation> {
    if cancel_token.is_cancelled() {
        return vec![];
    }
    let uri = params.text_document.uri;
    let analysis = context.analysis().read().await;
    if cancel_token.is_cancelled() {
        return vec![];
    }
    let file_id = if let Some(file_id) = analysis.get_file_id(&uri) {
        file_id
    } else {
        return vec![];
    };

    let semantic_model =
        if let Some(semantic_model) = analysis.compilation.get_semantic_model(file_id) {
            semantic_model
        } else {
            return vec![];
        };

    if !semantic_model.get_emmyrc().document_color.enable {
        return vec![];
    }

    let document = semantic_model.get_document();
    let root = semantic_model.get_root();
    build_colors(root.syntax().clone(), &document)
}

pub async fn on_document_color_presentation(
    context: ServerContextSnapshot,
    params: ColorPresentationParams,
    _: CancellationToken,
) -> Vec<ColorPresentation> {
    let uri = params.text_document.uri;
    let analysis = context.analysis().read().await;
    let file_id = if let Some(file_id) = analysis.get_file_id(&uri) {
        file_id
    } else {
        return vec![];
    };

    let semantic_model =
        if let Some(semantic_model) = analysis.compilation.get_semantic_model(file_id) {
            semantic_model
        } else {
            return vec![];
        };
    let document = semantic_model.get_document();

    let range = if let Some(range) = document.to_rowan_range(params.range) {
        range
    } else {
        return vec![];
    };
    let color = params.color;
    let text = document.get_text_slice(range);
    let color_text = if text.starts_with("Color(") {
        convert_color_to_gmod(color, text)
    } else {
        convert_color_to_hex(color, text.len())
    };
    let color_presentations = vec![ColorPresentation {
        label: text.to_string(),
        text_edit: Some(TextEdit {
            range: params.range,
            new_text: color_text,
        }),
        additional_text_edits: None,
    }];

    color_presentations
}

pub struct DocumentColorCapabilities;

impl RegisterCapabilities for DocumentColorCapabilities {
    fn register_capabilities(server_capabilities: &mut ServerCapabilities, _: &ClientCapabilities) {
        server_capabilities.color_provider = Some(ColorProviderCapability::Simple(true));
    }
}
