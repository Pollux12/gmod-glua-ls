use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use glua_code_analysis::{
    DeclReferenceCell, FileId, LuaCompilation, LuaDeclId, LuaMemberId, LuaMemberKey,
    LuaSemanticDeclId, LuaType, LuaTypeDeclId, SemanticDeclLevel, SemanticModel,
};
use glua_parser::{
    LuaAssignStat, LuaAst, LuaAstNode, LuaAstToken, LuaCallExpr, LuaExpr, LuaLiteralToken,
    LuaNameToken, LuaStringToken, LuaSyntaxNode, LuaSyntaxToken, LuaTableField, PathTrait,
};
use lsp_types::Location;

use crate::handlers::gmod_string_context::{
    extract_string_call_context, is_vgui_panel_string_context, net_message_call_kind,
    normalize_string_name,
};

#[derive(Default)]
struct ReferenceSearchContext {
    visited_module_exports: HashSet<FileId>,
    visited_semantic_ids: HashSet<LuaSemanticDeclId>,
}

pub fn search_references(
    semantic_model: &SemanticModel,
    compilation: &LuaCompilation,
    token: LuaSyntaxToken,
) -> Option<Vec<Location>> {
    let mut result = Vec::new();
    if let Some(semantic_decl) =
        semantic_model.find_decl(token.clone().into(), SemanticDeclLevel::default())
    {
        match semantic_decl {
            LuaSemanticDeclId::LuaDecl(decl_id) => {
                let _ = search_decl_references_with_token(
                    semantic_model,
                    compilation,
                    decl_id,
                    token,
                    &mut result,
                );
            }
            LuaSemanticDeclId::Member(member_id) => {
                let _ =
                    search_member_references(semantic_model, compilation, member_id, &mut result);
            }
            LuaSemanticDeclId::TypeDecl(type_decl_id) => {
                let _ = search_type_decl_references(semantic_model, type_decl_id, &mut result);
            }
            _ => {}
        }
    } else if let Some(string_token) = LuaStringToken::cast(token.clone()) {
        if semantic_model.get_emmyrc().gmod.enabled {
            if search_vgui_panel_string_references(
                semantic_model,
                compilation,
                string_token.clone(),
                &mut result,
            )
            .is_some()
            {
                return Some(result);
            }

            if search_net_message_references(semantic_model, string_token.clone(), &mut result)
                .is_some()
            {
                return Some(result);
            }
        }

        let _ = search_string_references(semantic_model, string_token, &mut result);
    } else if semantic_model.get_emmyrc().references.fuzzy_search {
        let _ = fuzzy_search_references(compilation, token, &mut result);
    }

    // 简单过滤, 同行的多个引用只保留一个
    // let filtered_result = filter_duplicate_and_covered_locations(result);
    // Some(filtered_result)

    Some(result)
}

pub fn search_decl_references_with_token(
    semantic_model: &SemanticModel,
    compilation: &LuaCompilation,
    decl_id: LuaDeclId,
    token: LuaSyntaxToken,
    result: &mut Vec<Location>,
) -> Option<()> {
    let mut ctx = ReferenceSearchContext::default();
    let mut semantic_cache = HashMap::new();
    let previous_result = result.len();
    let ret = search_semantic_references_with_ctx(
        &mut ctx,
        compilation,
        &mut semantic_cache,
        LuaSemanticDeclId::LuaDecl(decl_id),
        result,
    );
    // 如果不等于当前文件, 那么我们可能是引用了其他文件的导出
    if ret.is_none()
        && previous_result == result.len()
        && decl_id.file_id != semantic_model.get_file_id()
    {
        if let Some(semantic_decl) =
            semantic_model.find_decl(token.clone().into(), SemanticDeclLevel::NoTrace)
        {
            if let LuaSemanticDeclId::LuaDecl(decl_id) = semantic_decl {
                return search_semantic_references_with_ctx(
                    &mut ctx,
                    compilation,
                    &mut semantic_cache,
                    LuaSemanticDeclId::LuaDecl(decl_id),
                    result,
                );
            }
        }
    }
    ret
}

