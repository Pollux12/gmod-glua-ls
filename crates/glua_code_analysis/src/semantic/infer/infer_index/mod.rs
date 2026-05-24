mod infer_array;
pub(crate) use infer_array::check_iter_var_range;

use std::collections::HashSet;

use glua_parser::{
    LuaAstNode, LuaCallExpr, LuaExpr, LuaIndexExpr, LuaIndexKey, LuaIndexMemberExpr, LuaLocalStat,
    LuaNameExpr, NumberResult, PathTrait,
};
use internment::ArcIntern;
use rowan::{TextRange, TextSize};
use smol_str::SmolStr;

use crate::{
    CacheEntry, FileId, GenericTpl, GlobalId, InFiled, InferGuardRef, LuaAliasCallKind, LuaDeclId,
    LuaDeclOrMemberId, LuaInferCache, LuaInstanceType, LuaMemberOwner, LuaOperatorOwner, TypeOps,
    compilation::get_scripted_class_info_for_file,
    db_index::{
        DbIndex, LuaGenericType, LuaIntersectionType, LuaMemberKey, LuaObjectType,
        LuaOperatorMetaMethod, LuaTupleType, LuaType, LuaTypeDeclId, LuaUnionType,
    },
    enum_variable_is_param, get_keyof_members, get_tpl_ref_extend_type,
    semantic::{
        InferGuard,
        generic::{TypeSubstitutor, instantiate_type_generic},
        infer::{
            VarRefId, infer_index::infer_array::infer_array_member,
            infer_name::get_name_expr_var_ref_id, narrow::infer_expr_narrow_type,
        },
        is_doc_tag_table_const,
        member::get_buildin_type_map_type_id,
        member::infer_owner_raw_member_type_with_realm,
        member::intersect_member_types,
        member::member_key_matches_type,
        member::resolve_dynamic_field_member,
        type_check::{self, check_type_compact},
    },
};

use super::{InferFailReason, InferResult, infer_expr, infer_name::infer_global_type};

pub fn infer_index_expr(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: LuaIndexExpr,
    pass_flow: bool,
) -> InferResult {
    let prefix_expr = index_expr.get_prefix_expr().ok_or(InferFailReason::None)?;
    let mut prefix_is_unresolved_param = false;
    let prefix_type = match infer_expr(db, cache, prefix_expr.clone()) {
        Ok(prefix_type) => prefix_type,
        Err(InferFailReason::UnResolveDeclType(decl_id))
            if is_unresolved_param_decl(db, decl_id) =>
        {
            prefix_is_unresolved_param = true;
            LuaType::Unknown
        }
        Err(err) => return Err(err),
    };
    let index_member_expr = LuaIndexMemberExpr::IndexExpr(index_expr.clone());

    let reason = match infer_member_by_member_key(
        db,
        cache,
        &prefix_type,
        index_member_expr.clone(),
        &InferGuard::new(),
    ) {
        Ok(member_type) => {
            if pass_flow {
                return infer_member_type_pass_flow(
                    db,
                    cache,
                    index_expr,
                    // &prefix_type,
                    member_type,
                );
            }
            return Ok(member_type);
        }
        Err(InferFailReason::FieldNotFound) => InferFailReason::FieldNotFound,
        Err(err) => return Err(err),
    };

    match infer_member_by_operator(
        db,
        cache,
        &prefix_type,
        index_member_expr,
        &InferGuard::new(),
    ) {
        Ok(member_type) => {
            if pass_flow {
                return infer_member_type_pass_flow(
                    db,
                    cache,
                    index_expr,
                    // &prefix_type,
                    member_type,
                );
            }
            return Ok(member_type);
        }
        Err(InferFailReason::FieldNotFound) => {}
        Err(err) => return Err(err),
    }

    if pass_flow {
        match infer_member_type_fallback_pass_flow(
            db,
            cache,
            index_expr,
            prefix_is_unresolved_param,
        ) {
            Ok(member_type) => return Ok(member_type),
            Err(InferFailReason::FieldNotFound) | Err(InferFailReason::None) => {}
            Err(err) => return Err(err),
        }
    }

    if prefix_is_unresolved_param {
        return Ok(LuaType::Unknown);
    }

    Err(reason)
}

fn is_unresolved_param_decl(db: &DbIndex, decl_id: LuaDeclId) -> bool {
    db.get_decl_index()
        .get_decl(&decl_id)
        .is_some_and(|decl| decl.is_param())
}

fn infer_member_type_pass_flow(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: LuaIndexExpr,
    // prefix_type: &LuaType,
    member_type: LuaType,
) -> InferResult {
    let Some(var_ref_id) = get_index_expr_var_ref_id(db, cache, &index_expr) else {
        return Ok(member_type.clone());
    };

    cache
        .index_ref_origin_type_cache
        .insert(var_ref_id.clone(), CacheEntry::Cache(member_type.clone()));
    let result = infer_expr_narrow_type(db, cache, LuaExpr::IndexExpr(index_expr), var_ref_id);
    match &result {
        Err(InferFailReason::None) => Ok(member_type.clone()),
        _ => result,
    }
}

fn infer_member_type_fallback_pass_flow(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: LuaIndexExpr,
    unknown_truthy_as_any: bool,
) -> InferResult {
    let Some(var_ref_id) = get_index_expr_var_ref_id(db, cache, &index_expr) else {
        return Err(InferFailReason::FieldNotFound);
    };

    cache
        .index_ref_origin_type_cache
        .insert(var_ref_id.clone(), CacheEntry::Cache(LuaType::Nil));
    match infer_expr_narrow_type(db, cache, LuaExpr::IndexExpr(index_expr), var_ref_id) {
        Ok(member_type) if !member_type.is_nil() && !member_type.is_unknown() => Ok(member_type),
        Ok(member_type) if member_type.is_unknown() && unknown_truthy_as_any => Ok(LuaType::Any),
        Ok(_) | Err(InferFailReason::None) => Err(InferFailReason::FieldNotFound),
        Err(err) => Err(err),
    }
}

pub fn get_index_expr_var_ref_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: &LuaIndexExpr,
) -> Option<VarRefId> {
    let syntax_id = index_expr.get_syntax_id();
    if let Some(var_ref_id) = cache.expr_var_ref_id_cache.get(&syntax_id) {
        return Some(var_ref_id.clone());
    }

    let access_path = match index_expr.get_access_path() {
        Some(path) => ArcIntern::new(SmolStr::new(&path)),
        None => return None,
    };

    let mut prefix_expr = index_expr.get_prefix_expr()?;
    while let LuaExpr::IndexExpr(index_expr) = prefix_expr {
        prefix_expr = index_expr.get_prefix_expr()?;
    }

    if let LuaExpr::NameExpr(name_expr) = prefix_expr {
        let decl_or_member_id = match get_name_expr_var_ref_id(db, cache, &name_expr) {
            Some(VarRefId::SelfRef(decl_or_id)) => decl_or_id,
            Some(VarRefId::VarRef(decl_id)) => LuaDeclOrMemberId::Decl(decl_id),
            _ => return None,
        };

        let var_ref_id = VarRefId::IndexRef(decl_or_member_id, access_path);
        cache
            .expr_var_ref_id_cache
            .insert(syntax_id, var_ref_id.clone());
        return Some(var_ref_id);
    }

    None
}

