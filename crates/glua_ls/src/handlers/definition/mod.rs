mod goto_def_definition;
mod goto_doc_see;
mod goto_function;
mod goto_module_file;
mod goto_path;

use std::{collections::HashMap, rc::Rc};

use glua_code_analysis::{
    EmmyLuaAnalysis, FileId, GmodScriptedClassCallKind, LuaCompilation, LuaType, SemanticDeclLevel,
    SemanticModel, WorkspaceId,
};
use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaAstToken, LuaCallArgList, LuaCallExpr, LuaDocDescription,
    LuaDocTagSee, LuaExpr, LuaGeneralToken, LuaIndexExpr, LuaLiteralExpr, LuaLiteralToken,
    LuaStringToken, LuaTokenKind,
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
use crate::handlers::gmod_string_context::{
    NetMessageCallKind, annotated_net_message_flow_call_kind, extract_string_call_context,
    find_string_call_arg_role, is_annotated_derma_skin_string_context,
    is_annotated_vgui_panel_string_context, is_hook_name_string_context,
    is_net_message_string_context,
};
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
    let analysis = context.read_analysis(&cancel_token).await?;
    if cancel_token.is_cancelled() {
        return None;
    }
    let file_id = analysis.get_file_id(&uri)?;
    let position = params.text_document_position_params.position;

    definition_with_cancel(&analysis, file_id, position, Some(&cancel_token))
}

#[cfg(test)]
pub fn definition(
    analysis: &EmmyLuaAnalysis,
    file_id: FileId,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    definition_with_cancel(analysis, file_id, position, None)
}

