use glua_code_analysis::SemanticModel;
use glua_parser::LuaStringToken;
use lsp_types::{GotoDefinitionResponse, Location};

use crate::handlers::document_link::linked_file;

pub fn goto_module_file(
    semantic_model: &SemanticModel,
    string_token: LuaStringToken,
) -> Option<GotoDefinitionResponse> {
    let file_id = linked_file(
        semantic_model.get_db(),
        semantic_model.get_file_id(),
        &string_token,
    )?;
    let document = semantic_model.get_document_by_file_id(file_id)?;
    let uri = document.get_uri();
    // Make sure the target file exists
    let file_path = document.get_file_path();
    if !file_path.try_exists().unwrap_or(false) {
        return None;
    }

    let lsp_range = document.get_document_lsp_range();

    Some(GotoDefinitionResponse::Scalar(Location {
        uri,
        range: lsp_range,
    }))
}