pub fn infer_member_by_member_key(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    prefix_type: &LuaType,
    index_expr: LuaIndexMemberExpr,
    infer_guard: &InferGuardRef,
) -> InferResult {
    match &prefix_type {
        LuaType::Table => infer_plain_table_member(db, cache, index_expr),
        LuaType::Any => Ok(LuaType::Any),
        LuaType::Unknown => Err(InferFailReason::FieldNotFound),
        LuaType::Nil => Ok(LuaType::Never),
        LuaType::TableConst(id) => infer_table_member(db, cache, id.clone(), index_expr),
        LuaType::String
        | LuaType::Io
        | LuaType::StringConst(_)
        | LuaType::DocStringConst(_)
        | LuaType::Language(_) => {
            if db.get_emmyrc().gmod.enabled {
                if let Some(index_key) = index_expr.get_index_key() {
                    let is_numeric = match &index_key {
                        LuaIndexKey::Integer(_) | LuaIndexKey::Idx(_) => true,
                        LuaIndexKey::Expr(expr) => infer_expr(db, cache, expr.clone())
                            .map(|t| t.is_number())
                            .unwrap_or(false),
                        _ => false,
                    };
                    if is_numeric {
                        return Ok(LuaType::String);
                    }
                }
            }
            let decl_id = get_buildin_type_map_type_id(prefix_type).ok_or(InferFailReason::None)?;
            infer_custom_type_member(db, cache, decl_id, index_expr, infer_guard)
        }
        LuaType::Ref(decl_id) => {
            infer_custom_type_member(db, cache, decl_id.clone(), index_expr, infer_guard)
        }
        LuaType::Def(decl_id) => {
            infer_custom_type_member(db, cache, decl_id.clone(), index_expr, infer_guard)
        }
        // LuaType::Module(_) => todo!(),
        LuaType::Tuple(tuple_type) => infer_tuple_member(db, cache, tuple_type, index_expr),
        LuaType::Object(object_type) => infer_object_member(db, cache, object_type, index_expr),
        LuaType::Union(union_type) => {
            infer_union_member(db, cache, union_type, index_expr, infer_guard)
        }
        LuaType::MultiLineUnion(multi_union) => {
            let union_type = multi_union.to_union();
            if let LuaType::Union(union_type) = union_type {
                infer_union_member(db, cache, &union_type, index_expr, infer_guard)
            } else {
                Err(InferFailReason::FieldNotFound)
            }
        }
        LuaType::Intersection(intersection_type) => {
            infer_intersection_member(db, cache, intersection_type, index_expr, infer_guard)
        }
        LuaType::Generic(generic_type) => {
            infer_generic_member(db, cache, generic_type, index_expr, infer_guard)
        }
        LuaType::Global => infer_global_field_member(db, cache, index_expr),
        LuaType::Instance(inst) => infer_instance_member(db, cache, inst, index_expr, infer_guard),
        LuaType::Namespace(ns) => infer_namespace_member(db, cache, ns, index_expr),
        LuaType::Array(array_type) => infer_array_member(db, cache, array_type, index_expr),
        LuaType::TplRef(tpl) => infer_tpl_ref_member(db, cache, tpl, index_expr, infer_guard),
        LuaType::ModuleRef(file_id) => {
            let module_info = db.get_module_index().get_module(*file_id);
            if let Some(module_info) = module_info {
                if let Some(export_type) = &module_info.export_type {
                    if export_type.is_module_ref() {
                        return Err(InferFailReason::RecursiveInfer);
                    }

                    return infer_member_by_member_key(
                        db,
                        cache,
                        export_type,
                        index_expr,
                        infer_guard,
                    );
                } else {
                    return Err(InferFailReason::UnResolveModuleExport(*file_id));
                }
            }

            Err(InferFailReason::FieldNotFound)
        }
        LuaType::TableOf(inner) => {
            infer_member_by_member_key(db, cache, inner, index_expr, infer_guard)
        }
        _ => Err(InferFailReason::FieldNotFound),
    }
}

fn infer_table_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    inst: InFiled<TextRange>,
    index_expr: LuaIndexMemberExpr,
) -> InferResult {
    let owner = LuaMemberOwner::Element(inst.clone());
    let index_key = index_expr.get_index_key().ok_or(InferFailReason::None)?;
    let key = match LuaMemberKey::from_index_key_or_unknown(db, cache, &index_key) {
        Ok(key) => key,
        Err(err)
            if is_unknown_dynamic_key_without_table_data(db, &owner, &inst, &index_key, &err) =>
        {
            return Ok(nullable_any_type());
        }
        Err(err) => return Err(err),
    };
    if let Some(member_item) = db.get_member_index().get_member_item(&owner, &key) {
        return member_item.resolve_type_with_realm_at_offset(
            db,
            &cache.get_file_id(),
            index_expr.get_position(),
        );
    }

    match infer_owner_raw_member_type_with_realm(
        db,
        owner.clone(),
        &key,
        cache.get_file_id(),
        Some(index_expr.get_position()),
    ) {
        Ok(typ) => Ok(typ),
        Err(InferFailReason::FieldNotFound) => {
            if let Some(dynamic_field) = resolve_dynamic_field_member(
                db,
                cache,
                &LuaType::TableConst(inst.clone()),
                &key,
                Some(index_expr.get_position()),
            ) {
                return Ok(dynamic_field.typ);
            }
            if is_dynamic_expr_key_without_table_data(db, &owner, &inst, &key) {
                return Ok(nullable_any_type());
            }
            if is_table_const_from_doc_tag(db, &inst) {
                Ok(nullable_any_type())
            } else {
                Err(InferFailReason::FieldNotFound)
            }
        }
        Err(err) => Err(err),
    }
}

fn is_table_const_from_doc_tag(db: &DbIndex, inst: &InFiled<TextRange>) -> bool {
    let Some(root) = db.get_vfs().get_syntax_tree(&inst.file_id) else {
        return false;
    };

    is_doc_tag_table_const(&root.get_red_root(), inst.value)
}

fn nullable_any_type() -> LuaType {
    LuaType::Union(LuaUnionType::from_vec(vec![LuaType::Any, LuaType::Nil]).into())
}

fn is_unknown_dynamic_key_without_table_data(
    db: &DbIndex,
    owner: &LuaMemberOwner,
    inst: &InFiled<TextRange>,
    index_key: &LuaIndexKey,
    err: &InferFailReason,
) -> bool {
    matches!(index_key, LuaIndexKey::Expr(_))
        && table_const_has_no_specific_data(db, owner, inst)
        && matches!(
            err,
            InferFailReason::None
                | InferFailReason::UnResolveDeclType(_)
                | InferFailReason::UnResolveExpr(_)
        )
}

fn is_dynamic_expr_key_without_table_data(
    db: &DbIndex,
    owner: &LuaMemberOwner,
    inst: &InFiled<TextRange>,
    key: &LuaMemberKey,
) -> bool {
    matches!(key, LuaMemberKey::ExprType(_)) && table_const_has_no_specific_data(db, owner, inst)
}

