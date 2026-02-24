mod build_hover;
mod find_origin;
mod function;
mod hover_builder;
mod humanize_type_decl;
mod humanize_types;
mod keyword_hover;

use super::RegisterCapabilities;
use crate::context::ServerContextSnapshot;
use crate::util::{find_ref_at, resolve_ref_single};
pub use build_hover::build_hover_content_for_completion;
use build_hover::build_semantic_info_hover;
use emmylua_code_analysis::{
    EmmyLuaAnalysis, FileId, GmodRealm, LuaMemberKey, LuaSemanticDeclId, LuaType, LuaTypeDeclId,
    WorkspaceId,
};
use emmylua_parser::{
    LuaAstNode, LuaAstToken, LuaCallArgList, LuaCallExpr, LuaDocDescription, LuaLiteralExpr,
    LuaStringToken, LuaTokenKind, PathTrait,
};
use emmylua_parser_desc::parse_ref_target;
pub use find_origin::{find_all_same_named_members, find_member_origin_owner};
pub use hover_builder::HoverBuilder;
pub use humanize_types::infer_prefix_global_name;
use humanize_types::infer_property_owner_realm;
use keyword_hover::{hover_keyword, is_keyword};
use lsp_types::{
    ClientCapabilities, Hover, HoverContents, HoverParams, HoverProviderCapability, MarkupContent,
    Position, ServerCapabilities,
};
use rowan::{TextSize, TokenAtOffset};
use tokio_util::sync::CancellationToken;

pub async fn on_hover(
    context: ServerContextSnapshot,
    params: HoverParams,
    cancel_token: CancellationToken,
) -> Option<Hover> {
    if cancel_token.is_cancelled() {
        return None;
    }
    let uri = params.text_document_position_params.text_document.uri;
    let position = params.text_document_position_params.position;
    let analysis = context.analysis().read().await;
    if cancel_token.is_cancelled() {
        return None;
    }
    let file_id = analysis.get_file_id(&uri)?;
    hover(&analysis, file_id, position)
}

pub fn hover(analysis: &EmmyLuaAnalysis, file_id: FileId, position: Position) -> Option<Hover> {
    let semantic_model = analysis.compilation.get_semantic_model(file_id)?;
    if !semantic_model.get_emmyrc().hover.enable {
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
        TokenAtOffset::Between(left, right) => {
            if matches!(
                right.kind().into(),
                LuaTokenKind::TkDot
                    | LuaTokenKind::TkColon
                    | LuaTokenKind::TkLeftBracket
                    | LuaTokenKind::TkRightBracket
            ) {
                left
            } else {
                right
            }
        }
        TokenAtOffset::None => return None,
    };
    match token {
        keywords if is_keyword(keywords.clone()) => {
            let document = semantic_model.get_document();
            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: lsp_types::MarkupKind::Markdown,
                    value: hover_keyword(keywords.clone()),
                }),
                range: document.to_lsp_range(keywords.text_range()),
            })
        }
        detail if detail.kind() == LuaTokenKind::TkDocDetail.into() => {
            let parent = detail.parent()?;
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

            let db = analysis.compilation.get_db();
            let semantic_info = resolve_ref_single(db, file_id, &path, &detail)?;

            build_semantic_info_hover(
                &analysis.compilation,
                &semantic_model,
                db,
                &document,
                detail,
                semantic_info,
                path.last()?.1,
            )
        }
        doc_see if doc_see.kind() == LuaTokenKind::TkDocSeeContent.into() => {
            let document = semantic_model.get_document();

            let path =
                parse_ref_target(document.get_text(), doc_see.text_range(), position_offset)?;

            let db = analysis.compilation.get_db();
            let semantic_info = resolve_ref_single(db, file_id, &path, &doc_see)?;

            build_semantic_info_hover(
                &analysis.compilation,
                &semantic_model,
                db,
                &document,
                doc_see,
                semantic_info,
                path.last()?.1,
            )
        }
        _ => {
            if let Some(hook_hover) = hover_gmod_hook_name_string(
                analysis,
                &semantic_model,
                file_id,
                position_offset,
                &token,
            ) {
                return Some(hook_hover);
            }

            if let Some(hook_callback_hover) = hover_gmod_hook_callback_function(
                analysis,
                &semantic_model,
                file_id,
                position_offset,
                &token,
            ) {
                return Some(hook_callback_hover);
            }

            let semantic_info = semantic_model.get_semantic_info(token.clone().into())?;
            let db = semantic_model.get_db();
            let document = semantic_model.get_document();
            let range = token.text_range();

            build_semantic_info_hover(
                &analysis.compilation,
                &semantic_model,
                db,
                &document,
                token,
                semantic_info,
                range,
            )
        }
    }
}

const HOOK_OWNER_TYPES: &[&str] = &["GM", "GAMEMODE", "SANDBOX", "PLUGIN"];

