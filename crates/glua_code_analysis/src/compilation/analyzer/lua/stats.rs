use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAstNode, LuaExpr, LuaFuncStat, LuaIndexExpr, LuaIndexKey,
    LuaLiteralToken, LuaLocalFuncStat, LuaLocalStat, LuaNameExpr, LuaTableExpr, LuaTableField,
    LuaVarExpr, NumberResult, PathTrait, UnaryOperator,
};

use crate::{
    InFiled, InferFailReason, LuaArrayType, LuaMemberKey, LuaSemanticDeclId, LuaTypeCache,
    LuaTypeOwner, TypeOps,
    compilation::analyzer::{
        common::{add_member, bind_type},
        unresolve::{UnResolveDecl, UnResolveMember},
    },
    db_index::{LuaDeclId, LuaMember, LuaMemberFeature, LuaMemberId, LuaMemberOwner, LuaType},
};

use super::LuaAnalyzer;

pub fn analyze_local_stat(analyzer: &mut LuaAnalyzer, local_stat: LuaLocalStat) -> Option<()> {
    let name_list: Vec<_> = local_stat.get_local_name_list().collect();
    let expr_list: Vec<_> = local_stat.get_value_exprs().collect();
    let name_count = name_list.len();
    let expr_count = expr_list.len();
    if expr_count == 0 {
        for local_name in name_list {
            let position = local_name.get_position();
            let decl_id = LuaDeclId::new(analyzer.file_id, position);
            // 标记了延迟定义属性, 此时将跳过绑定类型, 等待第一次赋值时再绑定类型
            if has_delayed_definition_attribute(analyzer, decl_id) {
                return Some(());
            }
            // Skip Nil binding for mutable locals (those with subsequent write-assignments).
            // This prevents false "cannot assign X to never" diagnostics when a local is used
            // as an upvalue inside a closure and assigned before the closure is first called.
            if is_local_mutable(analyzer, decl_id) {
                continue;
            }
            analyzer
                .db
                .get_type_index_mut()
                .bind_type(decl_id.into(), LuaTypeCache::InferType(LuaType::Nil));
        }

        return Some(());
    }

    for i in 0..name_count {
        let name = name_list.get(i)?;
        let position = name.get_position();
        let expr = if let Some(expr) = expr_list.get(i) {
            expr.clone()
        } else {
            break;
        };

        match analyzer.infer_expr(&expr) {
            Ok(mut expr_type) => {
                if let LuaType::Variadic(multi) = expr_type {
                    expr_type = multi.get_type(0)?.clone();
                }
                let decl_id = LuaDeclId::new(analyzer.file_id, position);
                // 当`call`参数包含表时, 表可能未被分析, 需要延迟
                if let LuaType::Instance(instance) = &expr_type
                    && instance.get_base().is_unknown()
                    && call_expr_has_effect_table_arg(&expr).is_some()
                {
                    let unresolve = UnResolveDecl {
                        file_id: analyzer.file_id,
                        decl_id,
                        expr: expr.clone(),
                        ret_idx: 0,
                    };
                    analyzer.context.add_unresolve(
                        unresolve.into(),
                        InferFailReason::UnResolveExpr(InFiled::new(
                            analyzer.file_id,
                            expr.clone(),
                        )),
                    );
                    continue;
                }

                bind_type(
                    analyzer.db,
                    decl_id.into(),
                    LuaTypeCache::InferType(expr_type),
                );
            }
            Err(InferFailReason::None) => {
                let decl_id = LuaDeclId::new(analyzer.file_id, position);
                analyzer
                    .db
                    .get_type_index_mut()
                    .bind_type(decl_id.into(), LuaTypeCache::InferType(LuaType::Nil));
            }
            Err(reason) => {
                let decl_id = LuaDeclId::new(analyzer.file_id, position);
                let unresolve = UnResolveDecl {
                    file_id: analyzer.file_id,
                    decl_id,
                    expr: expr.clone(),
                    ret_idx: 0,
                };

                analyzer.context.add_unresolve(unresolve.into(), reason);
            }
        }
    }

    // The complexity brought by multiple return values is too high
    if name_count > expr_count {
        let last_expr = expr_list.last();
        if let Some(last_expr) = last_expr {
            match analyzer.infer_expr(last_expr) {
                Ok(last_expr_type) => {
                    if let LuaType::Variadic(variadic) = last_expr_type {
                        for i in expr_count..name_count {
                            let name = name_list.get(i)?;
                            let position = name.get_position();
                            let decl_id = LuaDeclId::new(analyzer.file_id, position);
                            let ret_type = variadic.get_type(i - expr_count + 1);
                            if let Some(ret_type) = ret_type {
                                bind_type(
                                    analyzer.db,
                                    decl_id.into(),
                                    LuaTypeCache::InferType(ret_type.clone()),
                                );
                            } else {
                                analyzer.db.get_type_index_mut().bind_type(
                                    decl_id.into(),
                                    LuaTypeCache::InferType(LuaType::Unknown),
                                );
                            }
                        }
                        return Some(());
                    }
                }
                Err(reason) => {
                    for i in expr_count..name_count {
                        let name = name_list.get(i)?;
                        let position = name.get_position();
                        let decl_id = LuaDeclId::new(analyzer.file_id, position);
                        let unresolve = UnResolveDecl {
                            file_id: analyzer.file_id,
                            decl_id,
                            expr: last_expr.clone(),
                            ret_idx: i - expr_count + 1,
                        };

                        analyzer
                            .context
                            .add_unresolve(unresolve.into(), reason.clone());
                    }
                }
            }
        } else {
            for i in expr_count..name_count {
                let name = name_list.get(i)?;
                let position = name.get_position();
                let decl_id = LuaDeclId::new(analyzer.file_id, position);
                analyzer
                    .db
                    .get_type_index_mut()
                    .bind_type(decl_id.into(), LuaTypeCache::InferType(LuaType::Nil));
            }
        }
    }

    Some(())
}

