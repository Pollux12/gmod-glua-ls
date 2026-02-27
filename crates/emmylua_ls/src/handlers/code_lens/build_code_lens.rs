use emmylua_code_analysis::{
    LuaDeclId, LuaMemberId, LuaMemberOwner, LuaType, LuaTypeDeclId, SemanticModel,
};
use emmylua_parser::{
    LuaAssignStat, LuaAst, LuaAstNode, LuaAstToken, LuaFuncStat, LuaLocalFuncStat, LuaLocalStat,
    LuaVarExpr,
};
use lsp_types::{CodeLens, Command, Range};
use rowan::NodeOrToken;

use super::CodeLensData;

pub fn build_code_lens(semantic_model: &SemanticModel) -> Option<Vec<CodeLens>> {
    let mut result = Vec::new();
    let enable_vgui_code_lens = semantic_model.get_emmyrc().gmod.vgui.code_lens_enabled;
    let root = semantic_model.get_root().clone();
    for node in root.descendants::<LuaAst>() {
        match node {
            LuaAst::LuaFuncStat(func_stat) => {
                add_func_stat_code_lens(
                    semantic_model,
                    &mut result,
                    func_stat,
                    enable_vgui_code_lens,
                )?;
            }
            LuaAst::LuaLocalFuncStat(local_func_stat) => {
                add_local_func_stat_code_lens(
                    semantic_model,
                    &mut result,
                    local_func_stat,
                    enable_vgui_code_lens,
                )?;
            }
            LuaAst::LuaLocalStat(local_stat) => {
                if enable_vgui_code_lens {
                    add_local_stat_code_lens(semantic_model, &mut result, local_stat)?;
                }
            }
            LuaAst::LuaAssignStat(assign_stat) => {
                if enable_vgui_code_lens {
                    add_assign_stat_code_lens(semantic_model, &mut result, assign_stat)?;
                }
            }
            _ => {}
        }
    }

    Some(result)
}

fn add_func_stat_code_lens(
    semantic_model: &SemanticModel,
    result: &mut Vec<CodeLens>,
    func_stat: LuaFuncStat,
    enable_vgui_code_lens: bool,
) -> Option<()> {
    let file_id = semantic_model.get_file_id();
    let func_name = func_stat.get_func_name()?;
    let document = semantic_model.get_document();
    match func_name {
        LuaVarExpr::IndexExpr(index_expr) => {
            let member_id = LuaMemberId::new(index_expr.get_syntax_id(), file_id);
            let data = CodeLensData::Member(member_id);
            let index_name_token = index_expr.get_index_name_token()?;
            let range = document.to_lsp_range(index_name_token.text_range())?;
            result.push(CodeLens {
                range: range.clone(),
                command: None,
                data: Some(serde_json::to_value(data).unwrap()),
            });

            if enable_vgui_code_lens
                && let Some(owner) = semantic_model
                    .get_db()
                    .get_member_index()
                    .get_member_owner(&member_id)
                && let Some((panel_name, base_name)) =
                    find_vgui_panel_from_member_owner(semantic_model, owner)
            {
                push_vgui_panel_code_lens(result, range, &panel_name, base_name.as_deref());
            }
        }
        LuaVarExpr::NameExpr(name_expr) => {
            let name_token = name_expr.get_name_token()?;
            let decl_id = LuaDeclId::new(file_id, name_token.get_position());
            let data = CodeLensData::DeclId(decl_id);
            let range = document.to_lsp_range(name_token.get_range())?;
            result.push(CodeLens {
                range: range.clone(),
                command: None,
                data: Some(serde_json::to_value(data).unwrap()),
            });

            if enable_vgui_code_lens
                && let Some((panel_name, base_name)) =
                    find_vgui_panel_from_decl(semantic_model, decl_id)
            {
                push_vgui_panel_code_lens(result, range, &panel_name, base_name.as_deref());
            }
        }
    }

    Some(())
}