pub fn search_decl_references(
    _semantic_model: &SemanticModel,
    compilation: &LuaCompilation,
    decl_id: LuaDeclId,
    result: &mut Vec<Location>,
) -> Option<()> {
    let mut ctx = ReferenceSearchContext::default();
    let mut semantic_cache = HashMap::new();
    search_semantic_references_with_ctx(
        &mut ctx,
        compilation,
        &mut semantic_cache,
        LuaSemanticDeclId::LuaDecl(decl_id),
        result,
    )
}

fn search_decl_references_with_ctx<'a>(
    ctx: &mut ReferenceSearchContext,
    semantic_model: &SemanticModel<'a>,
    compilation: &'a LuaCompilation,
    semantic_cache: &mut HashMap<FileId, Arc<SemanticModel<'a>>>,
    decl_id: LuaDeclId,
    result: &mut Vec<Location>,
    worklist: &mut Vec<LuaSemanticDeclId>,
) -> Option<()> {
    let decl = semantic_model
        .get_db()
        .get_decl_index()
        .get_decl(&decl_id)?;
    if decl.is_local() {
        let decl_refs = semantic_model
            .get_db()
            .get_reference_index()
            .get_decl_references(&decl_id.file_id, &decl_id)?;
        let document = semantic_model.get_document();
        // 加入自己
        if let Some(location) = document.to_lsp_location(decl.get_range()) {
            result.push(location);
        }
        let typ = semantic_model.get_type(decl.get_id().into());
        let should_follow_value_alias = matches!(
            typ,
            LuaType::Signature(_)
                | LuaType::Table
                | LuaType::TableConst(_)
                | LuaType::Ref(_)
                | LuaType::Def(_)
        );

        for decl_ref in &decl_refs.cells {
            let location = document.to_lsp_location(decl_ref.range)?;
            result.push(location);
            if should_follow_value_alias {
                let _ = enqueue_value_alias_references(ctx, semantic_model, decl_ref, worklist);
            }
        }

        let _ = extend_module_return_value_references(
            ctx,
            semantic_model,
            compilation,
            semantic_cache,
            decl_id,
            result,
            worklist,
        );

        return Some(());
    } else {
        let name = decl.get_name();
        let global_references = semantic_model
            .get_db()
            .get_reference_index()
            .get_global_references(name)?;
        for in_filed_syntax_id in global_references {
            let document = semantic_model.get_document_by_file_id(in_filed_syntax_id.file_id)?;
            let location = document.to_lsp_location(in_filed_syntax_id.value.get_range())?;
            result.push(location);
        }
    }

    Some(())
}

pub fn search_member_references(
    _semantic_model: &SemanticModel,
    compilation: &LuaCompilation,
    member_id: LuaMemberId,
    result: &mut Vec<Location>,
) -> Option<()> {
    let mut ctx = ReferenceSearchContext::default();
    let mut semantic_cache = HashMap::new();
    search_semantic_references_with_ctx(
        &mut ctx,
        compilation,
        &mut semantic_cache,
        LuaSemanticDeclId::Member(member_id),
        result,
    )
}