fn call_expr_has_effect_table_arg(expr: &LuaExpr) -> Option<()> {
    if let LuaExpr::CallExpr(call_expr) = expr {
        let args_list = call_expr.get_args_list()?;
        for arg in args_list.get_args() {
            if let LuaExpr::TableExpr(table_expr) = arg
                && !table_expr.is_empty()
            {
                return Some(());
            }
        }
    }
    None
}

fn get_var_owner(analyzer: &mut LuaAnalyzer, var: LuaVarExpr) -> LuaTypeOwner {
    let file_id = analyzer.file_id;
    match var {
        LuaVarExpr::NameExpr(var_name) => {
            let position = var_name.get_position();
            let decl_id = LuaDeclId::new(file_id, position);
            LuaTypeOwner::Decl(decl_id)
        }
        LuaVarExpr::IndexExpr(index_expr) => {
            let maybe_decl_id = LuaDeclId::new(file_id, index_expr.get_position());
            if analyzer
                .db
                .get_decl_index()
                .get_decl(&maybe_decl_id)
                .is_some()
            {
                return LuaTypeOwner::Decl(maybe_decl_id);
            }

            let member_id = LuaMemberId::new(index_expr.get_syntax_id(), file_id);
            LuaTypeOwner::Member(member_id)
        }
    }
}

fn set_index_expr_owner(analyzer: &mut LuaAnalyzer, var_expr: LuaVarExpr) -> Option<()> {
    let file_id = analyzer.file_id;
    let index_expr = LuaIndexExpr::cast(var_expr.syntax().clone())?;
    let prefix_expr = index_expr.get_prefix_expr()?;

    match analyzer.infer_expr(&prefix_expr.clone()) {
        Ok(prefix_type) => {
            index_expr.get_index_key()?;
            let member_id = LuaMemberId::new(index_expr.get_syntax_id(), file_id);
            let (member_owner, set_owner_only) = resolve_index_expr_member_owner(&prefix_type)?;
            if set_owner_only {
                analyzer.db.get_member_index_mut().set_member_owner(
                    member_owner,
                    member_id.file_id,
                    member_id,
                );
                return Some(());
            }

            add_member(analyzer.db, member_owner, member_id);
        }
        Err(InferFailReason::None) => {}
        Err(reason) => {
            // record unresolve
            let unresolve_member = UnResolveMember {
                file_id: analyzer.file_id,
                member_id: LuaMemberId::new(var_expr.get_syntax_id(), file_id),
                expr: None,
                prefix: Some(prefix_expr),
                ret_idx: 0,
            };
            analyzer
                .context
                .add_unresolve(unresolve_member.into(), reason);
        }
    }

    Some(())
}