fn table_const_has_no_specific_data(
    db: &DbIndex,
    owner: &LuaMemberOwner,
    inst: &InFiled<TextRange>,
) -> bool {
    db.get_member_index()
        .get_members(owner)
        .is_none_or(|members| members.is_empty())
        && db.get_metatable_index().get(inst).is_none()
}

fn infer_plain_table_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: LuaIndexMemberExpr,
) -> InferResult {
    if let Some(member_type) = infer_gmod_plain_table_dynamic_field(db, cache, &index_expr) {
        return Ok(member_type);
    }

    let nullable_any = nullable_any_type();

    let index_prefix_expr = match index_expr.clone() {
        LuaIndexMemberExpr::TableField(_) => return Ok(nullable_any),
        _ => index_expr.get_prefix_expr().ok_or(InferFailReason::None)?,
    };

    let Some(index_key) = index_expr.get_index_key() else {
        return Ok(nullable_any);
    };

    if let LuaIndexKey::Expr(expr) = index_key
        && check_iter_var_range(db, cache, &expr, index_prefix_expr).unwrap_or(false)
    {
        return Ok(LuaType::Any);
    }

    Ok(nullable_any)
}

fn infer_gmod_plain_table_dynamic_field(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: &LuaIndexMemberExpr,
) -> Option<LuaType> {
    if !db.get_emmyrc().gmod.enabled || !db.get_emmyrc().gmod.infer_dynamic_fields {
        return None;
    }

    if !is_gmod_gettable_alias_member_access(db, cache, index_expr) {
        return None;
    }

    let (class_name, _) = get_scripted_class_info_for_file(db, cache.get_file_id())?;

    let index_key = index_expr.get_index_key()?;
    let member_key = LuaMemberKey::from_index_key(db, cache, &index_key).ok()?;
    let class_type = LuaType::Ref(LuaTypeDeclId::global(&class_name));
    if let Ok(member_type) = infer_member_by_member_key(
        db,
        cache,
        &class_type,
        index_expr.clone(),
        &InferGuard::new(),
    ) {
        return Some(member_type);
    }

    resolve_dynamic_field_member(
        db,
        cache,
        &class_type,
        &member_key,
        Some(index_expr.get_position()),
    )
    .map(|resolution| resolution.typ)
}

fn is_gmod_gettable_alias_member_access(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: &LuaIndexMemberExpr,
) -> bool {
    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return false;
    };

    match prefix_expr {
        LuaExpr::NameExpr(name_expr) => local_name_initializer_expr(db, cache, &name_expr)
            .is_some_and(|initializer| is_self_gettable_call(db, cache, &initializer)),
        LuaExpr::CallExpr(call_expr) => is_self_gettable_call_expr(db, cache, &call_expr),
        _ => false,
    }
}

fn local_name_initializer_expr(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: &LuaNameExpr,
) -> Option<LuaExpr> {
    let Some(VarRefId::VarRef(decl_id)) = get_name_expr_var_ref_id(db, cache, name_expr) else {
        return None;
    };
    if decl_id.file_id != cache.get_file_id()
        || has_local_write_before_use(db, &cache.get_file_id(), &decl_id, name_expr.get_position())
    {
        return None;
    }
    db.get_decl_index().get_decl(&decl_id)?;
    let token = match name_expr.get_root().token_at_offset(decl_id.position) {
        rowan::TokenAtOffset::Single(token) => token,
        rowan::TokenAtOffset::Between(_, right) => right,
        rowan::TokenAtOffset::None => return None,
    };

    for ancestor in token.parent_ancestors() {
        let Some(local_stat) = LuaLocalStat::cast(ancestor) else {
            continue;
        };

        let name_index = local_stat
            .get_local_name_list()
            .position(|local_name| local_name.get_position() == decl_id.position)?;
        return local_stat.get_value_exprs().nth(name_index);
    }

    None
}

fn has_local_write_before_use(
    db: &DbIndex,
    file_id: &FileId,
    decl_id: &LuaDeclId,
    use_position: rowan::TextSize,
) -> bool {
    db.get_reference_index()
        .get_decl_references(file_id, decl_id)
        .is_some_and(|decl_ref| {
            decl_ref.cells.iter().any(|cell| {
                cell.is_write
                    && cell.range.start() > decl_id.position
                    && cell.range.start() < use_position
            })
        })
}

fn is_self_gettable_call(db: &DbIndex, cache: &mut LuaInferCache, expr: &LuaExpr) -> bool {
    let LuaExpr::CallExpr(call_expr) = expr else {
        return false;
    };

    is_self_gettable_call_expr(db, cache, call_expr)
}

fn is_self_gettable_call_expr(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> bool {
    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return false;
    };

    if call_expr.is_colon_call() {
        let LuaExpr::IndexExpr(index_expr) = &prefix_expr else {
            return false;
        };
        return is_entity_gettable_index_expr(db, cache, &prefix_expr)
            && index_expr
                .get_prefix_expr()
                .is_some_and(|self_expr| is_self_expr(&self_expr));
    }

    if !call_expr
        .get_args_list()
        .and_then(|args| args.get_args().next())
        .is_some_and(|arg| is_self_expr(&arg))
    {
        return false;
    }

    match prefix_expr {
        LuaExpr::IndexExpr(_) => is_entity_gettable_index_expr(db, cache, &prefix_expr),
        LuaExpr::NameExpr(name_expr) => local_name_initializer_expr(db, cache, &name_expr)
            .is_some_and(|initializer| is_entity_gettable_index_expr(db, cache, &initializer)),
        _ => false,
    }
}

fn is_entity_gettable_index_expr(db: &DbIndex, cache: &mut LuaInferCache, expr: &LuaExpr) -> bool {
    let LuaExpr::IndexExpr(index_expr) = expr else {
        return false;
    };

    if !matches!(
        index_expr.get_index_key(),
        Some(LuaIndexKey::Name(name)) if name.get_name_text() == "GetTable"
    ) {
        return false;
    }

    let Some(receiver_expr) = index_expr.get_prefix_expr() else {
        return false;
    };
    let Ok(receiver_type) = infer_expr(db, cache, receiver_expr) else {
        return false;
    };

    is_entity_or_derived_type(db, &receiver_type)
}

fn is_entity_or_derived_type(db: &DbIndex, typ: &LuaType) -> bool {
    let entity_id = LuaTypeDeclId::global("Entity");
    match typ {
        LuaType::Ref(type_id) | LuaType::Def(type_id) => {
            type_id == &entity_id || type_check::is_sub_type_of(db, type_id, &entity_id)
        }
        LuaType::Generic(generic) => {
            let type_id = generic.get_base_type_id_ref();
            type_id == &entity_id || type_check::is_sub_type_of(db, type_id, &entity_id)
        }
        LuaType::Instance(instance) => is_entity_or_derived_type(db, instance.get_base()),
        LuaType::TableOf(base) => is_entity_or_derived_type(db, base),
        LuaType::Union(union) => {
            let types = union.into_vec();
            let mut saw_non_nil = false;
            for typ in types.iter().filter(|typ| !matches!(typ, LuaType::Nil)) {
                saw_non_nil = true;
                if !is_entity_or_derived_type(db, typ) {
                    return false;
                }
            }
            saw_non_nil
        }
        _ => false,
    }
}