fn search_member_references_with_ctx<'a>(
    ctx: &mut ReferenceSearchContext,
    semantic_model: &SemanticModel<'a>,
    compilation: &'a LuaCompilation,
    semantic_cache: &mut HashMap<FileId, Arc<SemanticModel<'a>>>,
    member_id: LuaMemberId,
    result: &mut Vec<Location>,
    worklist: &mut Vec<LuaSemanticDeclId>,
) -> Option<()> {
    let member = semantic_model
        .get_db()
        .get_member_index()
        .get_member(&member_id)?;
    let key = member.get_key();
    let index_references = semantic_model
        .get_db()
        .get_reference_index()
        .get_index_references(key)?;

    let semantic_id = LuaSemanticDeclId::Member(member_id);
    for in_filed_syntax_id in index_references {
        let reference_semantic_model =
            get_semantic_model_cached(compilation, semantic_cache, in_filed_syntax_id.file_id)?;
        let root = reference_semantic_model.get_root();
        let node = in_filed_syntax_id.value.to_node_from_root(root.syntax())?;
        if reference_semantic_model.is_reference_to(
            node.clone(),
            semantic_id.clone(),
            SemanticDeclLevel::default(),
        ) {
            let document = reference_semantic_model.get_document();
            let range = in_filed_syntax_id.value.get_range();
            let location = document.to_lsp_location(range)?;
            result.push(location);
            let _ = search_member_secondary_references(
                ctx,
                reference_semantic_model.as_ref(),
                node,
                result,
                worklist,
            );
        }
    }

    Some(())
}

fn search_member_secondary_references(
    ctx: &mut ReferenceSearchContext,
    semantic_model: &SemanticModel,
    node: LuaSyntaxNode,
    result: &mut Vec<Location>,
    worklist: &mut Vec<LuaSemanticDeclId>,
) -> Option<()> {
    let position = node.text_range().start();
    let parent = LuaAst::cast(node.parent()?)?;
    match parent {
        LuaAst::LuaAssignStat(assign_stat) => {
            let (vars, values) = assign_stat.get_var_and_expr_list();
            let idx = values
                .iter()
                .position(|value| value.get_position() == position)?;
            let var = vars.get(idx)?;
            let decl_id = LuaDeclId::new(semantic_model.get_file_id(), var.get_position());
            enqueue_semantic_id(ctx, worklist, LuaSemanticDeclId::LuaDecl(decl_id));
            let document = semantic_model.get_document();
            let range = document.to_lsp_location(var.get_range())?;
            result.push(range);
        }
        LuaAst::LuaLocalStat(local_stat) => {
            let local_names = local_stat.get_local_name_list().collect::<Vec<_>>();
            let mut values = local_stat.get_value_exprs();
            let idx = values.position(|value| value.get_position() == position)?;
            let name = local_names.get(idx)?;
            let decl_id = LuaDeclId::new(semantic_model.get_file_id(), name.get_position());
            enqueue_semantic_id(ctx, worklist, LuaSemanticDeclId::LuaDecl(decl_id));
            let document = semantic_model.get_document();
            let range = document.to_lsp_location(name.get_range())?;
            result.push(range);
        }
        _ => {}
    }

    Some(())
}

fn search_string_references(
    semantic_model: &SemanticModel,
    token: LuaStringToken,
    result: &mut Vec<Location>,
) -> Option<()> {
    let string_token_text = token.get_value();
    let string_refs = semantic_model
        .get_db()
        .get_reference_index()
        .get_string_references(&string_token_text);

    for in_filed_reference_range in string_refs {
        let document = semantic_model.get_document_by_file_id(in_filed_reference_range.file_id)?;
        let location = document.to_lsp_location(in_filed_reference_range.value)?;
        result.push(location);
    }

    Some(())
}

