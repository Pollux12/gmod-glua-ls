use crate::{DbIndex, FileId, LuaDeclId, LuaSignatureId, db_index::GmodScriptedClassCallMetadata};
use glua_parser::{LuaAstNode, LuaExpr, LuaFuncStat, LuaNameExpr, LuaVarExpr};
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
    let func_stat = name_expr.ancestors::<LuaFuncStat>().find(|func_stat| {
        matches!(
            func_stat.get_func_name(),
            Some(LuaVarExpr::IndexExpr(index_expr))
                if index_expr
                    .get_index_token()
                    .is_some_and(|token| token.is_colon())
        )
    })?;
    resolve_registered_vgui_method_context_for_func(db, cache.get_file_id(), &func_stat)
}

pub(crate) fn resolve_registered_vgui_method_context_for_func(
    db: &DbIndex,
    file_id: FileId,
    func_stat: &LuaFuncStat,
) -> Option<RegisteredVguiMethodContext> {
    let metadata = db.get_gmod_class_metadata_index();
    let file_metadata = metadata.get_file_metadata(&file_id)?;
    if file_metadata.derma_define_control_calls.is_empty()
        && file_metadata.vgui_register_calls.is_empty()
    {
        return None;
    }
    let (receiver_decl_id, receiver_position, receiver_signature_id) =
        panel_receiver_context_for_func(db, file_id, func_stat)?;

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
            && let Some(panel_name) = registered_panel_name(call)
        {
            return Some(RegisteredVguiMethodContext {
                panel_name,
                receiver_signature_id,
            });
        }
    }

    None
}

fn panel_receiver_context_for_func(
    db: &DbIndex,
    file_id: FileId,
    func_stat: &LuaFuncStat,
) -> Option<(LuaDeclId, TextSize, LuaSignatureId)> {
    let LuaVarExpr::IndexExpr(index_expr) = func_stat.get_func_name()? else {
        return None;
    };
    if !index_expr
        .get_index_token()
        .is_some_and(|token| token.is_colon())
    {
        return None;
    }
    let LuaExpr::NameExpr(prefix_name) = index_expr.get_prefix_expr()? else {
        return None;
    };
    let range = prefix_name.get_range();
    let decl_id = db
        .get_reference_index()
        .get_local_reference(&file_id)?
        .get_decl_id(&range)?;
    let closure = func_stat.get_closure()?;
    Some((
        decl_id,
        range.start(),
        LuaSignatureId::from_closure(file_id, &closure),
    ))
}

fn registered_panel_name(call: &GmodScriptedClassCallMetadata) -> Option<String> {
    let source = call.vgui_panel_define_arg_source();
    match call.value_for_arg_source(&source) {
        Some(crate::GmodClassCallLiteral::String(name)) if !name.is_empty() => Some(name.clone()),
        _ => None,
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