fn is_self_expr(expr: &LuaExpr) -> bool {
    matches!(expr, LuaExpr::NameExpr(name_expr) if name_expr.get_name_text().as_deref() == Some("self"))
}

fn infer_custom_type_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    prefix_type_id: LuaTypeDeclId,
    index_expr: LuaIndexMemberExpr,
    infer_guard: &InferGuardRef,
) -> InferResult {
    infer_guard.check(&prefix_type_id)?;
    let type_index = db.get_type_index();
    let type_decl = type_index
        .get_type_decl(&prefix_type_id)
        .ok_or(InferFailReason::None)?;
    if type_decl.is_alias() {
        if let Some(origin_type) = type_decl.get_alias_origin(db, None) {
            return infer_member_by_member_key(
                db,
                cache,
                &origin_type,
                index_expr.clone(),
                infer_guard,
            );
        } else {
            return Err(InferFailReason::FieldNotFound);
        }
    }
    if let LuaIndexMemberExpr::IndexExpr(index_expr) = &index_expr
        && enum_variable_is_param(db, cache, index_expr, &LuaType::Ref(prefix_type_id.clone()))
            .is_some()
    {
        return Err(InferFailReason::None);
    }

    let owner = LuaMemberOwner::Type(prefix_type_id.clone());
    let index_key = index_expr.get_index_key().ok_or(InferFailReason::None)?;
    let key = LuaMemberKey::from_index_key(db, cache, &index_key)?;
    let access_position = index_expr.get_position();

    if let Some(member_item) = db.get_member_index().get_member_item(&owner, &key) {
        return member_item.resolve_type_with_realm_at_offset(
            db,
            &cache.get_file_id(),
            access_position,
        );
    }
    let global_owner = LuaMemberOwner::GlobalPath(GlobalId::new(prefix_type_id.get_name()));
    if let Some(member_item) = db.get_member_index().get_member_item(&global_owner, &key) {
        let resolved = member_item.resolve_type_with_realm_at_offset(
            db,
            &cache.get_file_id(),
            access_position,
        );
        let decl_backed_type = resolve_decl_backed_global_path_member_type(
            db,
            member_item,
            &cache.get_file_id(),
            key.clone(),
            Some(access_position),
        );
        if let Some(module_decl_type) = decl_backed_type.clone()
            && resolved
                .as_ref()
                .is_ok_and(|resolved_type| resolved_type.is_table())
        {
            return Ok(module_decl_type);
        }
        if resolved.is_ok() {
            return resolved;
        }

        if let Some(module_decl_type) = decl_backed_type {
            return Ok(module_decl_type);
        }

        return resolved;
    }

    if let Some(dynamic_field) = resolve_dynamic_field_member(
        db,
        cache,
        &LuaType::Ref(prefix_type_id.clone()),
        &key,
        Some(index_expr.get_position()),
    ) {
        return Ok(dynamic_field.typ);
    }

    // 解决`key`为表达式的情况
    if let LuaIndexKey::Expr(expr) = index_key
        && let Some(keys) = get_expr_member_key(db, cache, &expr)
    {
        let mut result_types = Vec::new();
        for key in keys {
            // 解决 enum[enum] | class[class] 的情况
            if let Some(member_type) = get_expr_key_members(db, &key, &owner) {
                result_types.push(member_type);
                continue;
            }

            if let Some(member_item) = db.get_member_index().get_member_item(&owner, &key)
                && let Ok(member_type) = member_item.resolve_type_with_realm_at_offset(
                    db,
                    &cache.get_file_id(),
                    access_position,
                )
            {
                result_types.push(member_type);
            }
        }
        match &result_types[..] {
            [] => {}
            [first] => return Ok(first.clone()),
            _ => return Ok(LuaType::from_vec(result_types)),
        }
    }

    if type_decl.is_class()
        && let Some(super_types) = type_index.get_super_types(&prefix_type_id)
    {
        for super_type in super_types {
            let result =
                infer_member_by_member_key(db, cache, &super_type, index_expr.clone(), infer_guard);

            match result {
                Ok(member_type) => {
                    return Ok(member_type);
                }
                Err(InferFailReason::FieldNotFound) | Err(InferFailReason::None) => {}
                Err(err) => return Err(err),
            }
        }
    }

    Err(InferFailReason::FieldNotFound)
}

fn resolve_decl_backed_global_path_member_type(
    db: &DbIndex,
    member_item: &crate::db_index::LuaMemberIndexItem,
    caller_file_id: &crate::FileId,
    key: LuaMemberKey,
    caller_position: Option<TextSize>,
) -> Option<LuaType> {
    let visible_member_ids = match caller_position {
        Some(position) => {
            member_item.visible_member_ids_with_realm_at_offset(db, caller_file_id, position)
        }
        None => member_item.visible_member_ids_with_realm(db, caller_file_id),
    };
    let mut result = LuaType::Unknown;

    for member_id in visible_member_ids {
        let decl_id = crate::LuaDeclId::new(member_id.file_id, member_id.get_position());
        let decl = db.get_decl_index().get_decl(&decl_id)?;
        if !decl.is_module_scoped() || decl.get_name() != key.get_name()? {
            continue;
        }

        let decl_type = crate::semantic::infer::infer_name::infer_global_type(
            db,
            Some(member_id.file_id),
            Some(member_id.get_position()),
            decl.get_name(),
        )
        .or_else(|_| {
            db.get_type_index()
                .get_type_cache(&decl_id.into())
                .map(|cache| cache.as_type().clone())
                .ok_or(InferFailReason::None)
        })
        .unwrap_or(LuaType::Unknown);
        result = TypeOps::Union.apply(db, &result, &decl_type);
    }

    (!result.is_unknown()).then_some(result)
}

fn get_expr_key_members(
    db: &DbIndex,
    key: &LuaMemberKey,
    owner: &LuaMemberOwner,
) -> Option<LuaType> {
    let LuaMemberKey::ExprType(LuaType::Ref(index_id)) = key else {
        return None;
    };
    let index_type_decl = db.get_type_index().get_type_decl(index_id)?;
    let mut result = Vec::new();

    let origin_type = if index_type_decl.is_alias() {
        index_type_decl.get_alias_origin(db, None)?
    } else {
        LuaType::Ref(index_id.clone())
    };

    if let Some(member_keys) = get_all_member_key(db, &origin_type) {
        for key in member_keys {
            if let Some(member_item) = db.get_member_index().get_member_item(owner, &key)
                && let Ok(member_type) = member_item.resolve_type(db)
            {
                result.push(member_type);
            }
        }
    }

    match result.len() {
        0 => None,
        1 => Some(result[0].clone()),
        _ => Some(LuaType::from_vec(result)),
    }
}