fn search_vgui_panel_string_references(
    semantic_model: &SemanticModel,
    compilation: &LuaCompilation,
    token: LuaStringToken,
    result: &mut Vec<Location>,
) -> Option<()> {
    let context = extract_string_call_context(&token)?;
    if !is_vgui_panel_string_context(&context.call_path, context.arg_index) {
        return None;
    }

    for (file_id, call) in semantic_model
        .get_db()
        .get_gmod_class_metadata_index()
        .find_vgui_panel_definitions(&context.name)
    {
        let definition_range = call
            .args
            .first()
            .map(|arg| arg.syntax_id.get_range())
            .unwrap_or(call.syntax_id.get_range());
        let Some(document) = semantic_model.get_document_by_file_id(file_id) else {
            continue;
        };
        let Some(location) = document.to_lsp_location(definition_range) else {
            continue;
        };
        push_unique_location(result, location);
    }

    let string_refs = semantic_model
        .get_db()
        .get_reference_index()
        .get_string_references(&context.name);
    let before_usage_refs = result.len();
    let mut semantic_cache = HashMap::new();
    for in_filed_reference_range in string_refs {
        let Some(reference_semantic_model) = get_semantic_model_cached(
            compilation,
            &mut semantic_cache,
            in_filed_reference_range.file_id,
        ) else {
            continue;
        };

        let root = reference_semantic_model.get_root();
        let Some(reference_token) = root
            .syntax()
            .token_at_offset(in_filed_reference_range.value.start())
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
        if !is_vgui_panel_string_context(&reference_context.call_path, reference_context.arg_index)
            || reference_context.name != context.name
        {
            continue;
        }

        let document = reference_semantic_model.get_document();
        let Some(location) = document.to_lsp_location(in_filed_reference_range.value) else {
            continue;
        };
        push_unique_location(result, location);
    }

    if result.len() == before_usage_refs {
        collect_vgui_context_string_references_from_ast(compilation, &context.name, result);
    }

    Some(())
}

/// Fallback: scans all files' ASTs for VGUI-context string references.
/// Triggered when the string reference index has no results for the panel name
/// (e.g., when short string indexing is disabled or the index is incomplete).
/// This is O(files x calls) but only runs as a last resort.
fn collect_vgui_context_string_references_from_ast(
    compilation: &LuaCompilation,
    panel_name: &str,
    result: &mut Vec<Location>,
) {
    let mut semantic_cache = HashMap::new();
    let file_ids = compilation.get_db().get_vfs().get_all_file_ids();
    for file_id in file_ids {
        let Some(semantic_model) =
            get_semantic_model_cached(compilation, &mut semantic_cache, file_id)
        else {
            continue;
        };

        let root = semantic_model.get_root();
        for call_expr in root.descendants::<LuaCallExpr>() {
            let Some(call_path) = call_expr.get_access_path() else {
                continue;
            };
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
                let Some(name) = normalize_string_name(string_token.get_value()) else {
                    continue;
                };

                if name != panel_name || !is_vgui_panel_string_context(&call_path, arg_index) {
                    continue;
                }

                let Some(location) = semantic_model
                    .get_document()
                    .to_lsp_location(string_token.get_range())
                else {
                    continue;
                };
                push_unique_location(result, location);
            }
        }
    }
}

fn search_net_message_references(
    semantic_model: &SemanticModel,
    token: LuaStringToken,
    result: &mut Vec<Location>,
) -> Option<()> {
    let context = extract_string_call_context(&token)?;
    let _ = net_message_call_kind(&context.call_path, context.arg_index)?;

    let network_index = semantic_model.get_db().get_gmod_network_index();

    for (file_id, flow) in network_index.get_send_flows_for_message(&context.name) {
        let Some(document) = semantic_model.get_document_by_file_id(file_id) else {
            continue;
        };
        let Some(location) = document.to_lsp_location(flow.start_range) else {
            continue;
        };
        push_unique_location(result, location);
    }

    for (file_id, flow) in network_index.get_receive_flows_for_message(&context.name) {
        let Some(document) = semantic_model.get_document_by_file_id(file_id) else {
            continue;
        };
        let Some(location) = document.to_lsp_location(flow.receive_range) else {
            continue;
        };
        push_unique_location(result, location);
    }

    Some(())
}

fn push_unique_location(result: &mut Vec<Location>, location: Location) {
    if !result
        .iter()
        .any(|existing| existing.uri == location.uri && existing.range == location.range)
    {
        result.push(location);
    }
}

