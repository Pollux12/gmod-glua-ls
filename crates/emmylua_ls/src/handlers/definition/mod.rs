mod goto_def_definition;
mod goto_doc_see;
mod goto_function;
mod goto_module_file;
mod goto_path;

use emmylua_code_analysis::{
    EmmyLuaAnalysis, FileId, LuaCompilation, LuaType, SemanticDeclLevel, SemanticModel,
    WorkspaceId,
};
use emmylua_parser::{
    LuaAstNode, LuaAstToken, LuaCallArgList, LuaCallExpr, LuaDocDescription, LuaDocTagSee,
    LuaGeneralToken, LuaIndexExpr, LuaLiteralExpr, LuaStringToken, LuaTokenKind, PathTrait,
};
pub use goto_def_definition::goto_def_definition;
use goto_def_definition::goto_str_tpl_ref_definition;
pub use goto_doc_see::goto_doc_see;
pub use goto_function::compare_function_types;
pub use goto_module_file::goto_module_file;
use lsp_types::{
    ClientCapabilities, GotoDefinitionParams, GotoDefinitionResponse, Location, OneOf, Position,
    ServerCapabilities,
};
use rowan::TokenAtOffset;
use tokio_util::sync::CancellationToken;

use super::RegisterCapabilities;
use crate::context::ServerContextSnapshot;
use crate::handlers::definition::goto_function::goto_overload_function;
use crate::handlers::definition::goto_path::goto_path;
use crate::util::find_ref_at;

pub async fn on_goto_definition_handler(
    context: ServerContextSnapshot,
    params: GotoDefinitionParams,
    cancel_token: CancellationToken,
) -> Option<GotoDefinitionResponse> {
    if cancel_token.is_cancelled() {
        return None;
    }
    let uri = params.text_document_position_params.text_document.uri;
    let analysis = context.analysis().read().await;
    if cancel_token.is_cancelled() {
        return None;
    }
    let file_id = analysis.get_file_id(&uri)?;
    let position = params.text_document_position_params.position;

    definition(&analysis, file_id, position)
}

pub fn definition(
    analysis: &EmmyLuaAnalysis,
    file_id: FileId,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    let semantic_model = analysis.compilation.get_semantic_model(file_id)?;
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
        TokenAtOffset::Between(left, right) => {
            if left.kind() == LuaTokenKind::TkName.into()
                || (left.kind() == LuaTokenKind::TkLeftBracket.into()
                    && right.kind() == LuaTokenKind::TkInt.into())
            {
                left
            } else {
                right
            }
        }
        TokenAtOffset::None => {
            return None;
        }
    };

    if let Some(semantic_decl) =
        semantic_model.find_decl(token.clone().into(), SemanticDeclLevel::default())
    {
        return goto_def_definition(
            &semantic_model,
            &analysis.compilation,
            semantic_decl,
            &token,
        );
    } else if let Some(dynamic_field_response) =
        goto_inferred_dynamic_field_definition(&semantic_model, &token)
    {
        return Some(dynamic_field_response);
    } else if let Some(string_token) = LuaStringToken::cast(token.clone()) {
        if let Some(module_response) = goto_module_file(&semantic_model, string_token.clone()) {
            return Some(module_response);
        }
        if let Some(hook_response) = goto_hook_definition(
            &semantic_model,
            &analysis.compilation,
            file_id,
            position_offset,
            string_token.clone(),
        ) {
            return Some(hook_response);
        }
        if let Some(str_tpl_ref_response) =
            goto_str_tpl_ref_definition(&semantic_model, string_token)
        {
            return Some(str_tpl_ref_response);
        }
    } else if token.kind() == LuaTokenKind::TkDocSeeContent.into() {
        let general_token = LuaGeneralToken::cast(token.clone())?;
        if general_token.get_parent::<LuaDocTagSee>().is_some() {
            return goto_doc_see(
                &semantic_model,
                &analysis.compilation,
                general_token,
                position_offset,
            );
        }
    } else if token.kind() == LuaTokenKind::TkDocDetail.into() {
        let parent = token.parent()?;
        let description = LuaDocDescription::cast(parent)?;
        let document = semantic_model.get_document();

        let path = find_ref_at(
            semantic_model
                .get_module()
                .map(|m| m.workspace_id)
                .unwrap_or(WorkspaceId::MAIN),
            semantic_model.get_emmyrc(),
            document.get_text(),
            description.clone(),
            position_offset,
        )?;

        return goto_path(&semantic_model, &analysis.compilation, &path, &token);
    } else if token.kind() == LuaTokenKind::TkTagOverload.into() {
        return goto_overload_function(&semantic_model, &token);
    }

    None
}