// assign stat is toooooooooo complex
pub fn analyze_assign_stat(analyzer: &mut LuaAnalyzer, assign_stat: LuaAssignStat) -> Option<()> {
    let (var_list, expr_list) = assign_stat.get_var_and_expr_list();
    let expr_count = expr_list.len();
    let var_count = var_list.len();

    for i in 0..expr_count {
        let var = var_list.get(i)?;
        let expr = expr_list.get(i);
        if expr.is_none() {
            break;
        }
        let expr = expr?;

        let type_owner = get_var_owner(analyzer, var.clone());
        set_index_expr_owner(analyzer, var.clone());

        if special_assign_pattern(analyzer, type_owner.clone(), var.clone(), expr.clone()).is_some()
        {
            continue;
        }

        let expr_type = match analyzer.infer_expr(expr) {
            Ok(expr_type) => match expr_type {
                LuaType::Variadic(multi) => multi.get_type(0)?.clone(),
                _ => expr_type,
            },
            Err(InferFailReason::None) => LuaType::Unknown,
            Err(reason) => {
                match type_owner {
                    LuaTypeOwner::Decl(decl_id) => {
                        let unresolve_decl = UnResolveDecl {
                            file_id: analyzer.file_id,
                            decl_id,
                            expr: expr.clone(),
                            ret_idx: 0,
                        };

                        analyzer
                            .context
                            .add_unresolve(unresolve_decl.into(), reason);
                    }
                    LuaTypeOwner::Member(member_id) => {
                        let unresolve_member = UnResolveMember {
                            file_id: analyzer.file_id,
                            member_id,
                            expr: Some(expr.clone()),
                            prefix: None,
                            ret_idx: 0,
                        };
                        analyzer
                            .context
                            .add_unresolve(unresolve_member.into(), reason);
                    }
                    _ => {}
                }
                continue;
            }
        };

        // 如果具有延迟定义属性, 则先绑定最初的定义
        if let LuaVarExpr::NameExpr(name_expr) = var {
            if let Some(decl_id) = get_delayed_definition_decl_id(analyzer, name_expr) {
                bind_type(
                    analyzer.db,
                    decl_id.into(),
                    LuaTypeCache::InferType(expr_type.clone()),
                );
            }
        }

        widen_existing_member_collection_type(analyzer, &var, &expr_type);
        assign_merge_type_owner_and_expr_type(analyzer, type_owner, &expr_type, 0);
    }

    // The complexity brought by multiple return values is too high
    if var_count > expr_count
        && let Some(last_expr) = expr_list.last()
    {
        match analyzer.infer_expr(last_expr) {
            Ok(last_expr_type) => {
                if last_expr_type.is_multi_return() {
                    for i in expr_count..var_count {
                        let var = var_list.get(i)?;
                        let type_owner = get_var_owner(analyzer, var.clone());
                        set_index_expr_owner(analyzer, var.clone());
                        assign_merge_type_owner_and_expr_type(
                            analyzer,
                            type_owner,
                            &last_expr_type,
                            i - expr_count + 1,
                        );
                    }
                }
            }
            Err(_) => {
                for i in expr_count..var_count {
                    let var = var_list.get(i)?;
                    let type_owner = get_var_owner(analyzer, var.clone());
                    set_index_expr_owner(analyzer, var.clone());
                    merge_type_owner_and_unresolve_expr(
                        analyzer,
                        type_owner,
                        last_expr.clone(),
                        i - expr_count + 1,
                    );
                }
            }
        }
    }

    // Expressions like a, b are not valid

    Some(())
}

fn assign_merge_type_owner_and_expr_type(
    analyzer: &mut LuaAnalyzer,
    type_owner: LuaTypeOwner,
    expr_type: &LuaType,
    idx: usize,
) -> Option<()> {
    let mut expr_type = expr_type.clone();
    if let LuaType::Variadic(multi) = expr_type {
        expr_type = multi.get_type(idx).unwrap_or(&LuaType::Nil).clone();
    }

    if let Some(widened_type) =
        get_widened_member_assignment_collection_type(analyzer, &type_owner, &expr_type)
    {
        expr_type = widened_type;
    }

    if let Some(widened_type) =
        get_widened_member_assignment_type(analyzer, &type_owner, &expr_type)
    {
        expr_type = widened_type;
    }

    bind_type(
        analyzer.db,
        type_owner.clone(),
        LuaTypeCache::InferType(expr_type),
    );

    if let LuaTypeOwner::Member(member_id) = type_owner
        && is_assignment_file_define_member(analyzer.db, member_id)
    {
        analyzer
            .db
            .get_member_index_mut()
            .retain_only_member_for_owner_key(member_id);
    }

    Some(())
}