fn fuzzy_search_references(
    compilation: &LuaCompilation,
    token: LuaSyntaxToken,
    result: &mut Vec<Location>,
) -> Option<()> {
    let name = LuaNameToken::cast(token)?;
    let name_text = name.get_name_text();
    let fuzzy_references = compilation
        .get_db()
        .get_reference_index()
        .get_index_references(&LuaMemberKey::Name(name_text.to_string().into()))?;

    let mut semantic_cache = HashMap::new();
    for in_filed_syntax_id in fuzzy_references {
        let semantic_model =
            if let Some(semantic_model) = semantic_cache.get_mut(&in_filed_syntax_id.file_id) {
                semantic_model
            } else {
                let semantic_model = compilation.get_semantic_model(in_filed_syntax_id.file_id)?;
                semantic_cache.insert(in_filed_syntax_id.file_id, semantic_model);
                semantic_cache.get_mut(&in_filed_syntax_id.file_id)?
            };

        let document = semantic_model.get_document();
        let range = in_filed_syntax_id.value.get_range();
        let location = document.to_lsp_location(range)?;
        result.push(location);
    }

    Some(())
}

fn search_type_decl_references(
    semantic_model: &SemanticModel,
    type_decl_id: LuaTypeDeclId,
    result: &mut Vec<Location>,
) -> Option<()> {
    let refs = semantic_model
        .get_db()
        .get_reference_index()
        .get_type_references(&type_decl_id)?;
    let mut document_cache = HashMap::new();
    for in_filed_reference_range in refs {
        let document = if let Some(document) = document_cache.get(&in_filed_reference_range.file_id)
        {
            document
        } else {
            let document =
                semantic_model.get_document_by_file_id(in_filed_reference_range.file_id)?;
            document_cache.insert(in_filed_reference_range.file_id, document);
            document_cache.get(&in_filed_reference_range.file_id)?
        };
        let location = document.to_lsp_location(in_filed_reference_range.value)?;
        result.push(location);
    }

    Some(())
}

fn enqueue_value_alias_references(
    ctx: &mut ReferenceSearchContext,
    semantic_model: &SemanticModel,
    decl_ref: &DeclReferenceCell,
    worklist: &mut Vec<LuaSemanticDeclId>,
) -> Option<()> {
    let root = semantic_model.get_root();
    let position = decl_ref.range.start();
    let token = root.syntax().token_at_offset(position).right_biased()?;
    let parent = token.parent()?;

    match parent.parent()? {
        assign_stat_node if LuaAssignStat::can_cast(assign_stat_node.kind().into()) => {
            let assign_stat = LuaAssignStat::cast(assign_stat_node)?;
            let (vars, values) = assign_stat.get_var_and_expr_list();
            let idx = values
                .iter()
                .position(|value| value.get_position() == position)?;
            let var = vars.get(idx)?;
            let decl_id = semantic_model
                .find_decl(var.syntax().clone().into(), SemanticDeclLevel::default())?;
            if let LuaSemanticDeclId::Member(member_id) = decl_id {
                enqueue_semantic_id(ctx, worklist, LuaSemanticDeclId::Member(member_id));
            }
        }
        table_field_node if LuaTableField::can_cast(table_field_node.kind().into()) => {
            let table_field = LuaTableField::cast(table_field_node)?;
            let decl_id = semantic_model.find_decl(
                table_field.syntax().clone().into(),
                SemanticDeclLevel::default(),
            )?;
            if let LuaSemanticDeclId::Member(member_id) = decl_id {
                enqueue_semantic_id(ctx, worklist, LuaSemanticDeclId::Member(member_id));
            }
        }
        _ => {}
    }

    Some(())
}

