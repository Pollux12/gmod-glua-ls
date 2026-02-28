use glua_parser::{LuaAstNode, LuaComment, LuaSyntaxId, LuaTokenKind};
use lsp_types::SymbolKind;
use rowan::NodeOrToken;

use super::builder::{DocumentSymbolBuilder, LuaSymbol};

pub fn build_doc_region_symbol(
    builder: &mut DocumentSymbolBuilder,
    comment: LuaComment,
    parent_id: LuaSyntaxId,
) -> Option<LuaSyntaxId> {
    let mut region_token = None;
    for child in comment.syntax().children_with_tokens() {
        if let NodeOrToken::Token(token) = child {
            if token.kind() == LuaTokenKind::TkDocRegion.into() {
                region_token = Some(token);
                break;
            }
        }
    }

    let region_token = region_token?;

    let description = comment
        .get_description()
        .map(|desc| desc.get_description_text())
        .map(|text| {
            text.lines()
                .next()
                .map(|line| line.trim())
                .unwrap_or_default()
                .to_string()
        })
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "region".to_string());

    let range = comment.get_range();
    let selection_range = region_token.text_range();
    let symbol = LuaSymbol::with_selection_range(
        description,
        None,
        SymbolKind::NAMESPACE,
        range,
        selection_range,
    );

    let symbol_id = builder.add_node_symbol(comment.syntax().clone(), symbol, Some(parent_id));

    Some(symbol_id)
}