fn get_widened_member_assignment_collection_type(
    analyzer: &mut LuaAnalyzer,
    type_owner: &LuaTypeOwner,
    incoming_type: &LuaType,
) -> Option<LuaType> {
    let LuaTypeOwner::Member(member_id) = type_owner else {
        return None;
    };
    let incoming_array = normalize_infer_collection_type(analyzer.db, incoming_type)?;
    let member_index = analyzer.db.get_member_index();
    let owner = member_index.get_member_owner(member_id)?.clone();
    let key = member_index.get_member(member_id)?.get_key().clone();
    let related_members = member_index.get_members_for_owner_key(&owner, &key);
    let mut widened_base = incoming_array.get_base().clone();
    let mut saw_related_collection = false;

    for related_member in related_members {
        let related_member_id = related_member.get_id();
        if related_member_id == *member_id {
            continue;
        }

        let Some(existing_cache) = analyzer
            .db
            .get_type_index()
            .get_type_cache(&related_member_id.into())
            .cloned()
        else {
            continue;
        };
        if !existing_cache.is_infer() {
            continue;
        }

        let Some(existing_array) =
            normalize_infer_collection_type(analyzer.db, existing_cache.as_type())
        else {
            continue;
        };
        saw_related_collection = true;
        widened_base = TypeOps::Union.apply(analyzer.db, existing_array.get_base(), &widened_base);
    }

    if !saw_related_collection {
        return None;
    }

    Some(LuaType::Array(
        LuaArrayType::from_base_type(widened_base).into(),
    ))
}

fn get_widened_member_assignment_type(
    analyzer: &mut LuaAnalyzer,
    type_owner: &LuaTypeOwner,
    incoming_type: &LuaType,
) -> Option<LuaType> {
    let LuaTypeOwner::Member(member_id) = type_owner else {
        return None;
    };
    if !is_assignment_file_define_member(analyzer.db, *member_id) {
        return None;
    }

    let member_index = analyzer.db.get_member_index();
    let owner = member_index.get_member_owner(member_id)?.clone();
    let key = member_index.get_member(member_id)?.get_key().clone();
    let related_members = member_index.get_members_for_owner_key(&owner, &key);
    if related_members.len() < 2 {
        return None;
    }

    if let Some(class_type) = prefer_class_assignment_type(incoming_type) {
        let mut saw_previous_assignment = false;
        let mut class_bootstrap_compatible = true;

        for related_member in &related_members {
            let related_member_id = related_member.get_id();
            if related_member_id == *member_id {
                continue;
            }
            saw_previous_assignment = true;

            if !is_assignment_file_define_member(analyzer.db, related_member_id) {
                class_bootstrap_compatible = false;
                break;
            }

            let Some(existing_cache) = analyzer
                .db
                .get_type_index()
                .get_type_cache(&related_member_id.into())
                .cloned()
            else {
                continue;
            };

            if existing_cache.is_doc() {
                class_bootstrap_compatible = false;
                break;
            }

            if !is_class_bootstrap_compatible_type(existing_cache.as_type(), &class_type) {
                class_bootstrap_compatible = false;
                break;
            }
        }

        if saw_previous_assignment && class_bootstrap_compatible {
            return Some(class_type);
        }
    }

    let mut doc_type: Option<LuaType> = None;
    let mut widened_type = crate::widen_literal_type_for_assignment(incoming_type);
    let mut saw_previous_assignment = false;

    for related_member in related_members {
        let related_member_id = related_member.get_id();
        if related_member_id == *member_id {
            continue;
        }
        saw_previous_assignment = true;

        if !is_assignment_file_define_member(analyzer.db, related_member_id) {
            return None;
        }

        let Some(existing_cache) = analyzer
            .db
            .get_type_index()
            .get_type_cache(&related_member_id.into())
            .cloned()
        else {
            continue;
        };

        if existing_cache.is_doc() {
            let existing_type = existing_cache.as_type().clone();
            doc_type = Some(match doc_type {
                Some(current) => TypeOps::Union.apply(analyzer.db, &current, &existing_type),
                None => existing_type,
            });
            continue;
        }

        let existing_type = crate::widen_literal_type_for_assignment(existing_cache.as_type());
        widened_type = TypeOps::Union.apply(analyzer.db, &widened_type, &existing_type);
    }

    if !saw_previous_assignment {
        return None;
    }

    Some(doc_type.unwrap_or(widened_type))
}

