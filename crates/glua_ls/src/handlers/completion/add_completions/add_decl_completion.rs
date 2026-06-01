use glua_code_analysis::{DbIndex, LuaDeclId, LuaSemanticDeclId, LuaType};
use glua_parser::{LuaAstNode, LuaExpr, LuaSyntaxKind};
use lsp_types::CompletionItem;

use crate::handlers::completion::{
    add_completions::get_function_snippet,
    completion_builder::CompletionBuilder,
    completion_data::{CompletionColorInfo, CompletionData},
};

use super::{
    CallDisplay, check_visibility, color_label_detail,
    completion_item_info::{
        color_info_from_expr, color_info_from_type, gmod_constructor_literal_detail, is_color_type,
        is_gmod_literal_constructor_type, scalar_literal_description, scalar_literal_detail,
    },
    get_completion_tags, get_decl_completion_kind, get_description, get_detail, is_deprecated,
};

pub fn add_decl_completion(
    builder: &mut CompletionBuilder,
    decl_id: LuaDeclId,
    name: &str,
    typ: &LuaType,
) -> Option<()> {
    let property_owner = LuaSemanticDeclId::LuaDecl(decl_id);
    check_visibility(builder, property_owner.clone())?;

    let overload_count = count_function_overloads(builder.semantic_model.get_db(), typ);
    let (color, constructor_literal_detail) =
        get_decl_completion_literal_info(builder, decl_id, typ);
    let completion_data_color = builder
        .semantic_model
        .get_emmyrc()
        .document_color
        .enable
        .then(|| color.clone())
        .flatten();
    let literal_detail = scalar_literal_detail(typ);

    let mut completion_item = CompletionItem {
        label: name.to_string(),
        kind: Some(if color.is_some() {
            lsp_types::CompletionItemKind::COLOR
        } else {
            get_decl_completion_kind(builder, decl_id, typ)
        }),
        data: match completion_data_color {
            Some(color) => CompletionData::from_property_owner_id_with_color(
                builder,
                decl_id.into(),
                overload_count,
                color,
            ),
            None => CompletionData::from_property_owner_id(builder, decl_id.into(), overload_count),
        },
        label_details: Some(lsp_types::CompletionItemLabelDetails {
            detail: color
                .as_ref()
                .map(color_label_detail)
                .or_else(|| get_detail(builder, typ, CallDisplay::None))
                .or(constructor_literal_detail)
                .or(literal_detail),
            description: if color.is_some() {
                Some("Color".to_string())
            } else {
                scalar_literal_description(typ).or_else(|| get_description(builder, typ))
            },
        }),
        ..Default::default()
    };
    let deprecated = is_deprecated(builder, property_owner.clone());
    if deprecated {
        completion_item.deprecated = Some(true);
        completion_item.tags = get_completion_tags(builder, Some(true));
    }

    if builder.support_snippets(typ) {
        if let Some(snippet) = get_function_snippet(builder, name, typ, CallDisplay::None) {
            completion_item.insert_text = Some(snippet);
            completion_item.insert_text_format = Some(lsp_types::InsertTextFormat::SNIPPET);
        }
    }

    builder.add_completion_item(completion_item)?;
    Some(())
}

fn count_function_overloads(db: &DbIndex, typ: &LuaType) -> Option<usize> {
    let mut count = 0;
    match typ {
        LuaType::DocFunction(_) => {
            count += 1;
        }
        LuaType::Signature(id) => {
            count += 1;
            if let Some(signature) = db.get_signature_index().get(id) {
                count += signature.overloads.len();
            }
        }
        _ => {}
    }
    if count > 1 {
        count -= 1;
    }
    if count == 0 { None } else { Some(count) }
}

fn get_decl_completion_literal_info(
    builder: &CompletionBuilder,
    decl_id: LuaDeclId,
    typ: &LuaType,
) -> (Option<CompletionColorInfo>, Option<String>) {
    let mut color = color_info_from_type(typ);
    let should_inspect_color =
        color.is_none() && (is_color_type(typ) || matches!(typ, LuaType::Unknown));
    let should_inspect_constructor =
        is_gmod_literal_constructor_type(typ) || matches!(typ, LuaType::Unknown);
    if !should_inspect_color && !should_inspect_constructor {
        return (color, None);
    }

    let value_expr = get_decl_value_expr(builder, decl_id);
    if should_inspect_color {
        color = value_expr.as_ref().and_then(color_info_from_expr);
    }

    let constructor_literal_detail = if color.is_none() && should_inspect_constructor {
        value_expr
            .as_ref()
            .and_then(gmod_constructor_literal_detail)
    } else {
        None
    };

    (color, constructor_literal_detail)
}

fn get_decl_value_expr(builder: &CompletionBuilder, decl_id: LuaDeclId) -> Option<LuaExpr> {
    let decl = builder
        .semantic_model
        .get_db()
        .get_decl_index()
        .get_decl(&decl_id)?;
    let value_syntax_id = decl.get_value_syntax_id()?;
    if !can_expr_syntax_have_completion_literal(value_syntax_id.get_kind()) {
        return None;
    }
    let tree = builder
        .semantic_model
        .get_db()
        .get_vfs()
        .get_syntax_tree(&decl_id.file_id)?;
    let value_node = value_syntax_id.to_node_from_root(&tree.get_red_root())?;
    LuaExpr::cast(value_node)
}

fn can_expr_syntax_have_completion_literal(kind: LuaSyntaxKind) -> bool {
    matches!(
        kind,
        LuaSyntaxKind::CallExpr
            | LuaSyntaxKind::LiteralExpr
            | LuaSyntaxKind::RequireCallExpr
            | LuaSyntaxKind::AssertCallExpr
            | LuaSyntaxKind::ErrorCallExpr
            | LuaSyntaxKind::TypeCallExpr
            | LuaSyntaxKind::SetmetatableCallExpr
    )
}