fn definition_with_cancel(
    analysis: &EmmyLuaAnalysis,
    file_id: FileId,
    position: Position,
    cancel_token: Option<&CancellationToken>,
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
    let dynamic_lookup = resolve_dynamic_field_lookup(&semantic_model, &token);

    let decl_traced = semantic_model.find_decl(token.clone().into(), SemanticDeclLevel::default());
    let decl_no_trace = semantic_model.find_decl(token.clone().into(), SemanticDeclLevel::NoTrace);

    if decl_traced.is_some() || decl_no_trace.is_some() {
        let mut locations = Vec::new();

        let mut add_response = |resp: GotoDefinitionResponse| match resp {
            GotoDefinitionResponse::Scalar(loc) => locations.push(loc),
            GotoDefinitionResponse::Array(mut locs) => locations.append(&mut locs),
            GotoDefinitionResponse::Link(_) => {}
        };

        if let Some(decl) = decl_no_trace.clone() {
            if let Some(resp) =
                goto_def_definition(&semantic_model, &analysis.compilation, decl.clone(), &token)
            {
                add_response(resp);
            }
        }

        if let Some(decl) = decl_traced {
            if decl_no_trace != Some(decl.clone()) {
                if let Some(resp) =
                    goto_def_definition(&semantic_model, &analysis.compilation, decl, &token)
                {
                    add_response(resp);
                }
            }
        }

        if !locations.is_empty() {
            use itertools::Itertools;
            let mut unique_locations: Vec<_> = locations.into_iter().unique().collect();
            if let Some((index_expr, prefix_type, field_name)) = &dynamic_lookup {
                let emmyrc = semantic_model.get_emmyrc();
                if emmyrc.gmod.enabled
                    && emmyrc.gmod.infer_dynamic_fields
                    && !emmyrc.gmod.dynamic_fields_global
                {
                    let mut dynamic_locations = Vec::new();
                    collect_dynamic_field_locations(
                        &semantic_model,
                        prefix_type,
                        field_name,
                        &mut dynamic_locations,
                        semantic_model.get_file_id(),
                        false,
                    );
                    let current_uri = semantic_model.get_document().get_uri();
                    let cross_file_dynamic_locations = dynamic_locations
                        .into_iter()
                        .filter(|location| location.uri != current_uri)
                        .collect::<Vec<_>>();
                    unique_locations.retain(|location| {
                        !cross_file_dynamic_locations.iter().any(|dynamic_location| {
                            dynamic_location.uri == location.uri
                                && dynamic_location.range == location.range
                        })
                    });
                }
                filter_current_line_dynamic_locations(
                    &semantic_model,
                    index_expr,
                    &mut unique_locations,
                );
            }
            if unique_locations.len() == 1 {
                return Some(GotoDefinitionResponse::Scalar(unique_locations.remove(0)));
            } else {
                if !unique_locations.is_empty() {
                    return Some(GotoDefinitionResponse::Array(unique_locations));
                }
            }
        }
    }

    if let Some(dynamic_field_response) =
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
        if let Some(vgui_panel_response) = goto_vgui_panel_definition(
            &semantic_model,
            &analysis.compilation,
            string_token.clone(),
            cancel_token,
        ) {
            return Some(vgui_panel_response);
        }
        if let Some(derma_skin_response) = goto_derma_skin_definition(
            &semantic_model,
            &analysis.compilation,
            string_token.clone(),
            cancel_token,
        ) {
            return Some(derma_skin_response);
        }
        if let Some(net_message_response) = goto_net_message_definition(
            &semantic_model,
            &analysis.compilation,
            string_token.clone(),
            cancel_token,
        ) {
            return Some(net_message_response);
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

pub(crate) fn goto_inferred_dynamic_field_definition(
    semantic_model: &SemanticModel,
    token: &glua_parser::LuaSyntaxToken,
) -> Option<GotoDefinitionResponse> {
    let emmyrc = semantic_model.get_emmyrc();
    if !emmyrc.gmod.enabled || !emmyrc.gmod.infer_dynamic_fields {
        return None;
    }

    let (index_expr, prefix_type, field_name) =
        resolve_dynamic_field_lookup(semantic_model, token)?;

    let mut locations = Vec::new();
    collect_dynamic_field_locations(
        semantic_model,
        &prefix_type,
        &field_name,
        &mut locations,
        semantic_model.get_file_id(),
        true,
    );
    filter_current_line_dynamic_locations(semantic_model, &index_expr, &mut locations);
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
    caller_file_id: FileId,
    respect_file_scope: bool,
) {
    let dynamic_fields_global = semantic_model.get_emmyrc().gmod.dynamic_fields_global;
    match typ {
        LuaType::Ref(type_decl_id) | LuaType::Def(type_decl_id) => {
            let owner = glua_code_analysis::DynamicFieldOwner::Type(type_decl_id.clone());
            let definitions = semantic_model
                .get_db()
                .get_dynamic_field_index()
                .get_field_definitions(&owner, field_name);
            for definition in definitions {
                if respect_file_scope
                    && !dynamic_fields_global
                    && definition.file_id != caller_file_id
                {
                    continue;
                }
                if let Some(document) = semantic_model.get_document_by_file_id(definition.file_id)
                    && let Some(location) = document.to_lsp_location(definition.value)
                {
                    locations.push(location);
                }
            }
        }
        LuaType::TableConst(table_range) => {
            let owner = glua_code_analysis::DynamicFieldOwner::Table(table_range.clone());
            let definitions = semantic_model
                .get_db()
                .get_dynamic_field_index()
                .get_field_definitions(&owner, field_name);
            for definition in definitions {
                if respect_file_scope
                    && !dynamic_fields_global
                    && definition.file_id != caller_file_id
                {
                    continue;
                }
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
                caller_file_id,
                respect_file_scope,
            );
        }
        LuaType::TableOf(inner) => {
            collect_dynamic_field_locations(
                semantic_model,
                inner,
                field_name,
                locations,
                caller_file_id,
                respect_file_scope,
            );
        }
        LuaType::Union(union_type) => {
            for union_member in union_type.types() {
                collect_dynamic_field_locations(
                    semantic_model,
                    union_member,
                    field_name,
                    locations,
                    caller_file_id,
                    respect_file_scope,
                );
            }
        }
        _ => {}
    }
}

fn filter_current_line_dynamic_locations(
    semantic_model: &SemanticModel,
    index_expr: &LuaIndexExpr,
    locations: &mut Vec<Location>,
) {
    let Some(current_key_range) = index_expr.get_index_key().and_then(|key| key.get_range()) else {
        return;
    };
    let document = semantic_model.get_document();
    let current_uri = document.get_uri();
    let Some(current_location) = document.to_lsp_location(current_key_range) else {
        return;
    };
    let current_assignment_range = index_expr
        .syntax()
        .ancestors()
        .find_map(LuaAssignStat::cast)
        .and_then(|assign_stat| document.to_lsp_range(assign_stat.get_range()));
    locations.retain(|location| {
        if location.uri != current_uri {
            return true;
        }

        location.range != current_location.range
            && current_assignment_range
                .as_ref()
                .is_none_or(|assignment_range| {
                    !range_contains_range(assignment_range, &location.range)
                })
    });
}

fn range_contains_range(outer: &lsp_types::Range, inner: &lsp_types::Range) -> bool {
    position_le(&outer.start, &inner.start) && position_le(&inner.end, &outer.end)
}

fn position_le(left: &Position, right: &Position) -> bool {
    left.line < right.line || (left.line == right.line && left.character <= right.character)
}

fn resolve_dynamic_field_lookup(
    semantic_model: &SemanticModel,
    token: &glua_parser::LuaSyntaxToken,
) -> Option<(LuaIndexExpr, LuaType, String)> {
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
    Some((index_expr, prefix_type, field_name))
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

    let args_list = call_expr.get_args_list()?;
    let arg_index = args_list
        .get_args()
        .position(|arg| arg.get_position() == literal_expr.get_position())?;

    if !is_hook_name_string_context(semantic_model, &call_expr, arg_index) {
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

fn goto_vgui_panel_definition(
    semantic_model: &SemanticModel,
    compilation: &LuaCompilation,
    string_token: LuaStringToken,
    cancel_token: Option<&CancellationToken>,
) -> Option<GotoDefinitionResponse> {
    if !semantic_model.get_emmyrc().gmod.enabled {
        return None;
    }

    let context = extract_string_call_context(&string_token)?;
    let annotated_context = string_token
        .get_parent::<LuaLiteralExpr>()
        .and_then(|literal| literal.get_parent::<LuaCallArgList>())
        .and_then(|args| args.get_parent::<LuaCallExpr>())
        .is_some_and(|call_expr| {
            is_annotated_vgui_panel_string_context(semantic_model, &call_expr, context.arg_index)
        });
    if !annotated_context {
        return None;
    }

    let panel_name = context.name;
    let definitions = semantic_model
        .get_db()
        .get_gmod_class_metadata_index()
        .find_vgui_panel_definitions(&panel_name);

    let mut locations = Vec::new();
    for (file_id, call) in definitions {
        let definition_range = call.define_arg_range(GmodScriptedClassCallKind::VguiRegister);
        let Some(document) = semantic_model.get_document_by_file_id(file_id) else {
            continue;
        };
        let Some(location) = document.to_lsp_location(definition_range) else {
            continue;
        };
        push_unique_location(&mut locations, location);
    }

    collect_annotated_string_definitions(
        semantic_model,
        compilation,
        &panel_name,
        "gmod.vgui_panel",
        &["define", "define_control"],
        &mut locations,
        cancel_token,
    );

    if locations.is_empty() {
        return None;
    }

    Some(GotoDefinitionResponse::Array(locations))
}

fn goto_derma_skin_definition(
    semantic_model: &SemanticModel,
    compilation: &LuaCompilation,
    string_token: LuaStringToken,
    cancel_token: Option<&CancellationToken>,
) -> Option<GotoDefinitionResponse> {
    if !semantic_model.get_emmyrc().gmod.enabled {
        return None;
    }

    let context = extract_string_call_context(&string_token)?;
    let annotated_context = string_token
        .get_parent::<LuaLiteralExpr>()
        .and_then(|literal| literal.get_parent::<LuaCallArgList>())
        .and_then(|args| args.get_parent::<LuaCallExpr>())
        .is_some_and(|call_expr| {
            is_annotated_derma_skin_string_context(semantic_model, &call_expr, context.arg_index)
        });
    if !annotated_context {
        return None;
    }

    let definitions = semantic_model
        .get_db()
        .get_gmod_class_metadata_index()
        .find_derma_skin_definitions(&context.name);

    let mut locations = Vec::new();
    for (file_id, call) in definitions {
        let definition_range = call.define_arg_range(GmodScriptedClassCallKind::DermaDefineSkin);
        let Some(document) = semantic_model.get_document_by_file_id(file_id) else {
            continue;
        };
        let Some(location) = document.to_lsp_location(definition_range) else {
            continue;
        };
        push_unique_location(&mut locations, location);
    }

    collect_annotated_string_definitions(
        semantic_model,
        compilation,
        &context.name,
        "gmod.derma_skin",
        &["define"],
        &mut locations,
        cancel_token,
    );

    if locations.is_empty() {
        return None;
    }

    Some(GotoDefinitionResponse::Array(locations))
}

fn collect_annotated_string_definitions(
    semantic_model: &SemanticModel,
    compilation: &LuaCompilation,
    name: &str,
    domain: &str,
    roles: &[&str],
    locations: &mut Vec<Location>,
    cancel_token: Option<&CancellationToken>,
) {
    let before_indexed = locations.len();
    let mut semantic_cache = HashMap::new();
    for reference in semantic_model
        .get_db()
        .get_reference_index()
        .get_string_references(name)
    {
        if cancel_token.is_some_and(CancellationToken::is_cancelled) {
            return;
        }

        let Some(reference_semantic_model) =
            get_semantic_model_cached(compilation, &mut semantic_cache, reference.file_id)
        else {
            continue;
        };
        let root = reference_semantic_model.get_root();
        let Some(reference_token) = root
            .syntax()
            .token_at_offset(reference.value.start())
            .right_biased()
        else {
            continue;
        };
        let Some(reference_string_token) = LuaStringToken::cast(reference_token) else {
            continue;
        };
        let Some(reference_context) = extract_string_call_context(&reference_string_token) else {
            continue;
        };
        if reference_context.name != name {
            continue;
        }

        let Some(call_expr) = reference_string_token
            .get_parent::<LuaLiteralExpr>()
            .and_then(|literal| literal.get_parent::<LuaCallArgList>())
            .and_then(|args| args.get_parent::<LuaCallExpr>())
        else {
            continue;
        };
        if find_string_call_arg_role(
            &reference_semantic_model,
            &call_expr,
            reference_context.arg_index,
            domain,
            roles,
        )
        .is_none()
        {
            continue;
        }

        let Some(location) = reference_semantic_model
            .get_document()
            .to_lsp_location(reference_string_token.get_range())
        else {
            continue;
        };
        push_unique_location(locations, location);
    }

    if locations.len() == before_indexed {
        collect_annotated_string_definitions_from_ast(
            semantic_model,
            compilation,
            name,
            domain,
            roles,
            locations,
            cancel_token,
            &mut semantic_cache,
        );
    }
}

fn collect_annotated_string_definitions_from_ast<'a>(
    semantic_model: &SemanticModel,
    compilation: &'a LuaCompilation,
    name: &str,
    domain: &str,
    roles: &[&str],
    locations: &mut Vec<Location>,
    cancel_token: Option<&CancellationToken>,
    semantic_cache: &mut HashMap<FileId, Rc<SemanticModel<'a>>>,
) {
    for file_id in semantic_model.get_db().get_vfs().get_all_file_ids() {
        if cancel_token.is_some_and(CancellationToken::is_cancelled) {
            return;
        }

        let Some(reference_semantic_model) =
            get_semantic_model_cached(compilation, semantic_cache, file_id)
        else {
            continue;
        };
        let root = reference_semantic_model.get_root();
        for call_expr in root.descendants::<LuaCallExpr>() {
            if cancel_token.is_some_and(CancellationToken::is_cancelled) {
                return;
            }

            let Some(args_list) = call_expr.get_args_list() else {
                continue;
            };
            for (arg_index, arg) in args_list.get_args().enumerate() {
                let LuaExpr::LiteralExpr(literal_expr) = arg else {
                    continue;
                };
                let Some(LuaLiteralToken::String(string_token)) = literal_expr.get_literal() else {
                    continue;
                };
                let Some(candidate_name) =
                    crate::handlers::gmod_string_context::normalize_string_name(
                        string_token.get_value(),
                    )
                else {
                    continue;
                };
                if candidate_name != name {
                    continue;
                }
                if find_string_call_arg_role(
                    &reference_semantic_model,
                    &call_expr,
                    arg_index,
                    domain,
                    roles,
                )
                .is_none()
                {
                    continue;
                }

                let Some(location) = reference_semantic_model
                    .get_document()
                    .to_lsp_location(string_token.get_range())
                else {
                    continue;
                };
                push_unique_location(locations, location);
            }
        }
    }
}

fn get_semantic_model_cached<'a>(
    compilation: &'a LuaCompilation,
    semantic_cache: &mut HashMap<FileId, Rc<SemanticModel<'a>>>,
    file_id: FileId,
) -> Option<Rc<SemanticModel<'a>>> {
    if let Some(cached) = semantic_cache.get(&file_id) {
        return Some(Rc::clone(cached));
    }

    let semantic_model = Rc::new(compilation.get_semantic_model(file_id)?);
    semantic_cache.insert(file_id, Rc::clone(&semantic_model));
    Some(semantic_model)
}

fn push_unique_location(locations: &mut Vec<Location>, location: Location) {
    if !locations.contains(&location) {
        locations.push(location);
    }
}

fn goto_net_message_definition(
    semantic_model: &SemanticModel,
    compilation: &LuaCompilation,
    string_token: LuaStringToken,
    cancel_token: Option<&CancellationToken>,
) -> Option<GotoDefinitionResponse> {
    if !semantic_model.get_emmyrc().gmod.enabled {
        return None;
    }

    let context = extract_string_call_context(&string_token)?;
    let call_expr = string_token
        .get_parent::<LuaLiteralExpr>()
        .and_then(|literal| literal.get_parent::<LuaCallArgList>())
        .and_then(|args| args.get_parent::<LuaCallExpr>());
    let is_context = call_expr.as_ref().is_some_and(|call_expr| {
        is_net_message_string_context(semantic_model, call_expr, context.arg_index)
    });
    if !is_context {
        return None;
    }
    let call_kind = call_expr.as_ref().and_then(|call_expr| {
        annotated_net_message_flow_call_kind(semantic_model, call_expr, context.arg_index)
    });
    let annotated_reference_context = call_expr.as_ref().is_some_and(|call_expr| {
        find_string_call_arg_role(
            semantic_model,
            call_expr,
            context.arg_index,
            "gmod.net_message",
            &["define", "reference"],
        )
        .is_some()
    });
    if call_kind.is_none() && !annotated_reference_context {
        return None;
    }
    let message_name = context.name;

    let network_index = semantic_model.get_db().get_gmod_network_index();
    let mut locations = Vec::new();

    match call_kind {
        Some(NetMessageCallKind::Start) => {
            for (file_id, flow) in network_index.get_receive_flows_for_message(&message_name) {
                let Some(document) = semantic_model.get_document_by_file_id(file_id) else {
                    continue;
                };
                let Some(location) = document.to_lsp_location(flow.receive_range) else {
                    continue;
                };
                push_unique_location(&mut locations, location);
            }
        }
        Some(NetMessageCallKind::Receive) => {
            for (file_id, flow) in network_index.get_send_flows_for_message(&message_name) {
                let Some(document) = semantic_model.get_document_by_file_id(file_id) else {
                    continue;
                };
                let Some(location) = document.to_lsp_location(flow.start_range) else {
                    continue;
                };
                push_unique_location(&mut locations, location);
            }
        }
        None => {}
    }

    collect_annotated_string_definitions(
        semantic_model,
        compilation,
        &message_name,
        "gmod.net_message",
        &["define"],
        &mut locations,
        cancel_token,
    );

    if locations.is_empty() {
        return None;
    }

    Some(GotoDefinitionResponse::Array(locations))
}

pub struct DefinitionCapabilities;

impl RegisterCapabilities for DefinitionCapabilities {
    fn register_capabilities(server_capabilities: &mut ServerCapabilities, _: &ClientCapabilities) {
        server_capabilities.definition_provider = Some(OneOf::Left(true));
    }
}