fn get_all_member_key(db: &DbIndex, origin_type: &LuaType) -> Option<Vec<LuaMemberKey>> {
    let mut result = Vec::new();
    let mut stack = vec![origin_type.clone()]; // 堆栈用于迭代处理
    let mut visited = HashSet::new();

    while let Some(current_type) = stack.pop() {
        if visited.contains(&current_type) {
            continue;
        }
        visited.insert(current_type.clone());
        match current_type {
            LuaType::MultiLineUnion(types) => {
                for (typ, _) in types.get_unions() {
                    match typ {
                        LuaType::DocStringConst(s) | LuaType::StringConst(s) => {
                            result.push((*s).to_string().into());
                        }
                        LuaType::DocIntegerConst(i) | LuaType::IntegerConst(i) => {
                            result.push((*i).into());
                        }
                        LuaType::Ref(_) => {
                            stack.push(typ.clone()); // 将 Ref 类型推入堆栈进一步处理
                        }
                        _ => {}
                    }
                }
            }
            LuaType::Union(union_type) => {
                for typ in union_type.into_vec() {
                    if let LuaType::Ref(_) = typ {
                        stack.push(typ.clone()); // 推入堆栈
                    }
                }
            }
            LuaType::Ref(id) => {
                if let Some(type_decl) = db.get_type_index().get_type_decl(&id)
                    && type_decl.is_enum()
                {
                    let owner = LuaMemberOwner::Type(id.clone());
                    if let Some(members) = db.get_member_index().get_members(&owner) {
                        let is_enum_key = type_decl.is_enum_key();
                        for member in members {
                            if is_enum_key {
                                result.push(member.get_key().clone());
                            } else if let Some(typ) = db
                                .get_type_index()
                                .get_type_cache(&member.get_id().into())
                                .map(|it| it.as_type())
                            {
                                match typ {
                                    LuaType::DocStringConst(s) | LuaType::StringConst(s) => {
                                        result.push((*s).to_string().into());
                                    }
                                    LuaType::DocIntegerConst(i) | LuaType::IntegerConst(i) => {
                                        result.push((*i).into());
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Some(result)
}

fn infer_tuple_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    tuple_type: &LuaTupleType,
    index_expr: LuaIndexMemberExpr,
) -> InferResult {
    let index_key = index_expr.get_index_key().ok_or(InferFailReason::None)?;
    let key = LuaMemberKey::from_index_key(db, cache, &index_key)?;
    match &key {
        LuaMemberKey::Integer(i) => {
            let index = if *i > 0 { *i - 1 } else { 0 };
            return match tuple_type.get_type(index as usize) {
                Some(typ) => Ok(typ.clone()),
                None => Err(InferFailReason::FieldNotFound),
            };
        }
        LuaMemberKey::ExprType(expr_type) => match expr_type {
            LuaType::IntegerConst(i) => {
                let index = if *i > 0 { *i - 1 } else { 0 };
                return match tuple_type.get_type(index as usize) {
                    Some(typ) => Ok(typ.clone()),
                    None => Err(InferFailReason::FieldNotFound),
                };
            }
            LuaType::Integer | LuaType::Number => {
                let mut result = LuaType::Unknown;
                for typ in tuple_type.get_types() {
                    result = TypeOps::Union.apply(db, &result, typ);
                }

                let index_prefix_expr = match index_expr {
                    LuaIndexMemberExpr::TableField(_) => {
                        return Ok(result);
                    }
                    _ => index_expr.get_prefix_expr().ok_or(InferFailReason::None)?,
                };
                let maybe_iter_var = match &index_key {
                    LuaIndexKey::Expr(expr) => expr,
                    _ => return Ok(result),
                };
                if check_iter_var_range(db, cache, &maybe_iter_var, index_prefix_expr)
                    .unwrap_or(false)
                {
                    return Ok(result);
                }

                result = TypeOps::Union.apply(db, &result, &LuaType::Nil);
                return Ok(result);
            }
            _ => {}
        },
        _ => {}
    }

    Err(InferFailReason::FieldNotFound)
}

fn infer_object_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    object_type: &LuaObjectType,
    index_expr: LuaIndexMemberExpr,
) -> InferResult {
    let index_key = index_expr.get_index_key().ok_or(InferFailReason::None)?;
    let member_key = LuaMemberKey::from_index_key(db, cache, &index_key)?;
    if let Some(member_type) = object_type.get_field(&member_key) {
        return Ok(member_type.clone());
    }

    // todo
    let index_accesses = object_type.get_index_access();
    for (key, value) in index_accesses {
        let result = infer_index_metamethod(db, cache, &index_key, key, value);
        match result {
            Ok(typ) => {
                return Ok(typ);
            }
            Err(InferFailReason::FieldNotFound) => {}
            Err(err) => {
                return Err(err);
            }
        }
    }

    Err(InferFailReason::FieldNotFound)
}

fn infer_index_metamethod(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_key: &LuaIndexKey,
    key_type: &LuaType,
    value_type: &LuaType,
) -> InferResult {
    let access_key_type = index_key_access_type(db, cache, index_key)?;

    if check_type_compact(db, key_type, &access_key_type).is_ok() {
        return Ok(value_type.clone());
    }

    Err(InferFailReason::FieldNotFound)
}

fn index_key_access_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_key: &LuaIndexKey,
) -> InferResult {
    match index_key {
        LuaIndexKey::Name(name) => Ok(LuaType::StringConst(
            SmolStr::new(name.get_name_text()).into(),
        )),
        LuaIndexKey::String(s) => Ok(LuaType::StringConst(SmolStr::new(s.get_value()).into())),
        LuaIndexKey::Integer(i) => {
            if let NumberResult::Int(index_value) = i.get_number_value() {
                Ok(LuaType::IntegerConst(index_value))
            } else {
                Err(InferFailReason::FieldNotFound)
            }
        }
        LuaIndexKey::Idx(i) => Ok(LuaType::IntegerConst(*i as i64)),
        LuaIndexKey::Expr(expr) => infer_expr(db, cache, expr.clone()),
    }
}

fn infer_union_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    union_type: &LuaUnionType,
    index_expr: LuaIndexMemberExpr,
    infer_guard: &InferGuardRef,
) -> InferResult {
    let mut member_types = Vec::new();
    let mut meet_string = false;
    for sub_type in union_type.into_vec() {
        if sub_type.is_string() {
            if meet_string {
                continue;
            }
            meet_string = true;
        }
        let result = infer_member_by_member_key(
            db,
            cache,
            &sub_type,
            index_expr.clone(),
            &infer_guard.fork(),
        );
        if let Ok(typ) = result {
            if !typ.is_never() {
                member_types.push(typ);
            }
        } else {
            member_types.push(LuaType::Nil);
        }
    }

    if member_types.iter().all(|t| t.is_nil()) {
        return Err(InferFailReason::FieldNotFound);
    }

    Ok(LuaType::from_vec(member_types))
}

fn infer_intersection_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    intersection_type: &LuaIntersectionType,
    index_expr: LuaIndexMemberExpr,
    infer_guard: &InferGuardRef,
) -> InferResult {
    let mut result: Option<LuaType> = None;
    for member in intersection_type.get_types() {
        match infer_member_by_member_key(db, cache, member, index_expr.clone(), &infer_guard.fork())
        {
            Ok(ty) => {
                result = Some(match result {
                    Some(prev) => intersect_member_types(db, prev, ty),
                    None => ty,
                });

                if matches!(result, Some(LuaType::Never)) {
                    break;
                }
            }
            Err(InferFailReason::FieldNotFound) => continue,
            Err(reason) => return Err(reason),
        }
    }

    result.ok_or(InferFailReason::FieldNotFound)
}

fn infer_generic_members_from_super_generics(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    type_decl_id: &LuaTypeDeclId,
    substitutor: &TypeSubstitutor,
    index_expr: LuaIndexMemberExpr,
    infer_guard: &InferGuardRef,
) -> Option<LuaType> {
    let type_index = db.get_type_index();

    let type_decl = type_index.get_type_decl(type_decl_id)?;
    if !type_decl.is_class() {
        return None;
    };

    let type_decl_id = type_decl.get_id();
    if let Some(super_types) = type_index.get_super_types(&type_decl_id) {
        super_types.iter().find_map(|super_type| {
            let super_type = instantiate_type_generic(db, super_type, substitutor);
            infer_member_by_member_key(
                db,
                cache,
                &super_type,
                index_expr.clone(),
                &infer_guard.fork(),
            )
            .ok()
        })
    } else {
        None
    }
}

fn infer_generic_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    generic_type: &LuaGenericType,
    index_expr: LuaIndexMemberExpr,
    infer_guard: &InferGuardRef,
) -> InferResult {
    let base_type = generic_type.get_base_type();

    let generic_params = generic_type.get_params();
    let substitutor = TypeSubstitutor::from_type_array(generic_params.clone());

    if let LuaType::Ref(base_type_decl_id) = &base_type {
        let type_index = db.get_type_index();
        if let Some(type_decl) = type_index.get_type_decl(base_type_decl_id)
            && type_decl.is_alias()
            && let Some(origin_type) = type_decl.get_alias_origin(db, Some(&substitutor))
        {
            return infer_member_by_member_key(
                db,
                cache,
                &origin_type,
                index_expr,
                &infer_guard.fork(),
            );
        }

        let result = infer_generic_members_from_super_generics(
            db,
            cache,
            base_type_decl_id,
            &substitutor,
            index_expr.clone(),
            infer_guard,
        );
        if let Some(result) = result {
            return Ok(result);
        }
    }

    let member_type = infer_member_by_member_key(db, cache, &base_type, index_expr, infer_guard)?;

    Ok(instantiate_type_generic(db, &member_type, &substitutor))
}

fn infer_instance_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    inst: &LuaInstanceType,
    index_expr: LuaIndexMemberExpr,
    infer_guard: &InferGuardRef,
) -> InferResult {
    let range = inst.get_range();

    let origin_type = inst.get_base();
    let base_result =
        infer_member_by_member_key(db, cache, origin_type, index_expr.clone(), infer_guard);
    match base_result {
        Ok(typ) => match infer_table_member(db, cache, range.clone(), index_expr.clone()) {
            Ok(table_type) => {
                return Ok(match TypeOps::Intersect.apply(db, &typ, &table_type) {
                    LuaType::Never => typ,
                    intersected => intersected,
                });
            }
            Err(InferFailReason::FieldNotFound) => return Ok(typ),
            Err(err) => return Err(err),
        },
        Err(InferFailReason::FieldNotFound) => {}
        Err(err) => return Err(err),
    }

    infer_table_member(db, cache, range.clone(), index_expr.clone())
}

pub fn infer_member_by_operator(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    prefix_type: &LuaType,
    index_expr: LuaIndexMemberExpr,
    infer_guard: &InferGuardRef,
) -> InferResult {
    match &prefix_type {
        LuaType::Table => Ok(nullable_any_type()),
        LuaType::TableConst(in_filed) => {
            infer_member_by_index_table(db, cache, in_filed, index_expr)
        }
        LuaType::Ref(decl_id) => {
            infer_member_by_index_custom_type(db, cache, decl_id, index_expr, infer_guard)
        }
        LuaType::Def(decl_id) => {
            infer_member_by_index_custom_type(db, cache, decl_id, index_expr, infer_guard)
        }
        // LuaType::Module(arc) => todo!(),
        LuaType::Array(array_type) => {
            infer_member_by_index_array(db, cache, array_type.get_base(), index_expr)
        }
        LuaType::Object(object) => infer_member_by_index_object(db, cache, object, index_expr),
        LuaType::Union(union) => {
            infer_member_by_index_union(db, cache, union, index_expr, infer_guard)
        }
        LuaType::Intersection(intersection) => {
            infer_member_by_index_intersection(db, cache, intersection, index_expr, infer_guard)
        }
        LuaType::Generic(generic) => {
            infer_member_by_index_generic(db, cache, generic, index_expr, infer_guard)
        }
        LuaType::TableGeneric(table_generic) => {
            infer_member_by_index_table_generic(db, cache, table_generic, index_expr)
        }
        LuaType::Instance(inst) => {
            let base = inst.get_base();
            infer_member_by_operator(db, cache, base, index_expr, infer_guard)
        }
        LuaType::ModuleRef(file_id) => {
            let module_info = db.get_module_index().get_module(*file_id);
            if let Some(module_info) = module_info {
                if let Some(export_type) = &module_info.export_type {
                    return infer_member_by_operator(
                        db,
                        cache,
                        export_type,
                        index_expr,
                        infer_guard,
                    );
                } else {
                    return Err(InferFailReason::UnResolveModuleExport(*file_id));
                }
            }

            Err(InferFailReason::FieldNotFound)
        }
        _ => Err(InferFailReason::FieldNotFound),
    }
}

fn infer_member_by_index_table(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    table_range: &InFiled<TextRange>,
    index_expr: LuaIndexMemberExpr,
) -> InferResult {
    let metatable = db.get_metatable_index().get(table_range);
    match metatable {
        Some(metatable) => {
            let meta_owner = LuaOperatorOwner::Table(metatable.clone());
            let operator_ids = db
                .get_operator_index()
                .get_operators(&meta_owner, LuaOperatorMetaMethod::Index)
                .ok_or(InferFailReason::FieldNotFound)?;

            let index_key = index_expr.get_index_key().ok_or(InferFailReason::None)?;

            for operator_id in operator_ids {
                let operator = db
                    .get_operator_index()
                    .get_operator(operator_id)
                    .ok_or(InferFailReason::None)?;
                let operand = operator.get_operand(db);
                let return_type = operator.get_result(db)?;
                if let Ok(typ) =
                    infer_index_metamethod(db, cache, &index_key, &operand, &return_type)
                {
                    return Ok(typ);
                }
            }
        }
        None => {
            let index_key = index_expr.get_index_key().ok_or(InferFailReason::None)?;
            let key_type = index_key_access_type(db, cache, &index_key)?;
            let members = db
                .get_member_index()
                .get_members(&LuaMemberOwner::Element(table_range.clone()));
            if let Some(mut members) = members {
                members.sort_by(|a, b| a.get_key().cmp(b.get_key()));
                let mut result_type = LuaType::Unknown;
                let mut matched_inferred_index_key = false;
                for member in members {
                    if member_key_matches_type(db, &key_type, member.get_key()) {
                        matched_inferred_index_key |=
                            is_inferred_index_member_key(member.get_key());
                        let member_type = db
                            .get_type_index()
                            .get_type_cache(&member.get_id().into())
                            .map(|it| it.as_type())
                            .unwrap_or(&LuaType::Unknown);

                        result_type = TypeOps::Union.apply(db, &result_type, member_type);
                    }
                }

                if !result_type.is_unknown() {
                    if table_index_result_may_be_nil(db, &key_type, matched_inferred_index_key) {
                        result_type = TypeOps::Union.apply(db, &result_type, &LuaType::Nil);
                    }

                    return Ok(result_type);
                }
            }
        }
    }

    Err(InferFailReason::FieldNotFound)
}

fn table_index_result_may_be_nil(
    db: &DbIndex,
    key_type: &LuaType,
    matched_inferred_index_key: bool,
) -> bool {
    matches!(
        key_type,
        LuaType::String | LuaType::Number | LuaType::Integer
    ) || (db.get_emmyrc().strict.array_index
        && matched_inferred_index_key
        && is_numeric_access_key_type(key_type))
}

fn is_numeric_access_key_type(key_type: &LuaType) -> bool {
    matches!(
        key_type,
        LuaType::Integer | LuaType::Number | LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_)
    )
}

fn is_inferred_index_member_key(key: &LuaMemberKey) -> bool {
    matches!(key, LuaMemberKey::ExprType(_))
}

fn infer_member_by_index_custom_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    prefix_type_id: &LuaTypeDeclId,
    index_expr: LuaIndexMemberExpr,
    infer_guard: &InferGuardRef,
) -> InferResult {
    infer_guard.check(prefix_type_id)?;
    let type_index = db.get_type_index();
    let type_decl = type_index
        .get_type_decl(prefix_type_id)
        .ok_or(InferFailReason::None)?;
    if type_decl.is_alias() {
        if let Some(origin_type) = type_decl.get_alias_origin(db, None) {
            return infer_member_by_operator(db, cache, &origin_type, index_expr, infer_guard);
        }
        return Err(InferFailReason::None);
    }

    let index_key = index_expr.get_index_key().ok_or(InferFailReason::None)?;
    if let Some(index_operator_ids) = db
        .get_operator_index()
        .get_operators(&prefix_type_id.clone().into(), LuaOperatorMetaMethod::Index)
    {
        for operator_id in index_operator_ids {
            let operator = db
                .get_operator_index()
                .get_operator(operator_id)
                .ok_or(InferFailReason::None)?;
            let operand = operator.get_operand(db);
            let return_type = operator.get_result(db)?;
            let typ = infer_index_metamethod(db, cache, &index_key, &operand, &return_type);
            if let Ok(typ) = typ {
                return Ok(typ);
            }
        }
    }

    // find member by key in super
    if type_decl.is_class()
        && let Some(super_types) = type_index.get_super_types(prefix_type_id)
    {
        for super_type in super_types {
            let result =
                infer_member_by_operator(db, cache, &super_type, index_expr.clone(), infer_guard);
            match result {
                Ok(member_type) => {
                    return Ok(member_type);
                }
                Err(InferFailReason::FieldNotFound) => {}
                Err(err) => return Err(err),
            }
        }
    }

    Err(InferFailReason::FieldNotFound)
}