fn add_local_func_stat_code_lens(
    semantic_model: &SemanticModel,
    result: &mut Vec<CodeLens>,
    local_func_stat: LuaLocalFuncStat,
    enable_vgui_code_lens: bool,
) -> Option<()> {
    let file_id = semantic_model.get_file_id();
    let func_name = local_func_stat.get_local_name()?;
    let document = semantic_model.get_document();
    let range = document.to_lsp_range(func_name.get_range())?;
    let name_token = func_name.get_name_token()?;
    let decl_id = LuaDeclId::new(file_id, name_token.get_position());
    let data = CodeLensData::DeclId(decl_id);
    result.push(CodeLens {
        range: range.clone(),
        command: None,
        data: Some(serde_json::to_value(data).unwrap()),
    });

    if enable_vgui_code_lens
        && let Some((panel_name, base_name)) = find_vgui_panel_from_decl(semantic_model, decl_id)
    {
        push_vgui_panel_code_lens(result, range, &panel_name, base_name.as_deref());
    }

    Some(())
}

fn add_local_stat_code_lens(
    semantic_model: &SemanticModel,
    result: &mut Vec<CodeLens>,
    local_stat: LuaLocalStat,
) -> Option<()> {
    let file_id = semantic_model.get_file_id();
    let document = semantic_model.get_document();

    for local_name in local_stat.get_local_name_list() {
        let Some(name_token) = local_name.get_name_token() else {
            continue;
        };

        let decl_id = LuaDeclId::new(file_id, name_token.get_position());
        let Some((panel_name, base_name)) = find_vgui_panel_from_decl(semantic_model, decl_id)
        else {
            continue;
        };

        let range = document.to_lsp_range(name_token.get_range())?;
        push_vgui_panel_code_lens(result, range, &panel_name, base_name.as_deref());
    }

    Some(())
}

fn add_assign_stat_code_lens(
    semantic_model: &SemanticModel,
    result: &mut Vec<CodeLens>,
    assign_stat: LuaAssignStat,
) -> Option<()> {
    let document = semantic_model.get_document();
    let (vars, _) = assign_stat.get_var_and_expr_list();

    for var in vars {
        let Some(semantic_info) =
            semantic_model.get_semantic_info(NodeOrToken::Node(var.syntax().clone()))
        else {
            continue;
        };
        let Some((panel_name, base_name)) =
            find_vgui_panel_from_type(semantic_model, &semantic_info.typ)
        else {
            continue;
        };

        let range = document.to_lsp_range(var.get_range())?;
        push_vgui_panel_code_lens(result, range, &panel_name, base_name.as_deref());
    }

    Some(())
}

fn find_vgui_panel_from_decl(
    semantic_model: &SemanticModel,
    decl_id: LuaDeclId,
) -> Option<(String, Option<String>)> {
    let typ = semantic_model
        .get_db()
        .get_type_index()
        .get_type_cache(&decl_id.into())?
        .as_type();

    find_vgui_panel_from_type(semantic_model, typ)
}

fn find_vgui_panel_from_member_owner(
    semantic_model: &SemanticModel,
    owner: &LuaMemberOwner,
) -> Option<(String, Option<String>)> {
    match owner {
        LuaMemberOwner::Type(type_id) => find_vgui_panel_from_type_id(semantic_model, type_id),
        _ => None,
    }
}

fn find_vgui_panel_from_type(
    semantic_model: &SemanticModel,
    typ: &LuaType,
) -> Option<(String, Option<String>)> {
    match typ {
        LuaType::Def(type_id) => find_vgui_panel_from_type_id(semantic_model, type_id),
        _ => None,
    }
}

fn find_vgui_panel_from_type_id(
    semantic_model: &SemanticModel,
    type_id: &LuaTypeDeclId,
) -> Option<(String, Option<String>)> {
    let panel_name = type_id.get_simple_name().to_string();
    let base_name = semantic_model
        .get_db()
        .get_gmod_class_metadata_index()
        .get_vgui_panel_base(&panel_name)?;

    Some((panel_name, base_name))
}

fn push_vgui_panel_code_lens(
    result: &mut Vec<CodeLens>,
    range: Range,
    panel_name: &str,
    base_name: Option<&str>,
) {
    let title = match base_name {
        Some(base_name) => format!("VGUI Panel: {panel_name} (Base: {base_name})"),
        None => format!("VGUI Panel: {panel_name}"),
    };

    result.push(CodeLens {
        range,
        command: Some(Command {
            title,
            command: String::new(),
            arguments: None,
        }),
        data: None,
    });
}
