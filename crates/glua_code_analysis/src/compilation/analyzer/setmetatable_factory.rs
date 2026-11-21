use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaExpr, LuaFuncStat, LuaIndexExpr, LuaLocalStat, LuaVarExpr,
};
use rowan::{TextRange, TextSize};

use crate::{
    FileId, LuaMemberId, LuaMemberKey, LuaType,
    compilation::analyzer::common::add_member,
    db_index::{DbIndex, LuaMemberOwner, SetmetatableFactoryBinding},
};

pub fn synthesize_setmetatable_factory_members(db: &mut DbIndex, file_ids: &[FileId]) {
    let _p = crate::profile::PhaseGuard::new("setmetatable_factory");
    let mut bindings = file_ids
        .iter()
        .filter_map(|file_id| db.get_metatable_index().factory_bindings_for_file(*file_id))
        .flatten()
        .cloned()
        .collect::<Vec<_>>();

    bindings.sort_by_key(|binding| (binding.file_id.id, u32::from(binding.call_position)));

    for binding in bindings {
        let Some(class_owner) = prototype_owner(db, &binding) else {
            continue;
        };
        let Some(member_ids) = transferable_factory_member_ids(db, &binding) else {
            continue;
        };
        for member_id in member_ids {
            add_member(db, class_owner.clone(), member_id);
        }
    }
}

fn transferable_factory_member_ids(
    db: &DbIndex,
    binding: &SetmetatableFactoryBinding,
) -> Option<Vec<LuaMemberId>> {
    if let Some(local_name) = &binding.local_name
        && factory_local_has_blocking_write_or_alias(db, binding, local_name)
    {
        return None;
    }

    let source_owner = LuaMemberOwner::Element(binding.table_range.clone());
    let members = db.get_member_index().get_members(&source_owner)?;
    let mut member_ids = Vec::new();

    for member in members {
        let member_id = member.get_id();
        let member_position = member_id.get_position();
        let in_constructor = binding.table_range.value.contains(member_position);
        if member_id.file_id != binding.file_id
            || (member_position >= binding.call_position && !in_constructor)
        {
            continue;
        }

        let member_scope = db.get_member_index().member_function_scope_range(member_id);
        let direct_member = in_constructor
            || binding.local_name.as_ref().is_some_and(|local_name| {
                member_defined_via_variable_strict(db, binding.file_id, member_position, local_name)
            });

        if !direct_member || (!in_constructor && member_scope != Some(binding.function_scope)) {
            // Any field collected for the factory table through an alias or a
            // nested closure makes the constructor ambiguous. Transfer nothing.
            return None;
        }

        member_ids.push(member_id);
    }

    member_ids.sort_by_key(|id| (id.file_id.id, u32::from(id.get_position())));
    member_ids.dedup();
    Some(member_ids)
}

/// The owner of the table the metatable's `__index` names: the class whose
/// methods read what each instance the constructor makes starts with.
fn prototype_owner(db: &DbIndex, binding: &SetmetatableFactoryBinding) -> Option<LuaMemberOwner> {
    let owner = crate::semantic::metatable_member_owner(db, &binding.metatable_range);
    let key = LuaMemberKey::Name("__index".into());
    db.get_member_index()
        .get_current_owner_members_for_key(&owner, &key)
        .into_iter()
        .find_map(|member| {
            db.get_type_index()
                .get_type_cache(&member.get_id().into())
                .and_then(|cache| table_owner(db, cache.as_type()))
        })
}

fn table_owner(db: &DbIndex, typ: &LuaType) -> Option<LuaMemberOwner> {
    match typ {
        LuaType::TableConst(range) => Some(crate::semantic::metatable_member_owner(db, range)),
        LuaType::Instance(instance) => Some(crate::semantic::metatable_member_owner(
            db,
            instance.get_range(),
        )),
        LuaType::Def(id) | LuaType::Ref(id) => Some(LuaMemberOwner::Type(id.clone())),
        LuaType::TypeGuard(inner) => table_owner(db, inner),
        _ => None,
    }
}

fn factory_local_has_blocking_write_or_alias(
    db: &DbIndex,
    binding: &SetmetatableFactoryBinding,
    local_name: &str,
) -> bool {
    let Some(tree) = db.get_vfs().get_syntax_tree(&binding.file_id) else {
        return true;
    };
    let chunk = tree.get_chunk_node();

    for assign_stat in chunk.descendants::<LuaAssignStat>() {
        if !stat_before_call_in_scope(assign_stat.get_range(), binding) {
            continue;
        }
        let (vars, exprs) = assign_stat.get_var_and_expr_list();
        if vars.iter().any(|var| var_is_name(var, local_name)) {
            return true;
        }
        if exprs.iter().any(|expr| expr_is_name(expr, local_name)) {
            return true;
        }
    }

    for local_stat in chunk.descendants::<LuaLocalStat>() {
        if !stat_before_call_in_scope(local_stat.get_range(), binding) {
            continue;
        }
        if local_stat
            .get_value_exprs()
            .any(|expr| expr_is_name(&expr, local_name))
        {
            return true;
        }
    }

    false
}

fn stat_before_call_in_scope(range: TextRange, binding: &SetmetatableFactoryBinding) -> bool {
    range.start() < binding.call_position && range_in_scope(range, binding.function_scope)
}

fn range_in_scope(range: TextRange, scope: TextRange) -> bool {
    range.start() >= scope.start() && range.end() <= scope.end()
}

fn var_is_name(var: &LuaVarExpr, name: &str) -> bool {
    matches!(var, LuaVarExpr::NameExpr(name_expr) if name_expr.get_name_text().as_deref() == Some(name))
}

fn expr_is_name(expr: &LuaExpr, name: &str) -> bool {
    matches!(expr, LuaExpr::NameExpr(name_expr) if name_expr.get_name_text().as_deref() == Some(name))
}

fn member_defined_via_variable_strict(
    db: &DbIndex,
    file_id: FileId,
    member_position: TextSize,
    var_name: &str,
) -> bool {
    let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) else {
        return false;
    };
    let chunk = tree.get_chunk_node();
    let Some(token) = chunk
        .syntax()
        .token_at_offset(member_position)
        .right_biased()
    else {
        return false;
    };

    for ancestor in token.parent_ancestors() {
        if let Some(func_stat) = LuaFuncStat::cast(ancestor.clone()) {
            return matches!(func_stat.get_func_name(), Some(LuaVarExpr::IndexExpr(index_expr)) if index_expr_prefix_matches(&index_expr, var_name));
        }
        if let Some(assign_stat) = LuaAssignStat::cast(ancestor) {
            let (vars, _) = assign_stat.get_var_and_expr_list();
            return vars.iter().any(|var| {
                matches!(var, LuaVarExpr::IndexExpr(index_expr) if index_expr_prefix_matches(index_expr, var_name))
            });
        }
    }

    false
}

fn index_expr_prefix_matches(index_expr: &LuaIndexExpr, var_name: &str) -> bool {
    matches!(index_expr.get_prefix_expr(), Some(LuaExpr::NameExpr(prefix)) if prefix.get_name_text().as_deref() == Some(var_name))
}