fn infer_member_by_index_array(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    base: &LuaType,
    index_expr: LuaIndexMemberExpr,
) -> InferResult {
    let member_key = index_expr.get_index_key().ok_or(InferFailReason::None)?;
    let expression_type = if db.get_emmyrc().strict.array_index {
        TypeOps::Union.apply(db, base, &LuaType::Nil)
    } else {
        base.clone()
    };
    if member_key.is_integer() {
        return Ok(expression_type);
    } else if member_key.is_expr() {
        let expr = member_key.get_expr().ok_or(InferFailReason::None)?;
        let expr_type = infer_expr(db, cache, expr.clone())?;
        if check_type_compact(db, &LuaType::Number, &expr_type).is_ok() {
            return Ok(expression_type);
        }
    }

    Err(InferFailReason::FieldNotFound)
}

fn infer_member_by_index_object(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    object: &LuaObjectType,
    index_expr: LuaIndexMemberExpr,
) -> InferResult {
    let member_key = index_expr.get_index_key().ok_or(InferFailReason::None)?;
    let access_member_type = object.get_index_access();
    if member_key.is_expr() {
        let expr = member_key.get_expr().ok_or(InferFailReason::None)?;
        let expr_type = infer_expr(db, cache, expr.clone())?;
        for (key, field) in access_member_type {
            if type_check::check_type_compact(db, key, &expr_type).is_ok() {
                return Ok(field.clone());
            }
        }
    }

    Err(InferFailReason::FieldNotFound)
}