fn prefer_class_assignment_type(typ: &LuaType) -> Option<LuaType> {
    match typ {
        LuaType::Def(def_id) => Some(LuaType::Def(def_id.clone())),
        LuaType::Ref(ref_id) => Some(LuaType::Ref(ref_id.clone())),
        LuaType::Instance(instance) => prefer_class_assignment_type(instance.get_base()),
        LuaType::TypeGuard(inner) => prefer_class_assignment_type(inner),
        LuaType::Union(union) => prefer_class_assignment_type_from_iter(union.into_vec().iter()),
        LuaType::Intersection(intersection) => {
            prefer_class_assignment_type_from_iter(intersection.get_types().iter())
        }
        LuaType::MultiLineUnion(union) => {
            prefer_class_assignment_type_from_iter(union.get_unions().iter().map(|(typ, _)| typ))
        }
        _ => None,
    }
}

fn prefer_class_assignment_type_from_iter<'a>(
    types: impl Iterator<Item = &'a LuaType>,
) -> Option<LuaType> {
    for typ in types {
        if let Some(class_type) = prefer_class_assignment_type(typ) {
            return Some(class_type);
        }
    }

    None
}

fn is_class_bootstrap_compatible_type(typ: &LuaType, class_type: &LuaType) -> bool {
    if is_same_class_type(typ, class_type) {
        return true;
    }

    match typ {
        LuaType::TypeGuard(inner) => is_class_bootstrap_compatible_type(inner, class_type),
        LuaType::Instance(instance) => {
            is_class_bootstrap_compatible_type(instance.get_base(), class_type)
                || is_table_bootstrap_type(typ)
        }
        LuaType::Union(union) => union
            .into_vec()
            .iter()
            .all(|sub_type| is_class_bootstrap_compatible_type(sub_type, class_type)),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .all(|sub_type| is_class_bootstrap_compatible_type(sub_type, class_type)),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .all(|(sub_type, _)| is_class_bootstrap_compatible_type(sub_type, class_type)),
        _ => is_table_bootstrap_type(typ),
    }
}

fn is_same_class_type(left: &LuaType, right: &LuaType) -> bool {
    match (
        class_decl_id_from_type(left),
        class_decl_id_from_type(right),
    ) {
        (Some(left_id), Some(right_id)) => left_id == right_id,
        _ => false,
    }
}

fn class_decl_id_from_type(typ: &LuaType) -> Option<crate::LuaTypeDeclId> {
    match typ {
        LuaType::Def(def_id) | LuaType::Ref(def_id) => Some(def_id.clone()),
        LuaType::Instance(instance) => class_decl_id_from_type(instance.get_base()),
        LuaType::TypeGuard(inner) => class_decl_id_from_type(inner),
        _ => None,
    }
}

fn is_table_bootstrap_type(typ: &LuaType) -> bool {
    typ.is_table() || matches!(typ, LuaType::Unknown | LuaType::Nil | LuaType::Never)
}

fn widen_existing_member_collection_type(
    analyzer: &mut LuaAnalyzer,
    var: &LuaVarExpr,
    value_type: &LuaType,
) -> Option<()> {
    let LuaVarExpr::IndexExpr(index_expr) = var else {
        return Some(());
    };

    let is_collection_append = is_collection_append_write(index_expr).unwrap_or(false);
    let incoming_is_collection = normalize_infer_collection_type(analyzer.db, value_type).is_some();

    if !incoming_is_collection && !is_collection_append {
        return Some(());
    }

    if let Some(member_ids) = find_related_member_ids(analyzer, index_expr.clone()) {
        widen_member_collections_with_collection_type(analyzer, &member_ids, value_type);
    }

    if is_collection_append
        && let Some(prefix_expr) = index_expr.get_prefix_expr()
        && let Some(prefix_index_expr) = LuaIndexExpr::cast(prefix_expr.syntax().clone())
        && let Some(member_ids) = find_related_member_ids(analyzer, prefix_index_expr)
    {
        widen_member_collections_with_element_type(analyzer, &member_ids, value_type);
    }

    Some(())
}

fn find_related_member_ids(
    analyzer: &mut LuaAnalyzer,
    index_expr: LuaIndexExpr,
) -> Option<Vec<LuaMemberId>> {
    let prefix_expr = index_expr.get_prefix_expr()?;
    let prefix_type = analyzer.infer_expr(&prefix_expr).ok()?;
    let owner = get_member_owner_for_prefix_type(prefix_type)?;
    let index_key = index_expr.get_index_key()?;
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let member_key = LuaMemberKey::from_index_key(analyzer.db, cache, &index_key).ok()?;
    let members = analyzer
        .db
        .get_member_index()
        .get_members_for_owner_key(&owner, &member_key);

    if members.is_empty() {
        return None;
    }

    Some(members.into_iter().map(|member| member.get_id()).collect())
}