/// 如果是模块导出, 那么我们需要找到所有引用了这个模块的变量
fn extend_module_return_value_references<'a>(
    ctx: &mut ReferenceSearchContext,
    semantic_model: &SemanticModel<'a>,
    compilation: &'a LuaCompilation,
    semantic_cache: &mut HashMap<FileId, Arc<SemanticModel<'a>>>,
    decl_id: LuaDeclId,
    result: &mut Vec<Location>,
    worklist: &mut Vec<LuaSemanticDeclId>,
) -> Option<()> {
    let module_file_id = decl_id.file_id;
    let module_info = semantic_model
        .get_db()
        .get_module_index()
        .get_module(module_file_id)?;
    if module_info.semantic_id.as_ref() != Some(&LuaSemanticDeclId::LuaDecl(decl_id)) {
        return Some(());
    }

    if !ctx.visited_module_exports.insert(module_file_id) {
        return Some(());
    }

    let file_dependency = semantic_model
        .get_db()
        .get_file_dependencies_index()
        .get_file_dependencies();
    let mut dependents = file_dependency.collect_file_dependents(vec![module_file_id]);
    dependents.sort();
    let mut visited_bindings: HashSet<LuaSemanticDeclId> = HashSet::new();

    for dependent_file_id in dependents {
        let dependent_semantic_model =
            get_semantic_model_cached(compilation, semantic_cache, dependent_file_id)?;

        let root = dependent_semantic_model.get_root();
        for node in root.descendants::<LuaAst>() {
            let LuaAst::LuaCallExpr(call_expr) = node else {
                continue;
            };

            if !call_expr.is_require() {
                continue;
            }

            if resolve_require_target_file_id(dependent_semantic_model.as_ref(), &call_expr)
                != Some(module_file_id)
            {
                continue;
            }

            if let Some(binding_semantic) =
                find_require_call_binding_semantic(dependent_semantic_model.as_ref(), &call_expr)
            {
                if !visited_bindings.insert(binding_semantic.clone()) {
                    continue;
                }

                match binding_semantic {
                    LuaSemanticDeclId::LuaDecl(_) | LuaSemanticDeclId::Member(_) => {
                        enqueue_semantic_id(ctx, worklist, binding_semantic);
                    }
                    _ => {}
                }
            } else {
                let document = dependent_semantic_model.get_document();
                let location = document.to_lsp_location(call_expr.get_range())?;
                result.push(location);
            }
        }
    }

    Some(())
}

fn resolve_require_target_file_id(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
) -> Option<FileId> {
    let args = call_expr.get_args_list()?;
    let first_arg = args.get_args().next()?;
    let require_path_type = semantic_model.infer_expr(first_arg).ok()?;
    let module_path: String = match &require_path_type {
        LuaType::StringConst(module_path) => module_path.as_ref().to_string(),
        _ => return None,
    };

    let module_info = semantic_model
        .get_db()
        .get_module_index()
        .find_module(&module_path)?;
    Some(module_info.file_id)
}

fn find_require_call_binding_semantic(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
) -> Option<LuaSemanticDeclId> {
    let position = call_expr.get_position();

    let mut current = call_expr.syntax().parent();
    while let Some(node) = current {
        let Some(parent) = LuaAst::cast(node.clone()) else {
            current = node.parent();
            continue;
        };

        match parent {
            LuaAst::LuaLocalStat(local_stat) => {
                let local_names = local_stat.get_local_name_list().collect::<Vec<_>>();
                let mut values = local_stat.get_value_exprs();
                let idx = values.position(|value| value.get_position() == position)?;
                let name = local_names.get(idx)?;
                return Some(LuaSemanticDeclId::LuaDecl(LuaDeclId::new(
                    semantic_model.get_file_id(),
                    name.get_position(),
                )));
            }
            LuaAst::LuaAssignStat(assign_stat) => {
                let (vars, values) = assign_stat.get_var_and_expr_list();
                let idx = values
                    .iter()
                    .position(|value| value.get_position() == position)?;
                let var = vars.get(idx)?;
                return semantic_model
                    .find_decl(var.syntax().clone().into(), SemanticDeclLevel::default());
            }
            _ => {}
        }

        current = node.parent();
    }

    None
}

