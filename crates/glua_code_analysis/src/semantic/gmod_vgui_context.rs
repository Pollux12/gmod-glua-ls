use crate::{
    DbIndex, FileId, InFiled, LuaDeclId, LuaSignatureId, LuaType, LuaTypeOwner,
    db_index::GmodScriptedClassCallMetadata,
};
use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaAstToken, LuaExpr, LuaFuncStat, LuaLocalName, LuaLocalStat,
    LuaNameExpr, LuaTableExpr, LuaVarExpr,
};
use rowan::TextSize;

use super::LuaInferCache;

pub(crate) struct RegisteredVguiMethodContext {
    pub panel_name: String,
    pub receiver_signature_id: LuaSignatureId,
}

pub(crate) fn resolve_registered_vgui_method_context(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: &LuaNameExpr,
) -> Option<RegisteredVguiMethodContext> {
    let file_id = cache.get_file_id();
    let (receiver_decl_id, receiver_position, receiver_signature_id) =
        find_enclosing_panel_receiver_context(db, file_id, name_expr)?;
    let metadata = db.get_gmod_class_metadata_index();
    let file_metadata = metadata.get_file_metadata(&file_id)?;

    for call in file_metadata
        .derma_define_control_calls
        .iter()
        .chain(file_metadata.vgui_register_calls.iter())
    {
        let Some((table_decl_id, region_start, register_position)) =
            resolve_call_table_registration_region(db, file_id, call)
        else {
            continue;
        };
        if table_decl_id == receiver_decl_id
            && receiver_position >= region_start
            && receiver_position < register_position
            && let Some(panel_name) =
                synthesized_panel_name_for_registration(db, file_id, region_start, call)
        {
            return Some(RegisteredVguiMethodContext {
                panel_name,
                receiver_signature_id,
            });
        }
    }

    None
}

fn find_enclosing_panel_receiver_context(
    db: &DbIndex,
    file_id: FileId,
    name_expr: &LuaNameExpr,
) -> Option<(LuaDeclId, TextSize, LuaSignatureId)> {
    for func_stat in name_expr.ancestors::<LuaFuncStat>() {
        let Some(LuaVarExpr::IndexExpr(index_expr)) = func_stat.get_func_name() else {
            continue;
        };
        if !index_expr
            .get_index_token()
            .is_some_and(|token| token.is_colon())
        {
            continue;
        }
        let Some(LuaExpr::NameExpr(prefix_name)) = index_expr.get_prefix_expr() else {
            continue;
        };
        let range = prefix_name.get_range();
        let decl_id = db
            .get_reference_index()
            .get_local_reference(&file_id)?
            .get_decl_id(&range)?;
        let closure = func_stat.get_closure()?;
        return Some((
            decl_id,
            range.start(),
            LuaSignatureId::from_closure(file_id, &closure),
        ));
    }
    None
}

fn synthesized_panel_name_for_registration(
    db: &DbIndex,
    file_id: FileId,
    region_start: TextSize,
    call: &GmodScriptedClassCallMetadata,
) -> Option<String> {
    let table_expr = find_table_expr_at_write_position(db, file_id, region_start)?;
    let owner = LuaTypeOwner::SyntaxId(InFiled::new(file_id, table_expr.get_syntax_id()));
    let type_cache = db.get_type_index().get_type_cache(&owner)?;
    if !type_cache.is_infer() {
        return None;
    }
    let LuaType::Def(type_id) = type_cache.as_type() else {
        return None;
    };
    let call_range = call.syntax_id.get_range();
    let type_decl = db.get_type_index().get_type_decl(type_id)?;
    type_decl
        .get_locations()
        .iter()
        .any(|location| location.file_id == file_id && location.range == call_range)
        .then(|| type_id.get_name().to_string())
}

fn find_table_expr_at_write_position(
    db: &DbIndex,
    file_id: FileId,
    write_position: TextSize,
) -> Option<LuaTableExpr> {
    let chunk = db.get_vfs().get_syntax_tree(&file_id)?.get_chunk_node();
    let name_token = chunk
        .syntax()
        .token_at_offset(write_position)
        .right_biased()?;

    for ancestor in name_token.parent_ancestors() {
        if let Some(local_stat) = LuaLocalStat::cast(ancestor.clone()) {
            let names = local_stat
                .get_local_name_list()
                .collect::<Vec<LuaLocalName>>();
            let values = local_stat.get_value_exprs().collect::<Vec<LuaExpr>>();
            let index = names.iter().position(|name| {
                name.get_name_token()
                    .is_some_and(|token| token.syntax().text_range().start() == write_position)
            })?;
            return value_expr_as_table(values.get(index)?);
        }

        if let Some(assign_stat) = LuaAssignStat::cast(ancestor) {
            let (vars, exprs) = assign_stat.get_var_and_expr_list();
            let index = vars
                .iter()
                .position(|var| var.syntax().text_range().start() == write_position)?;
            return value_expr_as_table(exprs.get(index)?);
        }
    }
    None
}

fn value_expr_as_table(expr: &LuaExpr) -> Option<LuaTableExpr> {
    let mut current = expr.clone();
    loop {
        match current {
            LuaExpr::TableExpr(table) => return Some(table),
            LuaExpr::ParenExpr(paren) => current = paren.get_expr()?,
            _ => return None,
        }
    }
}

fn resolve_call_table_registration_region(
    db: &DbIndex,
    file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
) -> Option<(LuaDeclId, TextSize, TextSize)> {
    let table_source = call.vgui_panel_roles.as_ref()?.table.as_ref()?;
    let arg = call.args.get(table_source.arg_idx)?;
    let range = arg.syntax_id.get_range();
    let register_position = call.syntax_id.get_range().start();
    let decl_id = db
        .get_reference_index()
        .get_local_reference(&file_id)?
        .get_decl_id(&range)?;
    let region_start =
        find_latest_decl_write_before_position(db, file_id, decl_id, register_position)
            .unwrap_or(decl_id.position);
    Some((decl_id, region_start, register_position))
}

fn find_latest_decl_write_before_position(
    db: &DbIndex,
    file_id: FileId,
    decl_id: LuaDeclId,
    position: TextSize,
) -> Option<TextSize> {
    db.get_reference_index()
        .get_decl_references(&file_id, &decl_id)
        .and_then(|references| {
            references
                .cells
                .iter()
                .filter(|cell| cell.is_write && cell.range.start() < position)
                .max_by_key(|cell| cell.range.start())
                .map(|cell| cell.range.start())
        })
}