fn get_member_owner_for_prefix_type(prefix_type: LuaType) -> Option<LuaMemberOwner> {
    resolve_index_expr_member_owner(&prefix_type).map(|(owner, _)| owner)
}

fn resolve_index_expr_member_owner(prefix_type: &LuaType) -> Option<(LuaMemberOwner, bool)> {
    match prefix_type {
        LuaType::TableConst(in_file_range) => {
            Some((LuaMemberOwner::Element(in_file_range.clone()), false))
        }
        LuaType::Def(def_id) => Some((LuaMemberOwner::Type(def_id.clone()), false)),
        LuaType::Ref(ref_id) => Some((LuaMemberOwner::Type(ref_id.clone()), true)),
        LuaType::Instance(instance) => {
            Some((LuaMemberOwner::Element(instance.get_range().clone()), false))
        }
        LuaType::TypeGuard(inner) => resolve_index_expr_member_owner(inner),
        LuaType::Union(union) => pick_preferred_index_expr_member_owner(union.into_vec().iter()),
        LuaType::Intersection(intersection) => {
            pick_preferred_index_expr_member_owner(intersection.get_types().iter())
        }
        LuaType::MultiLineUnion(union) => {
            pick_preferred_index_expr_member_owner(union.get_unions().iter().map(|(typ, _)| typ))
        }
        _ => None,
    }
}

fn pick_preferred_index_expr_member_owner<'a>(
    types: impl Iterator<Item = &'a LuaType>,
) -> Option<(LuaMemberOwner, bool)> {
    let mut fallback_owner = None;
    for typ in types {
        let Some(owner_info) = resolve_index_expr_member_owner(typ) else {
            continue;
        };

        if matches!(&owner_info.0, LuaMemberOwner::Type(_)) && !owner_info.1 {
            return Some(owner_info);
        }

        if fallback_owner.is_none() {
            fallback_owner = Some(owner_info);
        }
    }

    fallback_owner
}

fn is_collection_append_write(index_expr: &LuaIndexExpr) -> Option<bool> {
    let prefix_expr = index_expr.get_prefix_expr()?;
    let LuaIndexKey::Expr(index_key_expr) = index_expr.get_index_key()? else {
        return Some(false);
    };
    let LuaExpr::BinaryExpr(binary_expr) = index_key_expr else {
        return Some(false);
    };
    if binary_expr.get_op_token()?.get_op() != BinaryOperator::OpAdd {
        return Some(false);
    }

    let (left, right) = binary_expr.get_exprs()?;
    if !is_literal_integer_one(&right) {
        return Some(false);
    }

    let LuaExpr::UnaryExpr(unary_expr) = left else {
        return Some(false);
    };
    if unary_expr.get_op_token()?.get_op() != UnaryOperator::OpLen {
        return Some(false);
    }

    let len_expr = unary_expr.get_expr()?;
    Some(expr_access_path(&prefix_expr) == expr_access_path(&len_expr))
}

fn expr_access_path(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::NameExpr(name_expr) => name_expr.get_access_path(),
        LuaExpr::IndexExpr(index_expr) => index_expr.get_access_path(),
        _ => None,
    }
}

fn is_literal_integer_one(expr: &LuaExpr) -> bool {
    let LuaExpr::LiteralExpr(literal_expr) = expr else {
        return false;
    };

    matches!(
        literal_expr.get_literal(),
        Some(LuaLiteralToken::Number(number))
            if matches!(number.get_number_value(), NumberResult::Int(1))
    )
}

fn widen_member_collections_with_collection_type(
    analyzer: &mut LuaAnalyzer,
    member_ids: &[LuaMemberId],
    incoming_type: &LuaType,
) -> Option<()> {
    let incoming_array = normalize_infer_collection_type(analyzer.db, incoming_type)?;

    for member_id in member_ids {
        let existing_cache = analyzer
            .db
            .get_type_index()
            .get_type_cache(&(*member_id).into())
            .cloned()?;
        if !existing_cache.is_infer() {
            continue;
        }

        let Some(existing_array) =
            normalize_infer_collection_type(analyzer.db, existing_cache.as_type())
        else {
            continue;
        };

        let widened_base = TypeOps::Union.apply(
            analyzer.db,
            existing_array.get_base(),
            incoming_array.get_base(),
        );
        analyzer.db.get_type_index_mut().force_bind_type(
            (*member_id).into(),
            LuaTypeCache::InferType(LuaType::Array(
                LuaArrayType::from_base_type(widened_base).into(),
            )),
        );
    }

    Some(())
}