fn hover_gmod_hook_name_string(
    analysis: &EmmyLuaAnalysis,
    semantic_model: &emmylua_code_analysis::SemanticModel,
    file_id: FileId,
    position_offset: TextSize,
    token: &emmylua_parser::LuaSyntaxToken,
) -> Option<Hover> {
    if !semantic_model.get_emmyrc().gmod.enabled {
        return None;
    }

    let string_token = LuaStringToken::cast(token.clone())?;
    let literal_expr = string_token.get_parent::<LuaLiteralExpr>()?;
    let call_expr = literal_expr
        .get_parent::<LuaCallArgList>()?
        .get_parent::<LuaCallExpr>()?;
    if !is_hook_name_string_context(&call_expr, literal_expr) {
        return None;
    }

    let hook_name = string_token.get_value();
    let hook_name = hook_name.trim();
    if hook_name.is_empty() {
        return None;
    }

    let property_owner =
        resolve_hook_property_owner(semantic_model, file_id, position_offset, hook_name)?;
    let db = semantic_model.get_db();
    let document = semantic_model.get_document();
    let builder = build_hover_content_for_completion(
        &analysis.compilation,
        semantic_model,
        db,
        property_owner,
    )?;
    builder.build_hover_result(document.to_lsp_range(token.text_range()))
}

fn hover_gmod_hook_callback_function(
    analysis: &EmmyLuaAnalysis,
    semantic_model: &emmylua_code_analysis::SemanticModel,
    file_id: FileId,
    position_offset: TextSize,
    token: &emmylua_parser::LuaSyntaxToken,
) -> Option<Hover> {
    if !semantic_model.get_emmyrc().gmod.enabled {
        return None;
    }

    if token.kind() != LuaTokenKind::TkFunction.into() {
        return None;
    }

    let closure_expr = emmylua_parser::LuaClosureExpr::cast(token.parent()?)?;
    let call_arg_list = closure_expr.get_parent::<LuaCallArgList>()?;
    let call_expr = call_arg_list.get_parent::<LuaCallExpr>()?;

    let call_path = call_expr.get_access_path()?;
    if !matches_call_path(&call_path, "hook.Add") {
        return None;
    }

    let mut param_idx = 0;
    for (idx, arg) in call_arg_list.get_args().enumerate() {
        if arg.syntax() == closure_expr.syntax() {
            param_idx = idx;
            break;
        }
    }

    if param_idx != 2 {
        return None;
    }

    let hook_name = emmylua_code_analysis::extract_hook_name(&call_expr)?;
    let property_owner =
        resolve_hook_property_owner(semantic_model, file_id, position_offset, &hook_name)?;
    let db = semantic_model.get_db();
    let document = semantic_model.get_document();
    let builder = build_hover_content_for_completion(
        &analysis.compilation,
        semantic_model,
        db,
        property_owner,
    )?;
    builder.build_hover_result(document.to_lsp_range(token.text_range()))
}

fn resolve_hook_property_owner(
    semantic_model: &emmylua_code_analysis::SemanticModel,
    file_id: FileId,
    position_offset: TextSize,
    hook_name: &str,
) -> Option<LuaSemanticDeclId> {
    let member_key = LuaMemberKey::Name(hook_name.into());
    let call_realm = semantic_model
        .get_db()
        .get_gmod_infer_index()
        .get_realm_at_offset(&file_id, position_offset);
    let mut fallback = None;

    for owner_name in HOOK_OWNER_TYPES {
        let owner_type = LuaType::Ref(LuaTypeDeclId::global(owner_name));
        let Some(member_infos) =
            semantic_model.get_member_info_with_key(&owner_type, member_key.clone(), true)
        else {
            continue;
        };

        for member_info in member_infos {
            let Some(property_owner) = member_info.property_owner_id else {
                continue;
            };
            if fallback.is_none() {
                fallback = Some(property_owner.clone());
            }

            let Some(property_realm) = infer_property_owner_realm(semantic_model, &property_owner)
            else {
                return Some(property_owner);
            };
            if is_realm_compatible(call_realm, property_realm) {
                return Some(property_owner);
            }
        }
    }

    fallback
}

fn is_hook_name_string_context(call_expr: &LuaCallExpr, literal_expr: LuaLiteralExpr) -> bool {
    let Some(call_path) = call_expr.get_access_path() else {
        return false;
    };
    if !matches_call_path(&call_path, "hook.Add")
        && !matches_call_path(&call_path, "hook.Run")
        && !matches_call_path(&call_path, "hook.Call")
    {
        return false;
    }

    let Some(args_list) = call_expr.get_args_list() else {
        return false;
    };
    let arg_idx = args_list
        .get_args()
        .position(|arg| arg.get_position() == literal_expr.get_position());
    arg_idx == Some(0)
}

fn matches_call_path(path: &str, target: &str) -> bool {
    path == target || path.ends_with(&format!(".{target}")) || path.ends_with(&format!(":{target}"))
}

fn is_realm_compatible(call_realm: GmodRealm, item_realm: GmodRealm) -> bool {
    !matches!(
        (call_realm, item_realm),
        (GmodRealm::Client, GmodRealm::Server) | (GmodRealm::Server, GmodRealm::Client)
    )
}

pub struct HoverCapabilities;

impl RegisterCapabilities for HoverCapabilities {
    fn register_capabilities(server_capabilities: &mut ServerCapabilities, _: &ClientCapabilities) {
        server_capabilities.hover_provider = Some(HoverProviderCapability::Simple(true));
    }
}
