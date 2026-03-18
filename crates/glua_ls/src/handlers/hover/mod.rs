mod build_hover;
mod find_origin;
mod function;
mod hover_builder;
mod humanize_type_decl;
mod humanize_types;
mod keyword_hover;
mod realm_badge;

use super::RegisterCapabilities;
use crate::context::ServerContextSnapshot;
use crate::util::{find_ref_at, resolve_ref_single};
pub use build_hover::build_hover_content_for_completion;
use build_hover::build_semantic_info_hover;
pub use find_origin::{
    find_all_same_named_members, find_member_origin_owner, find_member_origin_owners,
};
use glua_code_analysis::{
    EmmyLuaAnalysis, FileId, GmodRealm, LuaMemberKey, LuaSemanticDeclId, LuaType, LuaTypeDeclId,
    RenderLevel, WorkspaceId, humanize_type, resolve_gmod_hook_add_callback_doc_function,
};
use glua_parser::{
    LuaAstNode, LuaAstToken, LuaCallArgList, LuaCallExpr, LuaDocDescription, LuaLiteralExpr,
    LuaStringToken, LuaTokenKind, PathTrait,
};
use glua_parser_desc::parse_ref_target;
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
    let analysis = context.read_analysis(&cancel_token).await?;
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
        function_kw if function_kw.kind() == LuaTokenKind::TkFunction.into() => {
            // For `function` keyword tokens, check if this is a hook.Add callback first.
            // If so, show hook-specific hover (signature + description) instead of generic
            // keyword docs.
            if let Some(hook_callback_hover) = hover_gmod_hook_callback_function(
                analysis,
                &semantic_model,
                file_id,
                position_offset,
                &function_kw,
            ) {
                return Some(hook_callback_hover);
            }
            let document = semantic_model.get_document();
            Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: lsp_types::MarkupKind::Markdown,
                    value: hover_keyword(function_kw.clone()),
                }),
                range: document.to_lsp_range(function_kw.text_range()),
            })
        }
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
    semantic_model: &glua_code_analysis::SemanticModel,
    file_id: FileId,
    position_offset: TextSize,
    token: &glua_parser::LuaSyntaxToken,
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
    semantic_model: &glua_code_analysis::SemanticModel,
    file_id: FileId,
    position_offset: TextSize,
    token: &glua_parser::LuaSyntaxToken,
) -> Option<Hover> {
    if !semantic_model.get_emmyrc().gmod.enabled {
        return None;
    }

    // This function is only called from the TkFunction dispatch arm, so the
    // token kind is already guaranteed. No redundant check needed here.

    let closure_expr = glua_parser::LuaClosureExpr::cast(token.parent()?)?;
    let call_arg_list = closure_expr.get_parent::<LuaCallArgList>()?;
    let call_expr = call_arg_list.get_parent::<LuaCallExpr>()?;

    let call_path = call_expr.get_access_path()?;
    if !matches_call_path(&call_path, "hook.Add") {
        return None;
    }

    // Use text range comparison instead of syntax node identity to robustly
    // locate the closure's position in the argument list across traversal paths.
    let closure_range = closure_expr.syntax().text_range();
    let param_idx = call_arg_list
        .get_args()
        .enumerate()
        .find(|(_, arg)| arg.syntax().text_range() == closure_range)
        .map(|(idx, _)| idx);

    if param_idx != Some(2) {
        return None;
    }

    let hook_name = glua_code_analysis::extract_hook_name(&call_expr)?;
    let property_owner =
        resolve_hook_property_owner(semantic_model, file_id, position_offset, &hook_name)?;
    let db = semantic_model.get_db();
    let document = semantic_model.get_document();

    // Build the base hover from the hook property owner (gives description, realm, param docs)
    let mut builder = build_hover_content_for_completion(
        &analysis.compilation,
        semantic_model,
        db,
        property_owner,
    )?;

    // Now override the primary type description with an anonymous callback signature,
    // e.g. `function(ply: Player, seat: Vehicle) -> boolean`
    // using the resolved callback doc function for this hook.
    // param_idx == Some(2) is guaranteed by the guard above.
    if let Some(callback_func) =
        resolve_gmod_hook_add_callback_doc_function(db, &call_expr, 2, None, file_id)
    {
        let params_str = callback_func
            .get_params()
            .iter()
            .map(|(name, ty)| {
                if let Some(ty) = ty {
                    format!("{}: {}", name, humanize_type(db, ty, RenderLevel::Simple))
                } else {
                    name.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");

        let ret = callback_func.get_ret();
        // Show the return type if it is documented. Nil here means the hook signature has
        // no @return annotation (filter_signature_type uses Nil as the default), so we
        // suppress it to avoid a misleading "-> nil" in the hover.
        let ret_str = if ret.is_nil() || ret.is_unknown() {
            String::new()
        } else {
            format!(" -> {}", humanize_type(db, ret, RenderLevel::Simple))
        };

        builder.set_type_description(format!("function({}){}", params_str, ret_str));
        // Clear overloads — an anonymous callback shouldn't show named overloads.
        builder.signature_overload = None;
    }

    builder.build_hover_result(document.to_lsp_range(token.text_range()))
}

pub(crate) fn resolve_hook_property_owner(
    semantic_model: &glua_code_analysis::SemanticModel,
    file_id: FileId,
    position_offset: TextSize,
    hook_name: &str,
) -> Option<LuaSemanticDeclId> {
    let member_key = LuaMemberKey::Name(hook_name.into());
    let db = semantic_model.get_db();
    let call_realm = db
        .get_gmod_infer_index()
        .get_realm_at_offset(&file_id, position_offset);
    let mut fallback = None;

    // Build the full set of owner type names, matching the logic in iter_hook_owner_names()
    // in resolve_closure.rs so that user-configured hook_mappings.method_prefixes are included.
    let mut owner_names: Vec<String> = HOOK_OWNER_TYPES.iter().map(|s| s.to_string()).collect();
    for prefix in &db.get_emmyrc().gmod.hook_mappings.method_prefixes {
        let normalized = prefix.trim().trim_end_matches([':', '.']).to_string();
        if !normalized.is_empty()
            && !owner_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(&normalized))
        {
            owner_names.push(normalized);
        }
    }

    for owner_name in &owner_names {
        let owner_type = LuaType::Ref(LuaTypeDeclId::global(owner_name));
        let Some(member_infos) = semantic_model.get_member_info_with_key_at_offset(
            &owner_type,
            member_key.clone(),
            true,
            position_offset,
        ) else {
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
    // All call sites pass a fully-qualified target (e.g. "hook.Add").
    // get_access_path() also returns the full qualified path, so an exact equality check
    // is both necessary and sufficient. A suffix check would produce false positives for
    // paths like "mylib.hook.Add" when the target is "hook.Add".
    path == target
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