fn widen_member_collections_with_element_type(
    analyzer: &mut LuaAnalyzer,
    member_ids: &[LuaMemberId],
    element_type: &LuaType,
) -> Option<()> {
    for member_id in member_ids {
        let existing_cache = analyzer
            .db
            .get_type_index()
            .get_type_cache(&(*member_id).into())
            .cloned()?;
        if !existing_cache.is_infer() {
            continue;
        }

        let Some(existing_array) =
            normalize_infer_collection_type(analyzer.db, existing_cache.as_type())
        else {
            continue;
        };

        let widened_base =
            TypeOps::Union.apply(analyzer.db, existing_array.get_base(), element_type);
        analyzer.db.get_type_index_mut().force_bind_type(
            (*member_id).into(),
            LuaTypeCache::InferType(LuaType::Array(
                LuaArrayType::from_base_type(widened_base).into(),
            )),
        );
    }

    Some(())
}

fn normalize_infer_collection_type(db: &crate::DbIndex, typ: &LuaType) -> Option<LuaArrayType> {
    match typ {
        LuaType::Array(array) => Some(LuaArrayType::from_base_type(array.get_base().clone())),
        LuaType::Tuple(tuple) if tuple.is_infer_resolve() => {
            Some(LuaArrayType::from_base_type(tuple.cast_down_array_base(db)))
        }
        _ => None,
    }
}

fn is_assignment_file_define_member(db: &crate::DbIndex, member_id: LuaMemberId) -> bool {
    db.get_member_index()
        .get_member(&member_id)
        .is_some_and(|member| {
            member.get_feature() == LuaMemberFeature::FileDefine
                && member.get_syntax_id().get_kind() == glua_parser::LuaSyntaxKind::IndexExpr
        })
}

fn merge_type_owner_and_unresolve_expr(
    analyzer: &mut LuaAnalyzer,
    type_owner: LuaTypeOwner,
    expr: LuaExpr,
    idx: usize,
) -> Option<()> {
    match type_owner {
        LuaTypeOwner::Decl(decl_id) => {
            let unresolve_decl = UnResolveDecl {
                file_id: analyzer.file_id,
                decl_id,
                expr: expr.clone(),
                ret_idx: idx,
            };

            analyzer.context.add_unresolve(
                unresolve_decl.into(),
                InferFailReason::UnResolveExpr(InFiled::new(analyzer.file_id, expr.clone())),
            );
        }
        LuaTypeOwner::Member(member_id) => {
            let unresolve_member = UnResolveMember {
                file_id: analyzer.file_id,
                member_id,
                expr: Some(expr.clone()),
                prefix: None,
                ret_idx: idx,
            };
            analyzer.context.add_unresolve(
                unresolve_member.into(),
                InferFailReason::UnResolveExpr(InFiled::new(analyzer.file_id, expr.clone())),
            );
        }
        _ => {}
    }

    Some(())
}

pub fn analyze_func_stat(analyzer: &mut LuaAnalyzer, func_stat: LuaFuncStat) -> Option<()> {
    let closure = func_stat.get_closure()?;
    let func_name = func_stat.get_func_name()?;
    let signature_type = analyzer.infer_expr(&closure.clone().into()).ok()?;
    let type_owner = get_var_owner(analyzer, func_name.clone());
    set_index_expr_owner(analyzer, func_name.clone());
    analyzer
        .db
        .get_type_index_mut()
        .bind_type(type_owner, LuaTypeCache::InferType(signature_type.clone()));

    Some(())
}

pub fn analyze_local_func_stat(
    analyzer: &mut LuaAnalyzer,
    local_func_stat: LuaLocalFuncStat,
) -> Option<()> {
    let closure = local_func_stat.get_closure()?;
    let func_name = local_func_stat.get_local_name()?;
    let signature_type = analyzer.infer_expr(&closure.clone().into()).ok()?;
    let position = func_name.get_position();
    let decl_id = LuaDeclId::new(analyzer.file_id, position);
    analyzer.db.get_type_index_mut().bind_type(
        decl_id.into(),
        LuaTypeCache::InferType(signature_type.clone()),
    );

    Some(())
}