fn infer_member_by_index_union(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    union: &LuaUnionType,
    index_expr: LuaIndexMemberExpr,
    infer_guard: &InferGuardRef,
) -> InferResult {
    let mut member_type = LuaType::Unknown;
    for member in union.into_vec() {
        let result =
            infer_member_by_operator(db, cache, &member, index_expr.clone(), &infer_guard.fork());
        match result {
            Ok(typ) => {
                member_type = TypeOps::Union.apply(db, &member_type, &typ);
            }
            Err(InferFailReason::FieldNotFound) => {}
            Err(err) => {
                return Err(err);
            }
        }
    }

    if member_type.is_unknown() {
        return Err(InferFailReason::FieldNotFound);
    }

    Ok(member_type)
}

fn infer_member_by_index_intersection(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    intersection: &LuaIntersectionType,
    index_expr: LuaIndexMemberExpr,
    infer_guard: &InferGuardRef,
) -> InferResult {
    let mut result: Option<LuaType> = None;
    for member in intersection.get_types() {
        match infer_member_by_operator(db, cache, member, index_expr.clone(), &infer_guard.fork()) {
            Ok(ty) => {
                result = Some(match result {
                    Some(prev) => intersect_member_types(db, prev, ty),
                    None => ty,
                });

                if matches!(result, Some(LuaType::Never)) {
                    break;
                }
            }
            Err(InferFailReason::FieldNotFound) => continue,
            Err(reason) => return Err(reason),
        }
    }

    result.ok_or(InferFailReason::FieldNotFound)
}

