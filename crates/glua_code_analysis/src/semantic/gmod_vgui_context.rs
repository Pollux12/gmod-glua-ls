use crate::{
    DbIndex, FileId, GMOD_DOMAIN_VGUI_PANEL, GMOD_ROLE_VGUI_OWN_CLASS, LuaDeclId, LuaMemberKey,
    LuaMemberOwner, LuaSignatureId, LuaType, LuaTypeDeclId,
    db_index::GmodScriptedClassCallMetadata, find_best_direct_call_arg_role_for_param,
};
use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaClosureExpr, LuaExpr, LuaFuncStat, LuaIndexExpr, LuaIndexKey,
    LuaNameExpr, LuaVarExpr,
};
use rowan::{TextRange, TextSize};
use smol_str::SmolStr;

use super::LuaInferCache;

pub(crate) struct RegisteredVguiMethodContext {
    pub panel_name: String,
    pub receiver_signature_id: LuaSignatureId,
    pub method_name: String,
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
    let mut registrations = db
        .get_gmod_class_metadata_index()
        .get_vgui_registrations(&file_id)
        .peekable();
    registrations.peek()?;
    let (receiver_decl_id, receiver_position, receiver_signature_id, method_name) =
        panel_receiver_context_for_func(db, file_id, func_stat)?;

    for call in registrations {
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
                method_name: method_name.clone(),
            });
        }
    }

    None
}

fn panel_receiver_context_for_func(
    db: &DbIndex,
    file_id: FileId,
    func_stat: &LuaFuncStat,
) -> Option<(LuaDeclId, TextSize, LuaSignatureId, String)> {
    let LuaVarExpr::IndexExpr(index_expr) = func_stat.get_func_name()? else {
        return None;
    };
    if !index_expr
        .get_index_token()
        .is_some_and(|token| token.is_colon())
    {
        return None;
    }
    let Some(LuaIndexKey::Name(method_token)) = index_expr.get_index_key() else {
        return None;
    };
    let method_name = method_token.get_name_text().to_string();
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
        method_name,
    ))
}

/// Whether the method the context names overrides a signature that marks
/// `param_idx` with `call_arg("gmod.vgui_panel", "own_class")`.
///
/// The method is looked up on the registered panel class and every class it
/// inherits from, nearest first. Any declaration on that chain decides it:
/// the engine fills the parameter whatever an override declares, so the
/// user's own override (which carries no role) still gets the inherited
/// declaration's — e.g. the annotated `Panel:GenerateExample` whose `class`
/// parameter the engine fills with the panel's registered class name.
pub(crate) fn registered_vgui_method_param_is_own_class(
    db: &DbIndex,
    context: &RegisteredVguiMethodContext,
    param_idx: usize,
) -> bool {
    let class_id = LuaTypeDeclId::global(&context.panel_name);
    let mut supers = Vec::new();
    class_id.collect_super_types(db, &mut supers);
    let member_key = LuaMemberKey::Name(SmolStr::new(context.method_name.as_str()));
    for class_id in
        std::iter::once(class_id).chain(supers.into_iter().filter_map(|typ| match typ {
            LuaType::Ref(id) | LuaType::Def(id) => Some(id),
            _ => None,
        }))
    {
        let Some(members) = db
            .get_member_index()
            .get_members_with_key(&LuaMemberOwner::Type(class_id), &member_key)
        else {
            continue;
        };
        for member in members {
            let member_file_id = member.get_file_id();
            let Some(index_expr) = db
                .get_vfs()
                .syntax_node_around(&member_file_id, member.get_syntax_id())
                .and_then(LuaIndexExpr::cast)
            else {
                continue;
            };
            let closure = index_expr
                .get_parent::<LuaFuncStat>()
                .and_then(|stat| stat.get_closure())
                .or_else(|| {
                    let assignment = index_expr.get_parent::<LuaAssignStat>()?;
                    let (vars, exprs) = assignment.get_var_and_expr_list();
                    let position = vars
                        .iter()
                        .position(|var| var.syntax() == index_expr.syntax())?;
                    match exprs.get(position)? {
                        LuaExpr::ClosureExpr(body) => Some(body.clone()),
                        _ => None,
                    }
                });
            let Some(closure) = closure else {
                continue;
            };
            let signature_id = LuaSignatureId::from_closure(member_file_id, &closure);
            let Some(signature) = db.get_signature_index().get_declared(&signature_id) else {
                continue;
            };
            if find_best_direct_call_arg_role_for_param(
                signature,
                param_idx,
                GMOD_DOMAIN_VGUI_PANEL,
                &[GMOD_ROLE_VGUI_OWN_CLASS],
            )
            .is_some()
            {
                return true;
            }
        }
    }
    false
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

pub(crate) fn find_latest_decl_write_before_position(
    db: &DbIndex,
    file_id: FileId,
    decl_id: LuaDeclId,
    position: TextSize,
) -> Option<TextSize> {
    // All RHS expressions read the values from before the enclosing assignment,
    // which lies in the top-level statement around `position`.
    let position = db
        .get_vfs()
        .syntax_tree_around(&file_id, TextRange::at(position, TextSize::new(1)))
        .and_then(|tree| {
            tree.get_chunk_node()
                .syntax()
                .token_at_offset(position)
                .right_biased()?
                .parent_ancestors()
                .take_while(|node| !LuaClosureExpr::can_cast(node.kind().into()))
                .find_map(LuaAssignStat::cast)
                .map(|assignment| assignment.get_range().start())
        })
        .unwrap_or(position);
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