fn goto_inferred_dynamic_field_definition(
    semantic_model: &SemanticModel,
    token: &emmylua_parser::LuaSyntaxToken,
) -> Option<GotoDefinitionResponse> {
    let emmyrc = semantic_model.get_emmyrc();
    if !emmyrc.gmod.enabled || !emmyrc.gmod.infer_dynamic_fields {
        return None;
    }

    let index_expr = token.parent()?.ancestors().find_map(LuaIndexExpr::cast)?;
    let index_key = index_expr.get_index_key()?;
    let key_range = index_key.get_range()?;
    if !key_range.contains_range(token.text_range()) {
        return None;
    }

    let field_name = index_key.get_path_part();
    if field_name.is_empty() {
        return None;
    }

    let prefix_type = semantic_model
        .infer_expr(index_expr.get_prefix_expr()?)
        .ok()?;

    let mut locations = Vec::new();
    collect_dynamic_field_locations(semantic_model, &prefix_type, &field_name, &mut locations);
    if locations.is_empty() {
        return None;
    }

    Some(GotoDefinitionResponse::Array(locations))
}

fn collect_dynamic_field_locations(
    semantic_model: &SemanticModel,
    typ: &LuaType,
    field_name: &str,
    locations: &mut Vec<Location>,
) {
    match typ {
        LuaType::Ref(type_decl_id) | LuaType::Def(type_decl_id) => {
            let definitions = semantic_model
                .get_db()
                .get_dynamic_field_index()
                .get_field_definitions(type_decl_id, field_name);
            for definition in definitions {
                if let Some(document) = semantic_model.get_document_by_file_id(definition.file_id)
                    && let Some(location) = document.to_lsp_location(definition.value)
                {
                    locations.push(location);
                }
            }
        }
        LuaType::Instance(instance_type) => {
            collect_dynamic_field_locations(
                semantic_model,
                instance_type.get_base(),
                field_name,
                locations,
            );
        }
        LuaType::Union(union_type) => {
            for union_member in union_type.into_vec() {
                collect_dynamic_field_locations(
                    semantic_model,
                    &union_member,
                    field_name,
                    locations,
                );
            }
        }
        _ => {}
    }
}

fn goto_hook_definition(
    semantic_model: &SemanticModel,
    compilation: &LuaCompilation,
    file_id: FileId,
    position_offset: rowan::TextSize,
    string_token: LuaStringToken,
) -> Option<GotoDefinitionResponse> {
    if !semantic_model.get_emmyrc().gmod.enabled {
        return None;
    }

    let literal_expr = string_token.get_parent::<LuaLiteralExpr>()?;
    let call_expr = literal_expr
        .get_parent::<LuaCallArgList>()?
        .get_parent::<LuaCallExpr>()?;

    let call_path = call_expr.get_access_path()?;
    if !call_path.ends_with("hook.Add")
        && !call_path.ends_with("hook.Run")
        && !call_path.ends_with("hook.Call")
    {
        return None;
    }
    let args_list = call_expr.get_args_list()?;
    if args_list.get_args().next()?.get_position() != literal_expr.get_position() {
        return None;
    }

    let hook_name = string_token.get_value();
    let hook_name = hook_name.trim();
    if hook_name.is_empty() {
        return None;
    }

    let property_owner = crate::handlers::hover::resolve_hook_property_owner(
        semantic_model,
        file_id,
        position_offset,
        hook_name,
    )?;

    let trigger_token = string_token.syntax().clone();
    goto_def_definition(semantic_model, compilation, property_owner, &trigger_token)
}

pub struct DefinitionCapabilities;

impl RegisterCapabilities for DefinitionCapabilities {
    fn register_capabilities(server_capabilities: &mut ServerCapabilities, _: &ClientCapabilities) {
        server_capabilities.definition_provider = Some(OneOf::Left(true));
    }
}