fn infer_member_by_index_generic(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    generic: &LuaGenericType,
    index_expr: LuaIndexMemberExpr,
    infer_guard: &InferGuardRef,
) -> InferResult {
    let base_type = generic.get_base_type();
    let type_decl_id = if let LuaType::Ref(id) = base_type {
        id
    } else {
        return Err(InferFailReason::None);
    };
    let generic_params = generic.get_params();
    let substitutor = TypeSubstitutor::from_type_array(generic_params.clone());
    let type_index = db.get_type_index();
    let type_decl = type_index
        .get_type_decl(&type_decl_id)
        .ok_or(InferFailReason::None)?;
    if type_decl.is_alias() {
        if let Some(origin_type) = type_decl.get_alias_origin(db, Some(&substitutor)) {
            return infer_member_by_operator(
                db,
                cache,
                &instantiate_type_generic(db, &origin_type, &substitutor),
                index_expr.clone(),
                &infer_guard.fork(),
            );
        }
        return Err(InferFailReason::None);
    }

    let member_key = index_expr.get_index_key().ok_or(InferFailReason::None)?;
    let operator_index = db.get_operator_index();
    if let Some(index_operator_ids) =
        operator_index.get_operators(&type_decl_id.clone().into(), LuaOperatorMetaMethod::Index)
    {
        for index_operator_id in index_operator_ids {
            let index_operator = operator_index
                .get_operator(index_operator_id)
                .ok_or(InferFailReason::None)?;
            let operand = index_operator.get_operand(db);
            let instianted_operand = instantiate_type_generic(db, &operand, &substitutor);
            let return_type =
                instantiate_type_generic(db, &index_operator.get_result(db)?, &substitutor);

            let result =
                infer_index_metamethod(db, cache, &member_key, &instianted_operand, &return_type);

            match result {
                Ok(member_type) => {
                    if !member_type.is_nil() {
                        return Ok(member_type);
                    }
                }
                Err(InferFailReason::FieldNotFound) => {}
                Err(err) => return Err(err),
            }
        }
    }

    // for supers
    if let Some(supers) = type_index.get_super_types(&type_decl_id) {
        for super_type in supers {
            let result = infer_member_by_operator(
                db,
                cache,
                &instantiate_type_generic(db, &super_type, &substitutor),
                index_expr.clone(),
                &infer_guard.fork(),
            );
            match result {
                Ok(member_type) => {
                    return Ok(member_type);
                }
                Err(InferFailReason::FieldNotFound) => {}
                Err(err) => return Err(err),
            }
        }
    }

    Err(InferFailReason::FieldNotFound)
}

fn infer_member_by_index_table_generic(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    table_params: &[LuaType],
    index_expr: LuaIndexMemberExpr,
) -> InferResult {
    if table_params.len() != 2 {
        return Err(InferFailReason::None);
    }

    let index_key = index_expr.get_index_key().ok_or(InferFailReason::None)?;
    let key_type = &table_params[0];
    let value_type = &table_params[1];
    infer_index_metamethod(db, cache, &index_key, key_type, value_type)
}

fn infer_global_field_member(
    db: &DbIndex,
    cache: &LuaInferCache,
    index_expr: LuaIndexMemberExpr,
) -> InferResult {
    let member_key = index_expr.get_index_key().ok_or(InferFailReason::None)?;
    let name = member_key
        .get_name()
        .ok_or(InferFailReason::None)?
        .get_name_text();
    infer_global_type(
        db,
        Some(cache.get_file_id()),
        Some(index_expr.get_position()),
        name,
    )
}

fn infer_namespace_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    ns: &str,
    index_expr: LuaIndexMemberExpr,
) -> InferResult {
    let index_key = index_expr.get_index_key().ok_or(InferFailReason::None)?;
    let member_key = LuaMemberKey::from_index_key(db, cache, &index_key)?;

    if let Some(member_item) = db
        .get_member_index()
        .get_member_item(&LuaMemberOwner::GlobalPath(GlobalId::new(ns)), &member_key)
    {
        let resolved = member_item.resolve_type_with_realm(db, &cache.get_file_id());
        if resolved.is_ok() {
            return resolved;
        }

        if let Some(module_decl_type) = resolve_decl_backed_global_path_member_type(
            db,
            member_item,
            &cache.get_file_id(),
            member_key.clone(),
            None,
        ) {
            return Ok(module_decl_type);
        }

        return resolved;
    }

    let member_key = match member_key {
        LuaMemberKey::Name(name) => name.to_string(),
        LuaMemberKey::Integer(i) => i.to_string(),
        _ => return Err(InferFailReason::None),
    };

    let namespace_or_type_id = format!("{}.{}", ns, member_key);
    let type_id = LuaTypeDeclId::global(&namespace_or_type_id);
    if db.get_type_index().get_type_decl(&type_id).is_some() {
        return Ok(LuaType::Def(type_id));
    }

    Ok(LuaType::Namespace(
        SmolStr::new(namespace_or_type_id).into(),
    ))
}

fn get_expr_member_key(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: &LuaExpr,
) -> Option<Vec<LuaMemberKey>> {
    let expr_type = infer_expr(db, cache, expr.clone()).ok()?;
    let mut keys: HashSet<LuaMemberKey> = HashSet::new();
    let mut stack = vec![expr_type.clone()];
    let mut visited = HashSet::new();

    while let Some(current_type) = stack.pop() {
        if !visited.insert(current_type.clone()) {
            continue;
        }
        match &current_type {
            LuaType::StringConst(name) | LuaType::DocStringConst(name) => {
                keys.insert(LuaMemberKey::Name((**name).clone()));
            }
            LuaType::IntegerConst(i) | LuaType::DocIntegerConst(i) => {
                keys.insert(LuaMemberKey::Integer(*i));
            }
            LuaType::Call(alias_call) => {
                if alias_call.get_call_kind() == LuaAliasCallKind::KeyOf {
                    let operands = alias_call.get_operands();
                    if operands.len() == 1 {
                        if let Some(members) = get_keyof_members(db, &operands[0]) {
                            keys.extend(members.into_iter().map(|member| member.key));
                        }
                    }
                }
            }
            LuaType::MultiLineUnion(multi_union) => {
                for (typ, _) in multi_union.get_unions() {
                    if !visited.contains(typ) {
                        stack.push(typ.clone());
                    }
                }
            }
            LuaType::Union(union_typ) => {
                for t in union_typ.into_vec() {
                    if !visited.contains(&t) {
                        stack.push(t.clone());
                    }
                }
            }
            LuaType::TableConst(_) | LuaType::Tuple(_) => {
                keys.insert(LuaMemberKey::ExprType(current_type.clone()));
            }
            LuaType::Ref(id) => {
                if let Some(type_decl) = db.get_type_index().get_type_decl(id) {
                    if type_decl.is_alias() {
                        if let Some(origin_type) = type_decl.get_alias_origin(db, None) {
                            if !visited.contains(&origin_type) {
                                stack.push(origin_type);
                            }
                            continue;
                        }
                    }
                    if type_decl.is_enum() || type_decl.is_alias() {
                        keys.insert(LuaMemberKey::ExprType(current_type.clone()));
                    }
                }
            }
            _ => {}
        }
    }

    // 转换为 Vec 并排序以确保顺序确定性
    let mut keys: Vec<_> = keys.into_iter().collect();
    keys.sort();
    Some(keys)
}

fn infer_tpl_ref_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    generic: &GenericTpl,
    index_expr: LuaIndexMemberExpr,
    infer_guard: &InferGuardRef,
) -> InferResult {
    let extend_type = get_tpl_ref_extend_type(
        db,
        cache,
        &LuaType::TplRef(generic.clone().into()),
        index_expr
            .get_index_expr()
            .ok_or(InferFailReason::None)?
            .get_prefix_expr()
            .ok_or(InferFailReason::None)?,
        0,
    )
    .ok_or(InferFailReason::None)?;
    infer_member_by_member_key(db, cache, &extend_type, index_expr.clone(), infer_guard)
}