fn register_expr_key_member(analyzer: &mut LuaAnalyzer, field: &LuaTableField) {
    // Register expression-key members early so table-decl inference (and pairs)
    // can see them even when the table itself has no explicit generic type.
    let Some(field_key) = field.get_field_key() else {
        return;
    };
    let LuaIndexKey::Expr(_) = &field_key else {
        return;
    };
    let member_id = LuaMemberId::new(field.get_syntax_id(), analyzer.file_id);
    if analyzer
        .db
        .get_member_index()
        .get_member(&member_id)
        .is_some()
    {
        return;
    }
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let Ok(member_key) = LuaMemberKey::from_index_key(analyzer.db, cache, &field_key) else {
        return;
    };
    if matches!(member_key, LuaMemberKey::ExprType(ref typ) if typ.is_unknown()) {
        return;
    }
    let Some(table_expr) = field.get_parent::<LuaTableExpr>() else {
        return;
    };
    let owner_id = LuaMemberOwner::Element(InFiled::new(analyzer.file_id, table_expr.get_range()));
    let decl_feature = if analyzer.context.metas.contains(&analyzer.file_id) {
        LuaMemberFeature::MetaDefine
    } else {
        LuaMemberFeature::FileDefine
    };
    let member = LuaMember::new(member_id, member_key, decl_feature, None);
    analyzer
        .db
        .get_member_index_mut()
        .add_member(owner_id, member);
}

pub fn analyze_table_field(analyzer: &mut LuaAnalyzer, field: LuaTableField) -> Option<()> {
    register_expr_key_member(analyzer, &field);

    if field.is_assign_field() {
        let value_expr = field.get_value_expr()?;
        let member_id = LuaMemberId::new(field.get_syntax_id(), analyzer.file_id);
        let value_type = match analyzer.infer_expr(&value_expr.clone()) {
            Ok(value_type) => match value_type {
                LuaType::Def(ref_id) => LuaType::Ref(ref_id),
                _ => value_type,
            },
            Err(InferFailReason::None) => LuaType::Unknown,
            Err(reason) => {
                let unresolve = UnResolveMember {
                    file_id: analyzer.file_id,
                    member_id,
                    expr: Some(value_expr.clone()),
                    prefix: None,
                    ret_idx: 0,
                };

                analyzer.context.add_unresolve(unresolve.into(), reason);
                return None;
            }
        };
        bind_type(
            analyzer.db,
            member_id.into(),
            LuaTypeCache::InferType(value_type),
        );
    }
    Some(())
}

fn special_assign_pattern(
    analyzer: &mut LuaAnalyzer,
    type_owner: LuaTypeOwner,
    var: LuaVarExpr,
    expr: LuaExpr,
) -> Option<()> {
    let access_path = var.get_access_path()?;
    let binary_expr = if let LuaExpr::BinaryExpr(binary_expr) = expr {
        binary_expr
    } else {
        return None;
    };

    if binary_expr.get_op_token()?.get_op() != BinaryOperator::OpOr {
        return None;
    }

    let (left, right) = binary_expr.get_exprs()?;
    let left_var = LuaVarExpr::cast(left.syntax().clone())?;
    let left_access_path = left_var.get_access_path()?;
    if access_path != left_access_path {
        return None;
    }

    match analyzer.infer_expr(&right) {
        Ok(right_expr_type) => {
            assign_merge_type_owner_and_expr_type(analyzer, type_owner, &right_expr_type, 0);
        }
        Err(_) => return None,
    }

    Some(())
}

fn has_delayed_definition_attribute(analyzer: &LuaAnalyzer, decl_id: LuaDeclId) -> bool {
    if let Some(property) = analyzer
        .db
        .get_property_index()
        .get_property(&LuaSemanticDeclId::LuaDecl(decl_id))
    {
        if let Some(lsp_optimization) = property.find_attribute_use("lsp_optimization") {
            if let Some(LuaType::DocStringConst(code)) = lsp_optimization.get_param_by_name("code")
            {
                if code.as_ref() == "delayed_definition" {
                    return true;
                }
            };
        }
    }
    false
}

fn is_local_mutable(analyzer: &LuaAnalyzer, decl_id: LuaDeclId) -> bool {
    analyzer
        .db
        .get_reference_index()
        .get_decl_references(&analyzer.file_id, &decl_id)
        .map(|decl_ref| decl_ref.mutable)
        .unwrap_or(false)
}

// 获取延迟定义的声明id
fn get_delayed_definition_decl_id(
    analyzer: &LuaAnalyzer,
    name_expr: &LuaNameExpr,
) -> Option<LuaDeclId> {
    let file_id = analyzer.file_id;
    let references_index = analyzer.db.get_reference_index();
    let range = name_expr.get_range();
    let file_ref = references_index.get_local_reference(&file_id)?;
    let decl_id = file_ref.get_decl_id(&range)?;
    if !has_delayed_definition_attribute(analyzer, decl_id) {
        return None;
    }
    Some(decl_id)
}