#[allow(unused)]
fn filter_duplicate_and_covered_locations(locations: Vec<Location>) -> Vec<Location> {
    if locations.is_empty() {
        return locations;
    }
    let mut sorted_locations = locations;
    sorted_locations.sort_by(|a, b| {
        a.uri
            .to_string()
            .cmp(&b.uri.to_string())
            .then_with(|| a.range.start.line.cmp(&b.range.start.line))
            .then_with(|| b.range.end.line.cmp(&a.range.end.line))
    });

    let mut result = Vec::new();
    let mut seen_lines_by_uri: HashMap<String, HashSet<u32>> = HashMap::new();

    for location in sorted_locations {
        let uri_str = location.uri.to_string();
        let seen_lines = seen_lines_by_uri.entry(uri_str).or_default();

        let start_line = location.range.start.line;
        let end_line = location.range.end.line;

        let is_covered = (start_line..=end_line).any(|line| seen_lines.contains(&line));

        if !is_covered {
            for line in start_line..=end_line {
                seen_lines.insert(line);
            }
            result.push(location);
        }
    }

    // 最终按位置排序
    result.sort_by(|a, b| {
        a.uri
            .to_string()
            .cmp(&b.uri.to_string())
            .then_with(|| a.range.start.line.cmp(&b.range.start.line))
            .then_with(|| a.range.start.character.cmp(&b.range.start.character))
    });

    result
}

fn enqueue_semantic_id(
    ctx: &mut ReferenceSearchContext,
    worklist: &mut Vec<LuaSemanticDeclId>,
    semantic_id: LuaSemanticDeclId,
) {
    if ctx.visited_semantic_ids.insert(semantic_id.clone()) {
        worklist.push(semantic_id);
    }
}

fn get_semantic_model_cached<'a>(
    compilation: &'a LuaCompilation,
    semantic_cache: &mut HashMap<FileId, Arc<SemanticModel<'a>>>,
    file_id: FileId,
) -> Option<Arc<SemanticModel<'a>>> {
    if let Some(cached) = semantic_cache.get(&file_id) {
        return Some(Arc::clone(cached));
    }

    let semantic_model = Arc::new(compilation.get_semantic_model(file_id)?);
    semantic_cache.insert(file_id, Arc::clone(&semantic_model));
    Some(semantic_model)
}

fn search_semantic_references_with_ctx<'a>(
    ctx: &mut ReferenceSearchContext,
    compilation: &'a LuaCompilation,
    semantic_cache: &mut HashMap<FileId, Arc<SemanticModel<'a>>>,
    start: LuaSemanticDeclId,
    result: &mut Vec<Location>,
) -> Option<()> {
    let mut worklist = Vec::new();
    if ctx.visited_semantic_ids.insert(start.clone()) {
        worklist.push(start);
    } else {
        return Some(());
    }

    let mut first = true;
    let mut start_ret = Some(());

    while let Some(semantic_id) = worklist.pop() {
        let ret = match semantic_id {
            LuaSemanticDeclId::LuaDecl(decl_id) => {
                match get_semantic_model_cached(compilation, semantic_cache, decl_id.file_id) {
                    Some(semantic_model) => search_decl_references_with_ctx(
                        ctx,
                        semantic_model.as_ref(),
                        compilation,
                        semantic_cache,
                        decl_id,
                        result,
                        &mut worklist,
                    ),
                    None => None,
                }
            }
            LuaSemanticDeclId::Member(member_id) => {
                match get_semantic_model_cached(compilation, semantic_cache, member_id.file_id) {
                    Some(semantic_model) => search_member_references_with_ctx(
                        ctx,
                        semantic_model.as_ref(),
                        compilation,
                        semantic_cache,
                        member_id,
                        result,
                        &mut worklist,
                    ),
                    None => None,
                }
            }
            _ => Some(()),
        };

        if first {
            start_ret = ret;
            first = false;
        }
    }

    start_ret
}
