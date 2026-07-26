use std::collections::HashSet;

use crate::{
    CacheEntry, InFiled, InferFailReason, LuaMemberKey, LuaSemanticDeclId, LuaSignatureId,
    LuaTypeCache, LuaTypeOwner, LuaUnionType, TypeOps,
    compilation::analyzer::{
        common::{TypeCacheWriteMode, add_member, bind_type, write_type_cache},
        gmod::name_expr_resolves_to_scoped_authoring_table,
        unresolve::{UnResolveDecl, UnResolveMember},
    },
    db_index::{LuaDeclId, LuaMember, LuaMemberFeature, LuaMemberId, LuaMemberOwner, LuaType},
    semantic::{merge_open_table_types, remove_false_or_nil},
};
use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAstNode, LuaExpr, LuaFuncStat, LuaIndexExpr, LuaIndexKey,
    LuaLiteralToken, LuaLocalFuncStat, LuaLocalStat, LuaNameExpr, LuaSyntaxKind, LuaTableExpr,
    LuaTableField, LuaVarExpr, PathTrait,
};
use rustc_hash::FxHashMap;

#[cfg(test)]
use crate::{GmodStateMask, LuaArrayType};

use super::{
    LuaAnalyzer,
    member_write_policy::{
        MemberAssignmentWideningCacheKey, MemberAssignmentWideningDecision,
        MemberAssignmentWideningState, WideningCacheLookup, decide_member_assignment_widening,
        direct_local_prefix_has_declared_type, direct_local_table_prefix_member_owner,
        flush_pending_dynamic_key_collection_widening_for_members,
        get_widened_member_assignment_collection_type, is_collection_append_write,
        is_member_realm_compatible, lookup_widening_cache, member_assignment_state_mask,
        member_assignment_state_masks_compatible, merge_member_assignment_widening_state,
        record_member_collection_assignment_widening_cache, record_widening_cache,
        resolve_index_expr_member_owner_for_file, union_member_assignment_widening,
        widen_existing_member_collection_type, widen_related_assignment_type,
    },
};

#[cfg(test)]
use super::member_write_policy::{
    get_cached_widened_member_collection_assignment_type,
    record_pending_dynamic_key_collection_widening,
};

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
            write_type_cache(
                analyzer.db,
                decl_id.into(),
                LuaTypeCache::InferType(LuaType::Nil),
                TypeCacheWriteMode::InsertOnly,
            );
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
        let decl_id = LuaDeclId::new(analyzer.file_id, position);
        if is_call_or_index_expr(&expr) {
            analyzer
                .context
                .request_uninformative_local_decl_reinfer(decl_id);
        }

        if let Some(reason) = should_defer_guarded_index_alias(analyzer, &expr) {
            let unresolve = UnResolveDecl {
                file_id: analyzer.file_id,
                decl_id,
                expr: expr.clone(),
                ret_idx: 0,
            };
            analyzer.context.add_unresolve(unresolve.into(), reason);
            continue;
        }

        match analyzer.infer_expr(&expr) {
            Ok(mut expr_type) => {
                if let LuaType::Variadic(multi) = expr_type {
                    expr_type = multi.get_type(0)?.clone();
                }
                if expr_type.is_nil() && should_defer_nil_gmod_expr(analyzer, &expr) {
                    let unresolve = UnResolveDecl {
                        file_id: analyzer.file_id,
                        decl_id,
                        expr: expr.clone(),
                        ret_idx: 0,
                    };
                    analyzer
                        .context
                        .add_unresolve(unresolve.into(), InferFailReason::FieldNotFound);
                    continue;
                }
                if should_defer_pending_local_alias(analyzer, &expr, &expr_type) {
                    let unresolve = UnResolveDecl {
                        file_id: analyzer.file_id,
                        decl_id,
                        expr: expr.clone(),
                        ret_idx: 0,
                    };
                    analyzer
                        .context
                        .add_unresolve(unresolve.into(), InferFailReason::FieldNotFound);
                    continue;
                }
                if should_defer_weak_gmod_call_expr(analyzer, &expr, &expr_type) {
                    let unresolve = UnResolveDecl {
                        file_id: analyzer.file_id,
                        decl_id,
                        expr: expr.clone(),
                        ret_idx: 0,
                    };
                    analyzer
                        .context
                        .add_unresolve(unresolve.into(), InferFailReason::FieldNotFound);
                    continue;
                }
                if should_defer_nil_gmod_index_alias(analyzer, &expr, &expr_type)
                    || should_defer_weak_gmod_dynamic_index_alias(analyzer, &expr, &expr_type)
                {
                    clear_index_expr_type_cache(analyzer, &expr);
                    let unresolve = UnResolveDecl {
                        file_id: analyzer.file_id,
                        decl_id,
                        expr: expr.clone(),
                        ret_idx: 0,
                    };
                    analyzer
                        .context
                        .add_unresolve(unresolve.into(), InferFailReason::FieldNotFound);
                    continue;
                }

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
                if should_defer_none_infer_expr(&expr) {
                    let unresolve = UnResolveDecl {
                        file_id: analyzer.file_id,
                        decl_id,
                        expr: expr.clone(),
                        ret_idx: 0,
                    };
                    analyzer
                        .context
                        .add_unresolve(unresolve.into(), InferFailReason::FieldNotFound);
                } else {
                    write_type_cache(
                        analyzer.db,
                        decl_id.into(),
                        LuaTypeCache::InferType(LuaType::Nil),
                        TypeCacheWriteMode::InsertOnly,
                    );
                }
            }
            Err(reason) => {
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
            let last_expr_is_call = matches!(last_expr, LuaExpr::CallExpr(_));
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
                                write_type_cache(
                                    analyzer.db,
                                    decl_id.into(),
                                    LuaTypeCache::InferType(LuaType::Nil),
                                    TypeCacheWriteMode::InsertOnly,
                                );
                            }
                        }
                        return Some(());
                    } else {
                        // Preserve unknown call arity; known non-variadic values
                        // retain the legacy `any` convention for extra slots.
                        for i in expr_count..name_count {
                            let name = name_list.get(i)?;
                            let position = name.get_position();
                            let decl_id = LuaDeclId::new(analyzer.file_id, position);
                            let typ = if last_expr_type.is_unknown() && last_expr_is_call {
                                LuaType::Unknown
                            } else {
                                LuaType::Any
                            };
                            bind_type(analyzer.db, decl_id.into(), LuaTypeCache::InferType(typ));
                        }
                        return Some(());
                    }
                }
                Err(reason) => {
                    for i in expr_count..name_count {
                        let name = name_list.get(i)?;
                        let position = name.get_position();
                        let decl_id = LuaDeclId::new(analyzer.file_id, position);
                        if last_expr_is_call {
                            bind_type(
                                analyzer.db,
                                decl_id.into(),
                                LuaTypeCache::InferType(LuaType::Unknown),
                            );
                        }
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
                write_type_cache(
                    analyzer.db,
                    decl_id.into(),
                    LuaTypeCache::InferType(LuaType::Nil),
                    TypeCacheWriteMode::InsertOnly,
                );
            }
        }
    }

    Some(())
}

fn should_defer_guarded_index_alias(
    analyzer: &mut LuaAnalyzer,
    expr: &LuaExpr,
) -> Option<InferFailReason> {
    let left = guarded_index_or_empty_table_left(expr)?;
    match analyzer.infer_expr(&left) {
        Ok(ty) if ty.is_unknown() || ty.is_nil() => Some(InferFailReason::FieldNotFound),
        Err(reason) if reason.is_need_resolve() => Some(reason),
        _ => None,
    }
}

fn guarded_index_or_empty_table_left(expr: &LuaExpr) -> Option<LuaExpr> {
    let LuaExpr::BinaryExpr(binary_expr) = expr else {
        return None;
    };
    if binary_expr.get_op_token().map(|op| op.get_op()) != Some(BinaryOperator::OpOr) {
        return None;
    }
    let (left, right) = binary_expr.get_exprs()?;
    if !matches!(left, LuaExpr::IndexExpr(_)) {
        return None;
    }
    if !matches!(right, LuaExpr::TableExpr(table_expr) if table_expr.is_empty()) {
        return None;
    }

    Some(left)
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

fn should_defer_nil_gmod_index_alias(
    analyzer: &LuaAnalyzer,
    expr: &LuaExpr,
    expr_type: &LuaType,
) -> bool {
    expr_type.is_nil()
        && analyzer.gmod_enabled
        && analyzer.db.get_emmyrc().gmod.infer_dynamic_fields
        && matches!(expr, LuaExpr::IndexExpr(_))
}

fn should_defer_weak_gmod_member_index_assignment(
    analyzer: &LuaAnalyzer,
    type_owner: &LuaTypeOwner,
    expr: &LuaExpr,
    expr_type: &LuaType,
) -> bool {
    matches!(expr_type, LuaType::Any | LuaType::Unknown)
        && analyzer.gmod_enabled
        && analyzer.db.get_emmyrc().gmod.infer_dynamic_fields
        && matches!(expr, LuaExpr::IndexExpr(_))
        && matches!(type_owner, LuaTypeOwner::Member(_))
}

fn should_defer_weak_gmod_dynamic_index_alias(
    analyzer: &LuaAnalyzer,
    expr: &LuaExpr,
    expr_type: &LuaType,
) -> bool {
    analyzer.gmod_enabled
        && analyzer.db.get_emmyrc().gmod.infer_dynamic_fields
        && matches!(expr, LuaExpr::IndexExpr(index_expr) if matches!(index_expr.get_index_key(), Some(LuaIndexKey::Expr(_))))
        && is_weak_dynamic_index_alias_type(expr_type)
}

fn is_weak_dynamic_index_alias_type(expr_type: &LuaType) -> bool {
    match expr_type {
        LuaType::Any | LuaType::Unknown => true,
        LuaType::Union(union) => match union.as_ref() {
            LuaUnionType::Nullable(inner) => inner.is_any() || inner.is_unknown(),
            LuaUnionType::Multi(_) => false,
        },
        _ => false,
    }
}

fn index_expr_prefix_is_self(index_expr: &LuaIndexExpr) -> bool {
    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return false;
    };

    matches!(
        prefix_expr,
        LuaExpr::NameExpr(name_expr) if name_expr.get_name_text().as_deref() == Some("self")
    )
}

fn clear_index_expr_type_cache(analyzer: &mut LuaAnalyzer, expr: &LuaExpr) {
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let mut current_expr = expr.clone();
    while let LuaExpr::IndexExpr(index_expr) = current_expr {
        let syntax_id = index_expr.get_syntax_id();
        if matches!(cache.expr_cache.get(&syntax_id), Some(CacheEntry::Cache(typ)) if typ.is_nil() || is_weak_dynamic_index_alias_type(typ))
        {
            cache.expr_cache.remove(&syntax_id);
            cache.expr_var_ref_id_cache.remove(&syntax_id);
        }
        let Some(prefix_expr) = index_expr.get_prefix_expr() else {
            break;
        };
        current_expr = prefix_expr;
    }
}

fn get_var_owner(analyzer: &mut LuaAnalyzer, var: LuaVarExpr) -> LuaTypeOwner {
    let file_id = analyzer.file_id;
    match var {
        LuaVarExpr::NameExpr(var_name) => {
            let maybe_decl_id = LuaDeclId::new(file_id, var_name.get_position());
            if analyzer
                .db
                .get_decl_index()
                .get_decl(&maybe_decl_id)
                .is_some()
            {
                return LuaTypeOwner::Decl(maybe_decl_id);
            }

            let decl_id = analyzer
                .db
                .get_reference_index()
                .get_local_reference(&file_id)
                .and_then(|file_ref| file_ref.get_decl_id(&var_name.get_range()))
                .unwrap_or_else(|| LuaDeclId::new(file_id, var_name.get_position()));
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
    let index_expr = LuaIndexExpr::cast(var_expr.syntax().clone())?;
    let prefix_expr = index_expr.get_prefix_expr()?;

    if let Some((member_owner, set_owner_only)) =
        try_resolve_scoped_class_prefix_member_owner(analyzer, &prefix_expr)
    {
        apply_index_expr_member_owner(analyzer, index_expr, member_owner, set_owner_only);
        return Some(());
    }

    if let Some(member_owner) = direct_local_table_prefix_member_owner(analyzer, &prefix_expr) {
        apply_index_expr_member_owner(analyzer, index_expr, member_owner, false);
        return Some(());
    }

    if let Some(member_owner) = cached_literal_index_prefix_member_owner(analyzer, &prefix_expr) {
        apply_index_expr_member_owner(analyzer, index_expr, member_owner, false);
        return Some(());
    }

    match analyzer.infer_expr(&prefix_expr.clone()) {
        Ok(prefix_type) => {
            if should_skip_ambiguous_unknown_key_table_owner(analyzer, &prefix_type, &index_expr) {
                return Some(());
            }
            let (member_owner, set_owner_only) =
                resolve_index_expr_member_owner_for_file(&prefix_type, Some(analyzer.file_id))?;
            apply_index_expr_member_owner(analyzer, index_expr, member_owner, set_owner_only);
        }
        Err(InferFailReason::None) => {}
        Err(reason) => {
            // record unresolve
            let unresolve_member = UnResolveMember {
                file_id: analyzer.file_id,
                member_id: LuaMemberId::new(var_expr.get_syntax_id(), analyzer.file_id),
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

fn should_skip_ambiguous_unknown_key_table_owner(
    analyzer: &mut LuaAnalyzer,
    prefix_type: &LuaType,
    index_expr: &LuaIndexExpr,
) -> bool {
    let Some(index_key) = index_expr.get_index_key() else {
        return false;
    };
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let Ok(member_key) = LuaMemberKey::from_index_key_or_unknown(analyzer.db, cache, &index_key)
    else {
        return false;
    };
    if !matches!(member_key, LuaMemberKey::ExprType(ref typ) if typ.is_unknown()) {
        return false;
    }

    has_multiple_distinct_index_expr_member_owners(prefix_type)
}

fn has_multiple_distinct_index_expr_member_owners(typ: &LuaType) -> bool {
    let mut owners = HashSet::new();
    collect_distinct_index_expr_member_owners(typ, &mut owners);
    owners.len() > 1
}

fn collect_distinct_index_expr_member_owners(
    typ: &LuaType,
    owners: &mut HashSet<LuaMemberOwner>,
) -> bool {
    match typ {
        LuaType::TableConst(in_file_range) => {
            insert_index_expr_member_owner(owners, LuaMemberOwner::Element(in_file_range.clone()))
        }
        LuaType::Def(def_id) => {
            insert_index_expr_member_owner(owners, LuaMemberOwner::Type(def_id.clone()))
        }
        LuaType::Ref(ref_id) => {
            insert_index_expr_member_owner(owners, LuaMemberOwner::Type(ref_id.clone()))
        }
        LuaType::Instance(instance) => insert_index_expr_member_owner(
            owners,
            LuaMemberOwner::Element(instance.get_range().clone()),
        ),
        LuaType::TableOf(inner) => collect_distinct_index_expr_member_owners(inner, owners),
        LuaType::TypeGuard(inner) => collect_distinct_index_expr_member_owners(inner, owners),
        LuaType::Union(union) => {
            for typ in union.types() {
                if collect_distinct_index_expr_member_owners(typ, owners) {
                    return true;
                }
            }
            false
        }
        LuaType::Intersection(intersection) => {
            for typ in intersection.get_types() {
                if collect_distinct_index_expr_member_owners(typ, owners) {
                    return true;
                }
            }
            false
        }
        LuaType::MergedTable(merged_table) => {
            for typ in merged_table.get_types() {
                if collect_distinct_index_expr_member_owners(typ, owners) {
                    return true;
                }
            }
            false
        }
        LuaType::MultiLineUnion(union) => {
            for (typ, _) in union.get_unions() {
                if collect_distinct_index_expr_member_owners(typ, owners) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

fn insert_index_expr_member_owner(
    owners: &mut HashSet<LuaMemberOwner>,
    owner: LuaMemberOwner,
) -> bool {
    owners.insert(owner);
    owners.len() > 1
}

fn try_resolve_scoped_class_prefix_member_owner(
    analyzer: &LuaAnalyzer,
    prefix_expr: &LuaExpr,
) -> Option<(LuaMemberOwner, bool)> {
    let LuaExpr::NameExpr(name_expr) = prefix_expr else {
        return None;
    };

    let name = name_expr.get_name_text()?;
    if name != "self" {
        let class_decl_id =
            name_expr_resolves_to_scoped_authoring_table(analyzer.db, analyzer.file_id, name_expr)?;
        return Some((LuaMemberOwner::Type(class_decl_id), false));
    }

    if !name_expr_resolves_to_implicit_self(analyzer, name_expr) {
        return None;
    }

    let func_stat = name_expr.ancestors::<LuaFuncStat>().next()?;
    let LuaVarExpr::IndexExpr(func_name) = func_stat.get_func_name()? else {
        return None;
    };
    if !func_name.get_index_token()?.is_colon() {
        return None;
    }

    let LuaExpr::NameExpr(class_name_expr) = func_name.get_prefix_expr()? else {
        return None;
    };
    let class_decl_id = name_expr_resolves_to_scoped_authoring_table(
        analyzer.db,
        analyzer.file_id,
        &class_name_expr,
    )?;
    Some((LuaMemberOwner::Type(class_decl_id), false))
}

fn name_expr_resolves_to_implicit_self(analyzer: &LuaAnalyzer, name_expr: &LuaNameExpr) -> bool {
    analyzer
        .db
        .get_reference_index()
        .get_local_reference(&analyzer.file_id)
        .and_then(|file_ref| file_ref.get_decl_id(&name_expr.get_range()))
        .and_then(|decl_id| analyzer.db.get_decl_index().get_decl(&decl_id))
        .is_some_and(|decl| decl.is_implicit_self())
}

fn apply_index_expr_member_owner(
    analyzer: &mut LuaAnalyzer,
    index_expr: LuaIndexExpr,
    member_owner: LuaMemberOwner,
    set_owner_only: bool,
) -> Option<()> {
    let guarded_table_assignment = is_guarded_table_assignment_index_expr(&index_expr);
    apply_index_expr_member_owner_with_guarded(
        analyzer,
        index_expr,
        member_owner,
        set_owner_only,
        guarded_table_assignment,
    )
}

fn apply_index_expr_member_owner_with_guarded(
    analyzer: &mut LuaAnalyzer,
    index_expr: LuaIndexExpr,
    member_owner: LuaMemberOwner,
    set_owner_only: bool,
    guarded_table_assignment: bool,
) -> Option<()> {
    let index_key = index_expr.get_index_key()?;
    let member_id = LuaMemberId::new(index_expr.get_syntax_id(), analyzer.file_id);

    if analyzer
        .db
        .get_member_index()
        .get_member(&member_id)
        .is_none()
    {
        let cache = analyzer
            .context
            .infer_manager
            .get_infer_cache(analyzer.file_id);
        let Ok(member_key) =
            LuaMemberKey::from_index_key_or_unknown(analyzer.db, cache, &index_key)
        else {
            return Some(());
        };
        let decl_feature = if analyzer.context.metas.contains(&analyzer.file_id) {
            LuaMemberFeature::MetaDefine
        } else {
            LuaMemberFeature::FileDefine
        };
        let member = LuaMember::new(member_id, member_key, decl_feature, None);
        let guarded_file_define =
            guarded_table_assignment && matches!(decl_feature, LuaMemberFeature::FileDefine);
        if guarded_table_assignment {
            analyzer
                .db
                .get_member_index_mut()
                .mark_non_overwriting_assignment_member(member_id);
        }
        let member_index = analyzer.db.get_member_index_mut();
        member_index.add_member(member_owner, member);
        // `add_member` already records the enclosing function scope for
        // `FileDefine` index-expr members (via
        // `assignment_file_define_scope_for_member`). For other features
        // (e.g. `MetaDefine`) it stores `None`, so set the real scope here.
        if !matches!(decl_feature, LuaMemberFeature::FileDefine) {
            let function_scope = member_index
                .enclosing_function_scope_range(analyzer.file_id, member_id.get_position());
            member_index.set_member_function_scope_range(member_id, function_scope);
        }
        if guarded_table_assignment && !guarded_file_define {
            preserve_guarded_table_assignment_members(analyzer, member_id);
        }
        return Some(());
    }

    if set_owner_only {
        if guarded_table_assignment {
            analyzer
                .db
                .get_member_index_mut()
                .mark_non_overwriting_assignment_member(member_id);
        }
        let function_scope = analyzer
            .db
            .get_member_index()
            .enclosing_function_scope_range(analyzer.file_id, member_id.get_position());
        {
            let member_index = analyzer.db.get_member_index_mut();
            member_index.set_member_owner(member_owner, member_id.file_id, member_id);
            member_index.set_member_function_scope_range(member_id, function_scope);
        }
        if guarded_table_assignment {
            preserve_guarded_table_assignment_members(analyzer, member_id);
        }
        return Some(());
    }

    let guarded_existing_file_define = guarded_table_assignment
        && analyzer
            .db
            .get_member_index()
            .get_member(&member_id)
            .is_some_and(|member| member.get_feature() == LuaMemberFeature::FileDefine);

    if guarded_table_assignment {
        analyzer
            .db
            .get_member_index_mut()
            .mark_non_overwriting_assignment_member(member_id);
    }
    let function_scope = analyzer
        .db
        .get_member_index()
        .enclosing_function_scope_range(analyzer.file_id, member_id.get_position());
    add_member(analyzer.db, member_owner, member_id);
    analyzer
        .db
        .get_member_index_mut()
        .set_member_function_scope_range(member_id, function_scope);
    if guarded_table_assignment && !guarded_existing_file_define {
        preserve_guarded_table_assignment_members(analyzer, member_id);
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

        if should_skip_nil_table_shape_assignment(analyzer, &var, expr) {
            continue;
        }
        if should_skip_nullable_collection_append_shape_assignment(analyzer, &var, expr) {
            continue;
        }

        let type_owner = get_var_owner(analyzer, var.clone());

        let assign_stat_range = assign_stat.get_range();
        if special_assign_pattern(
            analyzer,
            type_owner.clone(),
            var.clone(),
            expr.clone(),
            assign_stat_range,
        )
        .is_some()
        {
            continue;
        }

        let declared_empty_table_type = declared_empty_table_assignment_type(analyzer, &var, expr);
        set_index_expr_owner(analyzer, var.clone());

        let expr_type = match declared_empty_table_type
            .map(Ok)
            .unwrap_or_else(|| analyzer.infer_expr(expr))
        {
            Ok(mut expr_type) => {
                if let LuaType::Variadic(multi) = expr_type {
                    expr_type = multi.get_type(0)?.clone();
                }

                if expr_type.is_nil() && should_defer_nil_gmod_expr(analyzer, expr) {
                    add_unresolve_for_assignment(
                        analyzer,
                        type_owner,
                        &var,
                        expr.clone(),
                        InferFailReason::FieldNotFound,
                    );
                    continue;
                }
                if should_defer_pending_local_alias(analyzer, expr, &expr_type) {
                    add_unresolve_for_assignment(
                        analyzer,
                        type_owner,
                        &var,
                        expr.clone(),
                        InferFailReason::FieldNotFound,
                    );
                    continue;
                }
                if should_defer_weak_gmod_call_expr(analyzer, expr, &expr_type) {
                    add_unresolve_for_assignment(
                        analyzer,
                        type_owner,
                        &var,
                        expr.clone(),
                        InferFailReason::FieldNotFound,
                    );
                    continue;
                }
                if should_defer_weak_gmod_member_index_assignment(
                    analyzer,
                    &type_owner,
                    expr,
                    &expr_type,
                ) {
                    add_unresolve_for_assignment(
                        analyzer,
                        type_owner,
                        &var,
                        expr.clone(),
                        InferFailReason::FieldNotFound,
                    );
                    continue;
                }
                if expr_type.is_unknown() && is_undefined_global_name_expr(analyzer, expr) {
                    // See note in analyze_local_stat: undefined-global RHS
                    // is `nil` at runtime, not "unknown".
                    LuaType::Nil
                } else {
                    expr_type
                }
            }
            // Reading an undefined global yields `nil` at runtime, so the
            // assignment target's value is `nil` (not unknown). This mirrors
            // the local-stat path above so hover/inference stays consistent.
            Err(InferFailReason::None) => {
                if should_defer_none_infer_expr(expr) {
                    add_unresolve_for_assignment(
                        analyzer,
                        type_owner,
                        &var,
                        expr.clone(),
                        InferFailReason::FieldNotFound,
                    );
                    continue;
                }
                LuaType::Nil
            }
            Err(reason) => {
                add_unresolve_for_assignment(analyzer, type_owner, &var, expr.clone(), reason);
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

        if analyzer.gmod_enabled
            && matches!(
                expr,
                LuaExpr::CallExpr(_) | LuaExpr::IndexExpr(_) | LuaExpr::NameExpr(_)
            )
            && type_contains_nominal_reference(&expr_type)
            && let LuaTypeOwner::Member(member_id) = &type_owner
        {
            analyzer
                .context
                .request_member_initializer_reinfer(*member_id);
        }

        let expr_type = member_assignment_or_source_type(analyzer, &type_owner, expr, expr_type);

        widen_existing_member_collection_type(analyzer, &var, &expr_type);
        assign_merge_type_owner_and_expr_type(analyzer, type_owner, &expr_type, 0, false);
        update_literal_index_member_owner_cache(analyzer, &var, &expr_type);
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
                            false,
                        );
                    }
                } else {
                    for i in expr_count..var_count {
                        let var = var_list.get(i)?;
                        let type_owner = get_var_owner(analyzer, var.clone());
                        set_index_expr_owner(analyzer, var.clone());
                        assign_merge_type_owner_and_expr_type(
                            analyzer,
                            type_owner,
                            &LuaType::Any,
                            0, // Any doesn't need indexing
                            false,
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

    Some(())
}

fn declared_empty_table_assignment_type(
    analyzer: &mut LuaAnalyzer,
    var: &LuaVarExpr,
    expr: &LuaExpr,
) -> Option<LuaType> {
    let LuaExpr::TableExpr(table_expr) = expr else {
        return None;
    };
    if table_expr.get_fields().next().is_some() {
        return None;
    }

    let LuaVarExpr::IndexExpr(index_expr) = var else {
        return None;
    };
    let prefix_expr = index_expr.get_prefix_expr()?;
    if !direct_local_prefix_has_declared_type(analyzer, &prefix_expr) {
        return None;
    }

    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let declared_type =
        crate::infer_index_expr(analyzer.db, cache, index_expr.clone(), false).ok()?;
    let table_value_type = declared_table_assignment_type(analyzer.db, &declared_type)?;

    write_type_cache(
        analyzer.db,
        LuaTypeOwner::SyntaxId(InFiled::new(analyzer.file_id, table_expr.get_syntax_id())),
        LuaTypeCache::InferType(table_value_type.clone()),
        TypeCacheWriteMode::InsertOnly,
    );

    Some(table_value_type)
}

fn declared_table_assignment_type(db: &crate::DbIndex, declared_type: &LuaType) -> Option<LuaType> {
    match declared_type {
        typ if typ.is_table() => Some(typ.clone()),
        LuaType::Ref(type_id) | LuaType::Def(type_id)
            if db
                .get_type_index()
                .get_type_decl(type_id)
                .is_some_and(|decl| decl.is_class()) =>
        {
            Some(declared_type.clone())
        }
        LuaType::Instance(instance)
            if declared_table_assignment_type(db, instance.get_base()).is_some() =>
        {
            Some(declared_type.clone())
        }
        LuaType::TypeGuard(inner) => declared_table_assignment_type(db, inner),
        LuaType::Union(union) => union
            .types()
            .filter_map(|typ| declared_table_assignment_type(db, typ))
            .fold(None, |result, typ| {
                Some(match result {
                    Some(current) => TypeOps::Union.apply(db, &current, &typ),
                    None => typ,
                })
            }),
        LuaType::Intersection(intersection)
            if intersection
                .get_types()
                .iter()
                .any(|typ| declared_table_assignment_type(db, typ).is_some()) =>
        {
            Some(declared_type.clone())
        }
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .filter_map(|(typ, _)| declared_table_assignment_type(db, typ))
            .fold(None, |result, typ| {
                Some(match result {
                    Some(current) => TypeOps::Union.apply(db, &current, &typ),
                    None => typ,
                })
            }),
        _ => None,
    }
}

fn type_contains_nominal_reference(typ: &LuaType) -> bool {
    match typ {
        LuaType::Def(_) | LuaType::Ref(_) => true,
        LuaType::Instance(instance) => type_contains_nominal_reference(instance.get_base()),
        LuaType::Union(union) => union.types().any(type_contains_nominal_reference),
        _ => false,
    }
}

fn cached_literal_index_prefix_member_owner(
    analyzer: &LuaAnalyzer,
    prefix_expr: &LuaExpr,
) -> Option<LuaMemberOwner> {
    let LuaExpr::IndexExpr(index_expr) = prefix_expr else {
        return None;
    };
    if !is_literal_index_cache_candidate(index_expr) {
        return None;
    }

    let access_path = index_expr.get_access_path()?;
    analyzer
        .literal_index_member_owner_cache
        .get(&access_path)
        .cloned()
}

fn update_literal_index_member_owner_cache(
    analyzer: &mut LuaAnalyzer,
    var: &LuaVarExpr,
    expr_type: &LuaType,
) -> Option<()> {
    let LuaVarExpr::IndexExpr(index_expr) = var else {
        return None;
    };
    if !is_literal_index_cache_candidate(index_expr) {
        return None;
    }

    let access_path = index_expr.get_access_path()?;
    if let Some(owner) = current_file_table_member_owner(analyzer, expr_type) {
        analyzer
            .literal_index_member_owner_cache
            .insert(access_path, owner);
    } else {
        analyzer
            .literal_index_member_owner_cache
            .remove(&access_path);
    }
    Some(())
}

fn is_literal_index_cache_candidate(index_expr: &LuaIndexExpr) -> bool {
    matches!(
        index_expr.get_index_key(),
        Some(LuaIndexKey::String(_) | LuaIndexKey::Integer(_))
    )
}

fn current_file_table_member_owner(
    analyzer: &LuaAnalyzer,
    typ: &LuaType,
) -> Option<LuaMemberOwner> {
    let LuaType::TableConst(range) = typ else {
        return None;
    };
    (range.file_id == analyzer.file_id).then(|| LuaMemberOwner::Element(range.clone()))
}

fn should_skip_nil_table_shape_assignment(
    analyzer: &mut LuaAnalyzer,
    var: &LuaVarExpr,
    expr: &LuaExpr,
) -> bool {
    if !is_nil_literal_expr(expr) {
        return false;
    }

    let LuaVarExpr::IndexExpr(index_expr) = var else {
        return false;
    };

    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return false;
    };

    let Ok(prefix_type) = analyzer.infer_expr(&prefix_expr) else {
        return false;
    };

    if !is_table_shape_cleanup_type(&prefix_type) {
        return false;
    }

    if is_typed_collection_cleanup_type(&prefix_type) {
        return true;
    }

    let Some((owner, _)) =
        resolve_index_expr_member_owner_for_file(&prefix_type, Some(analyzer.file_id))
    else {
        return false;
    };

    let Some(index_key) = index_expr.get_index_key() else {
        return false;
    };

    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let Ok(member_key) = LuaMemberKey::from_index_key_or_unknown(analyzer.db, cache, &index_key)
    else {
        return false;
    };
    if member_key.is_expr() {
        return true;
    }

    let member_id = LuaMemberId::new(index_expr.get_syntax_id(), analyzer.file_id);
    !analyzer
        .db
        .get_member_index()
        .has_visible_member_for_owner_key_other_than(&owner, &member_key, member_id)
}

fn should_skip_nullable_collection_append_shape_assignment(
    analyzer: &mut LuaAnalyzer,
    var: &LuaVarExpr,
    expr: &LuaExpr,
) -> bool {
    let LuaVarExpr::IndexExpr(index_expr) = var else {
        return false;
    };
    if !is_collection_append_write(index_expr).unwrap_or(false) {
        return false;
    }

    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return false;
    };
    let Ok(prefix_type) = analyzer.infer_expr(&prefix_expr) else {
        return false;
    };
    if !is_typed_collection_cleanup_type(&prefix_type) {
        return false;
    }

    analyzer
        .infer_expr(expr)
        .is_ok_and(|expr_type| expr_type.is_nullable())
}

fn is_nil_literal_expr(expr: &LuaExpr) -> bool {
    match expr {
        LuaExpr::LiteralExpr(literal_expr) => {
            matches!(literal_expr.get_literal(), Some(LuaLiteralToken::Nil(_)))
        }
        LuaExpr::ParenExpr(paren_expr) => paren_expr
            .get_expr()
            .is_some_and(|expr| is_nil_literal_expr(&expr)),
        _ => false,
    }
}

fn is_table_shape_cleanup_type(typ: &LuaType) -> bool {
    match typ {
        LuaType::Table
        | LuaType::TableConst(_)
        | LuaType::TableGeneric(_)
        | LuaType::Array(_)
        | LuaType::Tuple(_)
        | LuaType::TableOf(_) => true,
        LuaType::TypeGuard(inner) => is_table_shape_cleanup_type(inner),
        LuaType::Union(union) => {
            union.types().next().is_some()
                && union
                    .types()
                    .all(|typ| typ.is_nil() || typ.is_never() || is_table_shape_cleanup_type(typ))
        }
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .all(is_table_shape_cleanup_type),
        LuaType::MergedTable(merged_table) => merged_table
            .get_types()
            .iter()
            .all(is_table_shape_cleanup_type),
        LuaType::MultiLineUnion(union) => {
            let types = union.get_unions();
            !types.is_empty()
                && types.iter().all(|(typ, _)| {
                    typ.is_nil() || typ.is_never() || is_table_shape_cleanup_type(typ)
                })
        }
        _ => false,
    }
}

fn is_typed_collection_cleanup_type(typ: &LuaType) -> bool {
    match typ {
        LuaType::TableGeneric(_) | LuaType::Array(_) | LuaType::Tuple(_) | LuaType::TableOf(_) => {
            true
        }
        LuaType::TypeGuard(inner) => is_typed_collection_cleanup_type(inner),
        LuaType::Union(union) => {
            union.types().next().is_some()
                && union.types().all(|typ| {
                    typ.is_nil() || typ.is_never() || is_typed_collection_cleanup_type(typ)
                })
        }
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .all(is_typed_collection_cleanup_type),
        LuaType::MergedTable(merged_table) => merged_table
            .get_types()
            .iter()
            .all(is_typed_collection_cleanup_type),
        LuaType::MultiLineUnion(union) => {
            let types = union.get_unions();
            !types.is_empty()
                && types.iter().all(|(typ, _)| {
                    typ.is_nil() || typ.is_never() || is_typed_collection_cleanup_type(typ)
                })
        }
        _ => false,
    }
}

fn should_defer_none_infer_expr(expr: &LuaExpr) -> bool {
    matches!(expr, LuaExpr::CallExpr(_))
}

fn is_call_or_index_expr(expr: &LuaExpr) -> bool {
    matches!(expr, LuaExpr::CallExpr(_) | LuaExpr::IndexExpr(_))
}

fn should_defer_nil_gmod_expr(analyzer: &LuaAnalyzer, expr: &LuaExpr) -> bool {
    if !analyzer.gmod_enabled {
        return false;
    }

    matches!(expr, LuaExpr::CallExpr(_))
}

fn should_defer_weak_gmod_call_expr(
    analyzer: &LuaAnalyzer,
    expr: &LuaExpr,
    expr_type: &LuaType,
) -> bool {
    if !(expr_type.is_any() || expr_type.is_unknown())
        || !analyzer.gmod_enabled
        || !analyzer.is_scripted_class_scope
    {
        return false;
    }

    let LuaExpr::CallExpr(call_expr) = expr else {
        return false;
    };

    weak_call_has_nested_call_argument(call_expr) || weak_call_is_scoped_method_call(call_expr)
}

fn weak_call_has_nested_call_argument(call_expr: &glua_parser::LuaCallExpr) -> bool {
    call_expr.get_args_list().is_some_and(|args| {
        args.get_args().any(|arg| {
            arg.descendants::<LuaExpr>()
                .any(|expr| matches!(expr, LuaExpr::CallExpr(_)))
        })
    })
}

fn weak_call_is_scoped_method_call(call_expr: &glua_parser::LuaCallExpr) -> bool {
    let Some(LuaExpr::IndexExpr(index_expr)) = call_expr.get_prefix_expr() else {
        return false;
    };
    if !index_expr
        .get_index_token()
        .is_some_and(|token| token.is_colon())
    {
        return false;
    }

    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return false;
    };
    matches!(
        prefix_expr,
        LuaExpr::NameExpr(name_expr) if name_expr.get_name_text().as_deref() == Some("self")
    )
}

fn should_defer_pending_local_alias(
    analyzer: &LuaAnalyzer,
    expr: &LuaExpr,
    expr_type: &LuaType,
) -> bool {
    if !(expr_type.is_any() || expr_type.is_unknown() || expr_type.is_nil()) {
        return false;
    }

    let LuaExpr::NameExpr(name_expr) = expr else {
        return false;
    };
    let Some(decl_id) = analyzer
        .db
        .get_reference_index()
        .get_local_reference(&analyzer.file_id)
        .and_then(|file_ref| file_ref.get_decl_id(&name_expr.get_range()))
    else {
        return false;
    };

    analyzer.context.has_pending_decl_unresolve(decl_id)
}

fn add_unresolve_for_assignment(
    analyzer: &mut LuaAnalyzer,
    type_owner: LuaTypeOwner,
    var: &LuaVarExpr,
    expr: LuaExpr,
    reason: InferFailReason,
) {
    match type_owner {
        LuaTypeOwner::Decl(decl_id) => {
            let unresolve_decl = UnResolveDecl {
                file_id: analyzer.file_id,
                decl_id,
                expr,
                ret_idx: 0,
            };

            analyzer
                .context
                .add_unresolve(unresolve_decl.into(), reason);
        }
        LuaTypeOwner::Member(member_id) => {
            let prefix = if !analyzer.is_scripted_class_scope {
                match var {
                    LuaVarExpr::IndexExpr(index_expr) if index_expr_prefix_is_self(index_expr) => {
                        index_expr.get_prefix_expr()
                    }
                    _ => None,
                }
            } else {
                None
            };
            let unresolve_member = UnResolveMember {
                file_id: analyzer.file_id,
                member_id,
                expr: Some(expr),
                prefix,
                ret_idx: 0,
            };
            analyzer
                .context
                .add_unresolve(unresolve_member.into(), reason);
        }
        _ => {}
    }
}

fn assign_merge_type_owner_and_expr_type(
    analyzer: &mut LuaAnalyzer,
    type_owner: LuaTypeOwner,
    expr_type: &LuaType,
    idx: usize,
    preserve_table_literals: bool,
) -> Option<()> {
    let mut expr_type = expr_type.clone();
    if let LuaType::Variadic(multi) = expr_type {
        expr_type = multi.get_type(idx).unwrap_or(&LuaType::Nil).clone();
    }

    let dynamic_expr_key_member = is_dynamic_expr_key_member_assignment(analyzer, &type_owner);
    if !dynamic_expr_key_member {
        if let Some(widened_type) =
            get_widened_member_assignment_collection_type(analyzer, &type_owner, &expr_type)
        {
            expr_type = widened_type;
        }

        match get_cached_widened_member_assignment_type(
            analyzer,
            &type_owner,
            &expr_type,
            preserve_table_literals,
        ) {
            Some(Some(widened_type)) => {
                expr_type = widened_type;
            }
            Some(None) => {}
            None => {
                if let Some(widened_type) = get_widened_member_assignment_type(
                    analyzer,
                    &type_owner,
                    &expr_type,
                    preserve_table_literals,
                ) {
                    expr_type = widened_type;
                }
            }
        }
    }

    if is_global_decl_owner(analyzer, &type_owner) {
        expr_type = merge_open_table_types(analyzer.db, vec![expr_type]);
    }

    bind_type(
        analyzer.db,
        type_owner.clone(),
        LuaTypeCache::InferType(expr_type.clone()),
    );

    if let LuaTypeOwner::Member(member_id) = &type_owner
        && is_assignment_file_define_member(analyzer.db, *member_id)
    {
        let guarded_table_assignment =
            preserve_table_literals || is_guarded_table_assignment_member(analyzer.db, *member_id);
        let conditional_branch_assignment =
            is_member_assignment_in_conditional_branch(analyzer, *member_id);
        if guarded_table_assignment {
            let already_preserved = analyzer
                .db
                .get_member_index()
                .is_non_overwriting_assignment_member(*member_id);
            if !already_preserved {
                analyzer
                    .db
                    .get_member_index_mut()
                    .mark_non_overwriting_assignment_member(*member_id);
                preserve_guarded_table_assignment_members(analyzer, *member_id);
            }
        } else if conditional_branch_assignment {
            analyzer
                .db
                .get_member_index_mut()
                .mark_non_overwriting_assignment_member(*member_id);
        } else if !dynamic_expr_key_member
            && analyzer
                .db
                .get_member_index()
                .member_function_scope_range(*member_id)
                .is_none()
        {
            analyzer
                .db
                .get_member_index_mut()
                .retain_only_member_for_owner_key(*member_id);
        }
    }

    if !dynamic_expr_key_member {
        record_member_assignment_widening_cache(
            analyzer,
            &type_owner,
            &expr_type,
            preserve_table_literals,
        );
        record_member_collection_assignment_widening_cache(analyzer, &type_owner, &expr_type);
    }

    Some(())
}

fn member_assignment_or_source_type(
    analyzer: &mut LuaAnalyzer,
    type_owner: &LuaTypeOwner,
    expr: &LuaExpr,
    fallback_type: LuaType,
) -> LuaType {
    if !matches!(type_owner, LuaTypeOwner::Member(_)) {
        return fallback_type;
    }

    let Some(arms) = top_level_or_expr_arms(expr) else {
        return fallback_type;
    };

    let Some((last_arm, non_final_arms)) = arms.split_last() else {
        return fallback_type;
    };

    let mut source_type = None;
    for arm in non_final_arms {
        let Ok(arm_type) = analyzer.infer_expr(arm) else {
            return fallback_type;
        };
        if arm_type.is_unknown() || arm_type.is_any() {
            return fallback_type;
        }
        let arm_type = remove_false_or_nil(arm_type);
        source_type = Some(match source_type {
            Some(current) => TypeOps::Union.apply(analyzer.db, &current, &arm_type),
            None => arm_type,
        });
    }

    let Ok(last_type) = analyzer.infer_expr(last_arm) else {
        return fallback_type;
    };
    if last_type.is_unknown() || last_type.is_any() {
        return fallback_type;
    }

    match source_type {
        Some(current) => TypeOps::Union.apply(analyzer.db, &current, &last_type),
        None => fallback_type,
    }
}

fn top_level_or_expr_arms(expr: &LuaExpr) -> Option<Vec<LuaExpr>> {
    let LuaExpr::BinaryExpr(binary_expr) = expr else {
        return None;
    };
    if binary_expr.get_op_token()?.get_op() != BinaryOperator::OpOr {
        return None;
    }

    let mut arms = Vec::new();
    collect_or_expr_arms(expr, &mut arms)?;
    (arms.len() >= 2).then_some(arms)
}

fn collect_or_expr_arms(expr: &LuaExpr, arms: &mut Vec<LuaExpr>) -> Option<()> {
    if let LuaExpr::BinaryExpr(binary_expr) = expr
        && binary_expr.get_op_token()?.get_op() == BinaryOperator::OpOr
    {
        let (left, right) = binary_expr.get_exprs()?;
        collect_or_expr_arms(&left, arms)?;
        collect_or_expr_arms(&right, arms)?;
        return Some(());
    }

    arms.push(expr.clone());
    Some(())
}

fn is_dynamic_expr_key_member_assignment(
    analyzer: &LuaAnalyzer,
    type_owner: &LuaTypeOwner,
) -> bool {
    let LuaTypeOwner::Member(member_id) = type_owner else {
        return false;
    };
    analyzer
        .db
        .get_member_index()
        .get_member(member_id)
        .is_some_and(|member| member.get_key().is_expr())
}

fn is_global_decl_owner(analyzer: &LuaAnalyzer, type_owner: &LuaTypeOwner) -> bool {
    let LuaTypeOwner::Decl(decl_id) = type_owner else {
        return false;
    };

    analyzer
        .db
        .get_decl_index()
        .get_decl(decl_id)
        .is_some_and(|decl| decl.is_global())
}

fn get_cached_widened_member_assignment_type(
    analyzer: &mut LuaAnalyzer,
    type_owner: &LuaTypeOwner,
    incoming_type: &LuaType,
    _preserve_table_literals: bool,
) -> Option<Option<LuaType>> {
    let LuaTypeOwner::Member(member_id) = type_owner else {
        return None;
    };
    if !is_assignment_file_define_member(analyzer.db, *member_id) {
        return None;
    }

    let member_index = analyzer.db.get_member_index();
    let owner = member_index.get_member_owner(member_id)?.clone();
    let key = member_index.get_member(member_id)?.get_key().clone();
    let visible_count = member_index.visible_member_count_for_owner_key(&owner, &key);
    let cache_key = MemberAssignmentWideningCacheKey { owner, key };

    let cache = match lookup_widening_cache(
        &analyzer.member_assignment_widening_cache,
        &cache_key,
        visible_count,
    ) {
        WideningCacheLookup::FirstSighting => return Some(None),
        WideningCacheLookup::Fallback => return None,
        WideningCacheLookup::Hit(cache) => cache,
    };

    let current_state_mask = member_assignment_state_mask(analyzer, *member_id);
    let compatible_states = cache
        .by_state_mask
        .iter()
        .filter(|(state_mask, _)| {
            member_assignment_state_masks_compatible(analyzer, current_state_mask, **state_mask)
        })
        .map(|(_, state)| state.clone())
        .collect::<Vec<_>>();
    if compatible_states.is_empty() {
        return Some(None);
    }

    match decide_member_assignment_widening(
        analyzer.db,
        incoming_type,
        true,
        compatible_states.iter(),
    ) {
        MemberAssignmentWideningDecision::Widened(widened_type) => Some(Some(widened_type)),
        MemberAssignmentWideningDecision::ClassBootstrapRejected => None,
        MemberAssignmentWideningDecision::NoPreviousAssignments => Some(None),
    }
}

fn record_member_assignment_widening_cache(
    analyzer: &mut LuaAnalyzer,
    type_owner: &LuaTypeOwner,
    assigned_type: &LuaType,
    _preserve_table_literals: bool,
) {
    let LuaTypeOwner::Member(member_id) = type_owner else {
        return;
    };
    if !is_assignment_file_define_member(analyzer.db, *member_id) {
        return;
    }

    let member_index = analyzer.db.get_member_index();
    let Some(owner) = member_index.get_member_owner(member_id).cloned() else {
        return;
    };
    let Some(key) = member_index
        .get_member(member_id)
        .map(|member| member.get_key().clone())
    else {
        return;
    };
    let visible_count = member_index.visible_member_count_for_owner_key(&owner, &key);
    let state_mask = member_assignment_state_mask(analyzer, *member_id);
    let cache_key = MemberAssignmentWideningCacheKey { owner, key };
    let doc_type = analyzer
        .db
        .get_type_index()
        .get_type_cache(&(*member_id).into())
        .filter(|cache| cache.is_doc())
        .map(|cache| cache.as_type().clone());
    let new_state = MemberAssignmentWideningState::from_assigned_type(assigned_type, doc_type);
    let db = &*analyzer.db;
    record_widening_cache(
        &mut analyzer.member_assignment_widening_cache,
        cache_key,
        visible_count,
        state_mask,
        new_state,
        |state, new_state| {
            merge_member_assignment_widening_state(db, state, new_state, assigned_type);
        },
    );
}

fn preserve_guarded_table_assignment_members(analyzer: &mut LuaAnalyzer, member_id: LuaMemberId) {
    let Some(member_ids) = guarded_table_assignment_member_ids_for_owner_key(analyzer, member_id)
    else {
        return;
    };

    analyzer
        .db
        .get_member_index_mut()
        .preserve_members_for_owner_key(member_id, member_ids);
}

/// Returns true when the assignment that introduced this member sits inside a
/// branching construct (if / while / repeat / for). In those cases we must not
/// collapse to a single "latest write" member, because the assignments in
/// sibling branches (or earlier loop iterations) are not dominated by this one
/// and their types must remain available so reads can union them.
///
/// Without this guard, a pattern like
///
/// ```lua
/// if cond then
///     obj.field = Vector(...)
/// else
///     obj.field = nil
/// end
/// ```
///
/// would silently drop the `Vector` branch and hover `obj.field` as just `nil`.
fn is_member_assignment_in_conditional_branch(
    analyzer: &LuaAnalyzer,
    member_id: LuaMemberId,
) -> bool {
    let Some(tree) = analyzer.db.get_vfs().get_syntax_tree(&member_id.file_id) else {
        return false;
    };
    let root = tree.get_red_root();
    let Some(node) = member_id.get_syntax_id().to_node_from_root(&root) else {
        return false;
    };

    node.ancestors().any(|ancestor| {
        matches!(
            ancestor.kind().into(),
            LuaSyntaxKind::IfStat
                | LuaSyntaxKind::ElseIfClauseStat
                | LuaSyntaxKind::ElseClauseStat
                | LuaSyntaxKind::WhileStat
                | LuaSyntaxKind::RepeatStat
                | LuaSyntaxKind::ForStat
                | LuaSyntaxKind::ForRangeStat
        )
    })
}

fn guarded_table_assignment_member_ids_for_owner_key(
    analyzer: &LuaAnalyzer,
    member_id: LuaMemberId,
) -> Option<Vec<LuaMemberId>> {
    let member_index = analyzer.db.get_member_index();
    let owner = member_index.get_member_owner(&member_id)?.clone();
    let key = member_index.get_member(&member_id)?.get_key().clone();
    let mut member_ids = Vec::new();

    for related_member in member_index.get_current_owner_members_for_key(&owner, &key) {
        let related_member_id = related_member.get_id();
        if !is_guarded_table_assignment_member(analyzer.db, related_member_id) {
            return None;
        }

        member_ids.push(related_member_id);
    }

    (member_ids.len() >= 2).then_some(member_ids)
}

fn get_widened_member_assignment_type(
    analyzer: &mut LuaAnalyzer,
    type_owner: &LuaTypeOwner,
    incoming_type: &LuaType,
    preserve_table_literals: bool,
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
    let related_members = if preserve_table_literals {
        let related_member_ids =
            guarded_table_assignment_member_ids_for_owner_key(analyzer, *member_id)?;
        related_member_ids
            .into_iter()
            .filter_map(|related_member_id| member_index.get_member(&related_member_id))
            .collect()
    } else {
        member_index.get_members_for_owner_key(&owner, &key)
    };
    if related_members.len() < 2 {
        return None;
    }

    let mut previous_states = Vec::new();
    let mut saw_previous_assignment = false;

    for related_member in related_members {
        let related_member_id = related_member.get_id();
        if related_member_id == *member_id {
            continue;
        }
        if !is_member_realm_compatible(analyzer, *member_id, related_member_id) {
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

        previous_states.push(MemberAssignmentWideningState::from_type_cache(
            &existing_cache,
        ));
    }

    if !saw_previous_assignment {
        return None;
    }

    let widened_type = match decide_member_assignment_widening(
        analyzer.db,
        incoming_type,
        !preserve_table_literals,
        previous_states.iter(),
    ) {
        MemberAssignmentWideningDecision::Widened(widened_type) => widened_type,
        MemberAssignmentWideningDecision::ClassBootstrapRejected => {
            union_member_assignment_widening(
                analyzer.db,
                incoming_type,
                !preserve_table_literals,
                previous_states.iter(),
            )
        }
        MemberAssignmentWideningDecision::NoPreviousAssignments => {
            widen_related_assignment_type(incoming_type, false)
        }
    };

    Some(if preserve_table_literals {
        crate::prune_redundant_guarded_table_bootstrap_type(analyzer.db, widened_type)
    } else {
        widened_type
    })
}

pub(super) fn flush_pending_dynamic_key_collection_widenings(analyzer: &mut LuaAnalyzer) {
    let pending = std::mem::take(&mut analyzer.pending_dynamic_key_collection_widenings);
    let mut pending_by_owner: FxHashMap<LuaMemberOwner, Vec<(LuaMemberKey, LuaType)>> =
        FxHashMap::default();
    for (cache_key, additional_base) in pending {
        if !cache_key.key.is_expr() {
            continue;
        }

        pending_by_owner
            .entry(cache_key.owner)
            .or_default()
            .push((cache_key.key, additional_base));
    }

    for (owner, pending_items) in pending_by_owner {
        flush_pending_dynamic_key_collection_widening_for_members(analyzer, owner, pending_items);
    }
}

pub(super) fn is_assignment_file_define_member(
    db: &crate::DbIndex,
    member_id: LuaMemberId,
) -> bool {
    db.get_member_index()
        .get_member(&member_id)
        .is_some_and(|member| {
            member.get_feature() == LuaMemberFeature::FileDefine
                && member.get_syntax_id().get_kind() == glua_parser::LuaSyntaxKind::IndexExpr
        })
}

fn is_guarded_table_assignment_member(db: &crate::DbIndex, member_id: LuaMemberId) -> bool {
    let Some(tree) = db.get_vfs().get_syntax_tree(&member_id.file_id) else {
        return false;
    };
    let root = tree.get_red_root();
    let Some(node) = member_id.get_syntax_id().to_node_from_root(&root) else {
        return false;
    };
    let Some(index_expr) = LuaIndexExpr::cast(node) else {
        return false;
    };

    is_guarded_table_assignment_index_expr(&index_expr)
}

fn is_guarded_table_assignment_index_expr(index_expr: &LuaIndexExpr) -> bool {
    let Some(var) = LuaVarExpr::cast(index_expr.syntax().clone()) else {
        return false;
    };
    let Some(access_path) = var.get_access_path() else {
        return false;
    };
    let Some(assign_stat) = index_expr.get_parent::<LuaAssignStat>() else {
        return false;
    };
    let syntax_id = index_expr.get_syntax_id();
    let (var_list, expr_list) = assign_stat.get_var_and_expr_list();

    var_list
        .iter()
        .zip(expr_list.iter())
        .any(|(candidate_var, expr)| {
            candidate_var.get_syntax_id() == syntax_id
                && guarded_assignment_expr_matches_path(expr, &access_path)
        })
}

fn guarded_assignment_expr_matches_path(expr: &LuaExpr, access_path: &str) -> bool {
    let LuaExpr::BinaryExpr(binary_expr) = expr else {
        return false;
    };
    if binary_expr.get_op_token().map(|op| op.get_op()) != Some(BinaryOperator::OpOr) {
        return false;
    }

    let Some((left, right)) = binary_expr.get_exprs() else {
        return false;
    };
    if !matches!(right, LuaExpr::TableExpr(_)) {
        return false;
    }

    LuaVarExpr::cast(left.syntax().clone())
        .and_then(|left_var| left_var.get_access_path())
        .is_some_and(|left_path| left_path == access_path)
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
    let signature_type =
        LuaType::Signature(LuaSignatureId::from_closure(analyzer.file_id, &closure));
    let type_owner = get_var_owner(analyzer, func_name.clone());
    set_index_expr_owner(analyzer, func_name.clone());
    write_type_cache(
        analyzer.db,
        type_owner,
        LuaTypeCache::InferType(signature_type.clone()),
        TypeCacheWriteMode::InsertOnly,
    );

    Some(())
}

pub fn analyze_local_func_stat(
    analyzer: &mut LuaAnalyzer,
    local_func_stat: LuaLocalFuncStat,
) -> Option<()> {
    let closure = local_func_stat.get_closure()?;
    let func_name = local_func_stat.get_local_name()?;
    let signature_type =
        LuaType::Signature(LuaSignatureId::from_closure(analyzer.file_id, &closure));
    let position = func_name.get_position();
    let decl_id = LuaDeclId::new(analyzer.file_id, position);
    write_type_cache(
        analyzer.db,
        decl_id.into(),
        LuaTypeCache::InferType(signature_type.clone()),
        TypeCacheWriteMode::InsertOnly,
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

/// Whether this value-field (positional `{ expr }`) belongs to a shaped
/// sequential table literal whose integer members were registered in the
/// declaration pass (see `analyze_table_expr`). Such members need their value
/// types inferred and bound here, exactly like keyed/assign fields, otherwise
/// the registered `[n]` member has no type cache and dynamic indexing degrades.
fn is_shaped_array_value_field(field: &LuaTableField) -> bool {
    field.is_value_field()
        && field
            .get_parent::<LuaTableExpr>()
            .is_some_and(|table_expr| table_expr.is_shaped_array_literal())
}

pub fn analyze_table_field(analyzer: &mut LuaAnalyzer, field: LuaTableField) -> Option<()> {
    register_expr_key_member(analyzer, &field);

    if field.is_assign_field() || is_shaped_array_value_field(&field) {
        let value_expr = field.get_value_expr()?;
        let member_id = LuaMemberId::new(field.get_syntax_id(), analyzer.file_id);
        let value_type = match analyzer.infer_expr(&value_expr.clone()) {
            Ok(value_type) => match value_type {
                LuaType::Def(ref_id) => LuaType::Ref(ref_id),
                other => {
                    if other.is_unknown() && is_undefined_global_name_expr(analyzer, &value_expr) {
                        LuaType::Nil
                    } else {
                        other
                    }
                }
            },
            // Same rationale as `analyze_assign_stat`: a missing/undefined
            // RHS evaluates to `nil` at runtime.
            Err(InferFailReason::None) => LuaType::Nil,
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

/// Extract a string literal value from an expression, if it is a literal string.
fn extract_string_literal_from_expr(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::LiteralExpr(literal_expr) => match literal_expr.get_literal()? {
            LuaLiteralToken::String(string_token) => Some(string_token.get_value().to_string()),
            _ => None,
        },
        _ => None,
    }
}

fn special_assign_pattern(
    analyzer: &mut LuaAnalyzer,
    type_owner: LuaTypeOwner,
    var: LuaVarExpr,
    expr: LuaExpr,
    assign_stat_range: rowan::TextRange,
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

    let guarded_table_expr = matches!(&right, LuaExpr::TableExpr(_));
    let expr_type = if guarded_table_expr {
        infer_guarded_table_assignment_type(
            analyzer,
            &LuaExpr::BinaryExpr(binary_expr),
            &left,
            &right,
        )
    } else {
        set_index_expr_owner(analyzer, var.clone());
        analyzer.infer_expr(&right)
    };

    match expr_type {
        Ok(expr_type) => {
            if guarded_table_expr {
                set_guarded_table_assignment_index_owner(analyzer, var, &left);
            }

            // Register inferred string default for `x = x or "literal"`.
            // This is a SIBLING branch to the table-guard path: only fires
            // when the RHS is NOT a TableExpr and IS a string literal,
            // and the type_owner is a plain Decl. Completely disjoint from
            // the table-guard path.
            if !guarded_table_expr {
                if let LuaTypeOwner::Decl(decl_id) = &type_owner {
                    if let Some(string_value) = extract_string_literal_from_expr(&right) {
                        analyzer
                            .db
                            .get_property_index_mut()
                            .add_inferred_string_default(
                                analyzer.file_id,
                                *decl_id,
                                smol_str::SmolStr::new(string_value),
                                assign_stat_range,
                            );
                    }
                }
            }

            assign_merge_type_owner_and_expr_type(
                analyzer,
                type_owner,
                &expr_type,
                0,
                guarded_table_expr,
            );
        }
        Err(_) => return None,
    }

    Some(())
}

fn set_guarded_table_assignment_index_owner(
    analyzer: &mut LuaAnalyzer,
    var: LuaVarExpr,
    left: &LuaExpr,
) -> Option<()> {
    match var {
        LuaVarExpr::IndexExpr(index_expr) => {
            if let Some(cache_key) = guarded_table_assignment_cache_key(analyzer, left) {
                apply_index_expr_member_owner_with_guarded(
                    analyzer,
                    index_expr,
                    cache_key.owner,
                    false,
                    true,
                )
            } else {
                set_index_expr_owner(analyzer, LuaVarExpr::IndexExpr(index_expr))
            }
        }
        other => set_index_expr_owner(analyzer, other),
    }
}

fn infer_guarded_table_assignment_type(
    analyzer: &mut LuaAnalyzer,
    binary_expr: &LuaExpr,
    left: &LuaExpr,
    right: &LuaExpr,
) -> Result<LuaType, InferFailReason> {
    let right_type = analyzer.infer_expr(right)?;
    let cache_key = guarded_table_assignment_cache_key(analyzer, left);
    if let Some(cached_left_type) = cache_key
        .as_ref()
        .and_then(|key| analyzer.guarded_table_assignment_type_cache.get(key))
        .cloned()
    {
        if matches!(cached_left_type, LuaType::Table) && is_empty_table_expr(right) {
            return Ok(LuaType::Table);
        }
        return merge_guarded_table_assignment_type(
            analyzer,
            binary_expr,
            left,
            cached_left_type,
            right_type,
        );
    }

    let left_type = match analyzer.infer_expr(left) {
        Ok(left_type) => left_type,
        Err(reason) if reason.is_need_resolve() => LuaType::Nil,
        Err(reason) => return Err(reason),
    };

    let result =
        merge_guarded_table_assignment_type(analyzer, binary_expr, left, left_type, right_type)?;
    if let Some(cache_key) = cache_key
        && (result.is_any() || result.is_table())
    {
        analyzer
            .guarded_table_assignment_type_cache
            .insert(cache_key, widen_related_assignment_type(&result, true));
    }

    Ok(result)
}

fn is_empty_table_expr(expr: &LuaExpr) -> bool {
    matches!(expr, LuaExpr::TableExpr(table_expr) if table_expr.get_fields().next().is_none())
}

fn merge_guarded_table_assignment_type(
    analyzer: &mut LuaAnalyzer,
    binary_expr: &LuaExpr,
    left: &LuaExpr,
    left_type: LuaType,
    right_type: LuaType,
) -> Result<LuaType, InferFailReason> {
    let left_type = remove_false_or_nil(left_type);
    if left_type.is_nil() || left_type.is_unknown() || left_type.is_never() {
        return Ok(right_type);
    }
    if should_prefer_guarded_dynamic_index_rhs(analyzer, left, &left_type) {
        return Ok(right_type);
    }
    if !(left_type.is_any() || left_type.is_table()) {
        return analyzer.infer_expr(binary_expr);
    }

    Ok(TypeOps::Union.apply(analyzer.db, &left_type, &right_type))
}

fn guarded_table_assignment_cache_key(
    analyzer: &mut LuaAnalyzer,
    left: &LuaExpr,
) -> Option<MemberAssignmentWideningCacheKey> {
    let left_var = LuaVarExpr::cast(left.syntax().clone())?;
    let LuaVarExpr::IndexExpr(index_expr) = left_var else {
        return None;
    };
    let prefix_expr = index_expr.get_prefix_expr()?;
    let owner = direct_local_table_prefix_member_owner(analyzer, &prefix_expr).or_else(|| {
        let prefix_type = analyzer.infer_expr(&prefix_expr).ok()?;
        resolve_index_expr_member_owner_for_file(&prefix_type, Some(analyzer.file_id))
            .map(|(owner, _)| owner)
    })?;
    let index_key = index_expr.get_index_key()?;
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let key = LuaMemberKey::from_index_key_or_unknown(analyzer.db, cache, &index_key).ok()?;
    (!key.is_expr()).then_some(MemberAssignmentWideningCacheKey { owner, key })
}

fn should_prefer_guarded_dynamic_index_rhs(
    analyzer: &LuaAnalyzer,
    left: &LuaExpr,
    left_type: &LuaType,
) -> bool {
    analyzer.gmod_enabled
        && analyzer.db.get_emmyrc().gmod.infer_dynamic_fields
        && left_type.is_any()
        && matches!(left, LuaExpr::IndexExpr(index_expr) if matches!(index_expr.get_index_key(), Some(LuaIndexKey::Expr(_))))
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

pub(super) fn is_local_mutable(analyzer: &LuaAnalyzer, decl_id: LuaDeclId) -> bool {
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

/// Returns `true` when `expr` is a bare `NameExpr` that resolves to neither a
/// local declaration nor a registered global. Such reads evaluate to `nil` at
/// runtime, but `infer_expr` reports them as `Unknown` (see
/// `semantic/infer/mod.rs` where `InferFailReason::None` is collapsed to
/// `Ok(LuaType::Unknown)`). Callers use this to substitute `Nil` when binding
/// the LHS of a local/assign/table-field declaration so hover and downstream
/// inference reflect the runtime value.
fn is_undefined_global_name_expr(analyzer: &LuaAnalyzer, expr: &LuaExpr) -> bool {
    let LuaExpr::NameExpr(name_expr) = expr else {
        return false;
    };
    let Some(name) = name_expr.get_name_text() else {
        return false;
    };
    if name == "self" {
        return false;
    }
    let position = name_expr.get_position();
    let has_local = analyzer
        .db
        .get_decl_index()
        .get_decl_tree(&analyzer.file_id)
        .and_then(|tree| tree.find_local_decl(&name, position))
        .is_some();
    if has_local {
        return false;
    }
    // Workspace-scoped lookup matches the diagnostic's own visibility check
    // (see `diagnostic/checker/undefined_global.rs`). With multi-workspace
    // isolation enabled, a global declared in another root must not "rescue"
    // an undefined read in the current root.
    let module_index = analyzer.db.get_module_index();
    let global_index = analyzer.db.get_global_index();
    let has_global = if let Some(ws_id) = module_index.get_workspace_id(analyzer.file_id) {
        global_index.is_exist_global_decl_in_workspace(&name, module_index, ws_id)
    } else {
        global_index.is_exist_global_decl(&name)
    };
    !has_global
}

#[cfg(test)]
mod tests {
    use glua_parser::LuaSyntaxId;
    use rowan::{TextRange, TextSize};

    use crate::{DbIndex, FileId, InFiled, LuaMergedTableType, LuaTypeDeclId, LuaUnionType};

    use super::*;

    fn table_const(start: u32, end: u32) -> LuaType {
        LuaType::TableConst(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(start), TextSize::new(end)),
        ))
    }

    fn member_id_at(start: u32) -> LuaMemberId {
        member_id_at_file(FileId::new(0), start)
    }

    fn member_id_at_file(file_id: FileId, start: u32) -> LuaMemberId {
        let range = TextRange::new(TextSize::new(start), TextSize::new(start + 1));
        LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range),
            file_id,
        )
    }

    fn add_typed_file_define_member(
        db: &mut DbIndex,
        owner: LuaMemberOwner,
        member_id: LuaMemberId,
        key: LuaMemberKey,
        typ: LuaType,
    ) {
        db.get_member_index_mut().add_member(
            owner,
            LuaMember::new(member_id, key, LuaMemberFeature::FileDefine, None),
        );
        db.get_type_index_mut().bind_type(
            LuaTypeOwner::Member(member_id),
            LuaTypeCache::InferType(typ),
        );
    }

    fn with_analyzer<T>(db: &mut DbIndex, run: impl FnOnce(&mut LuaAnalyzer<'_>) -> T) -> T {
        with_analyzer_config(db, false, run)
    }

    fn with_analyzer_config<T>(
        db: &mut DbIndex,
        gmod_enabled: bool,
        run: impl FnOnce(&mut LuaAnalyzer<'_>) -> T,
    ) -> T {
        let mut context = crate::compilation::analyzer::AnalyzeContext::new();
        let matcher = super::super::call::SpecialCallDirectMatcher::default();
        let mut analyzer = LuaAnalyzer::new(
            db,
            FileId::new(0),
            &mut context,
            gmod_enabled,
            false,
            &matcher,
        );
        run(&mut analyzer)
    }

    #[test]
    fn duplicate_table_owner_is_not_ambiguous() {
        let table = table_const(1, 2);
        let typ = LuaMergedTableType::new(vec![table.clone(), table]).into();

        assert!(!has_multiple_distinct_index_expr_member_owners(&typ));
    }

    #[test]
    fn distinct_table_owners_are_ambiguous() {
        let typ = LuaType::Union(
            LuaUnionType::from_vec(vec![table_const(1, 2), table_const(3, 4)]).into(),
        );

        assert!(has_multiple_distinct_index_expr_member_owners(&typ));
    }

    #[test]
    fn member_assignment_widening_uses_cache_for_sequential_same_key_members() {
        let mut db = DbIndex::new();
        let owner = LuaMemberOwner::Element(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(10), TextSize::new(11)),
        ));
        let key = LuaMemberKey::from("field");
        let first_member = member_id_at(1);
        let second_member = member_id_at(3);
        add_typed_file_define_member(
            &mut db,
            owner.clone(),
            first_member,
            key.clone(),
            LuaType::Integer,
        );

        with_analyzer(&mut db, |analyzer| {
            record_member_assignment_widening_cache(
                analyzer,
                &LuaTypeOwner::Member(first_member),
                &LuaType::Integer,
                false,
            );
            add_typed_file_define_member(analyzer.db, owner, second_member, key, LuaType::String);

            let widened = get_cached_widened_member_assignment_type(
                analyzer,
                &LuaTypeOwner::Member(second_member),
                &LuaType::String,
                false,
            )
            .expect("sequential owner/key cache should be usable")
            .expect("second same-key assignment should widen with cached prior type");

            assert_eq!(
                widened,
                TypeOps::Union.apply(analyzer.db, &LuaType::Integer, &LuaType::String)
            );
        });
    }

    #[test]
    fn member_assignment_widening_cache_tracks_many_same_key_members() {
        let mut db = DbIndex::new();
        let owner = LuaMemberOwner::Element(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(10), TextSize::new(11)),
        ));
        let key = LuaMemberKey::from("field");

        with_analyzer(&mut db, |analyzer| {
            let mut cache_hits = 0;
            for i in 0..512 {
                let member_id = member_id_at(i * 2 + 1);
                add_typed_file_define_member(
                    analyzer.db,
                    owner.clone(),
                    member_id,
                    key.clone(),
                    LuaType::String,
                );

                if i > 0 {
                    assert!(
                        get_cached_widened_member_assignment_type(
                            analyzer,
                            &LuaTypeOwner::Member(member_id),
                            &LuaType::String,
                            false,
                        )
                        .is_some(),
                        "cache should stay enabled at member {i}"
                    );
                    cache_hits += 1;
                }

                record_member_assignment_widening_cache(
                    analyzer,
                    &LuaTypeOwner::Member(member_id),
                    &LuaType::String,
                    false,
                );
            }

            assert_eq!(cache_hits, 511);
        });
    }

    #[test]
    fn member_assignment_widening_cache_tracks_many_preserved_table_literal_members() {
        let mut db = DbIndex::new();
        let owner = LuaMemberOwner::Element(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(10), TextSize::new(11)),
        ));
        let key = LuaMemberKey::from("field");

        with_analyzer(&mut db, |analyzer| {
            let mut cache_hits = 0;
            for i in 0..512 {
                let member_id = member_id_at(i * 2 + 1);
                let table_type = table_const(i * 2 + 1000, i * 2 + 1001);
                add_typed_file_define_member(
                    analyzer.db,
                    owner.clone(),
                    member_id,
                    key.clone(),
                    table_type.clone(),
                );

                if i > 0 {
                    let cached_type = get_cached_widened_member_assignment_type(
                        analyzer,
                        &LuaTypeOwner::Member(member_id),
                        &table_type,
                        true,
                    )
                    .expect("preserved table-literal cache should stay enabled")
                    .expect("preserved table-literal cache should return a widened type");
                    assert_eq!(cached_type, LuaType::Table, "unexpected type at member {i}");
                    cache_hits += 1;
                }

                record_member_assignment_widening_cache(
                    analyzer,
                    &LuaTypeOwner::Member(member_id),
                    &table_type,
                    true,
                );
            }

            assert_eq!(cache_hits, 511);
        });
    }

    #[test]
    fn member_assignment_widening_cache_tracks_many_same_class_bootstrap_members() {
        let mut db = DbIndex::new();
        let owner = LuaMemberOwner::Element(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(10), TextSize::new(11)),
        ));
        let key = LuaMemberKey::from("field");
        let class_type = LuaType::Def(LuaTypeDeclId::global("ClassType"));

        with_analyzer(&mut db, |analyzer| {
            let mut cache_hits = 0;
            for i in 0..512 {
                let member_id = member_id_at(i * 2 + 1);
                add_typed_file_define_member(
                    analyzer.db,
                    owner.clone(),
                    member_id,
                    key.clone(),
                    class_type.clone(),
                );

                if i > 0 {
                    let cached_type = get_cached_widened_member_assignment_type(
                        analyzer,
                        &LuaTypeOwner::Member(member_id),
                        &class_type,
                        false,
                    )
                    .expect("class bootstrap cache should stay enabled")
                    .expect("same class bootstrap should return cached class type");
                    assert_eq!(cached_type, class_type, "unexpected type at member {i}");
                    cache_hits += 1;
                }

                record_member_assignment_widening_cache(
                    analyzer,
                    &LuaTypeOwner::Member(member_id),
                    &class_type,
                    false,
                );
            }

            assert_eq!(cache_hits, 511);
        });
    }

    #[test]
    fn member_assignment_widening_cache_rejects_different_class_bootstrap_members() {
        let mut db = DbIndex::new();
        let owner = LuaMemberOwner::Element(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(10), TextSize::new(11)),
        ));
        let key = LuaMemberKey::from("field");
        let first_class = LuaType::Def(LuaTypeDeclId::global("FirstClass"));
        let second_class = LuaType::Def(LuaTypeDeclId::global("SecondClass"));
        let first_member = member_id_at(1);
        let second_member = member_id_at(3);

        add_typed_file_define_member(
            &mut db,
            owner.clone(),
            first_member,
            key.clone(),
            first_class.clone(),
        );

        with_analyzer(&mut db, |analyzer| {
            record_member_assignment_widening_cache(
                analyzer,
                &LuaTypeOwner::Member(first_member),
                &first_class,
                false,
            );
            add_typed_file_define_member(
                analyzer.db,
                owner,
                second_member,
                key,
                second_class.clone(),
            );

            assert!(
                get_cached_widened_member_assignment_type(
                    analyzer,
                    &LuaTypeOwner::Member(second_member),
                    &second_class,
                    false,
                )
                .is_none(),
                "different class bootstraps must fall back to the full compatibility scan"
            );
        });
    }

    #[test]
    fn member_assignment_widening_fallback_preserves_doc_authority() {
        let mut db = DbIndex::new();
        let owner = LuaMemberOwner::Element(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(10), TextSize::new(11)),
        ));
        let key = LuaMemberKey::from("field");
        let doc_type = LuaType::Def(LuaTypeDeclId::global("DocType"));
        let first_member = member_id_at(1);
        let second_member = member_id_at(3);
        let third_member = member_id_at(5);

        add_typed_file_define_member(
            &mut db,
            owner.clone(),
            first_member,
            key.clone(),
            LuaType::Integer,
        );
        db.get_type_index_mut().force_bind_type(
            LuaTypeOwner::Member(first_member),
            LuaTypeCache::DocType(doc_type.clone()),
        );

        with_analyzer(&mut db, |analyzer| {
            record_member_assignment_widening_cache(
                analyzer,
                &LuaTypeOwner::Member(first_member),
                &LuaType::Integer,
                false,
            );
            add_typed_file_define_member(
                analyzer.db,
                owner.clone(),
                second_member,
                key.clone(),
                LuaType::String,
            );
            add_typed_file_define_member(analyzer.db, owner, third_member, key, LuaType::Boolean);

            assert_eq!(
                get_cached_widened_member_assignment_type(
                    analyzer,
                    &LuaTypeOwner::Member(third_member),
                    &LuaType::Boolean,
                    false,
                ),
                None,
                "visible-count mismatch should force the fallback scan"
            );

            let widened = get_widened_member_assignment_type(
                analyzer,
                &LuaTypeOwner::Member(third_member),
                &LuaType::Boolean,
                false,
            )
            .expect("fallback scan should find prior same-key assignments");

            assert_eq!(widened, doc_type);
        });
    }

    #[test]
    fn member_assignment_widening_fallback_rejects_different_class_bootstrap() {
        let mut db = DbIndex::new();
        let owner = LuaMemberOwner::Element(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(10), TextSize::new(11)),
        ));
        let key = LuaMemberKey::from("field");
        let first_class = LuaType::Def(LuaTypeDeclId::global("FirstClass"));
        let second_class = LuaType::Def(LuaTypeDeclId::global("SecondClass"));
        let first_member = member_id_at(1);
        let second_member = member_id_at(3);
        let third_member = member_id_at(5);

        add_typed_file_define_member(
            &mut db,
            owner.clone(),
            first_member,
            key.clone(),
            first_class.clone(),
        );

        with_analyzer(&mut db, |analyzer| {
            record_member_assignment_widening_cache(
                analyzer,
                &LuaTypeOwner::Member(first_member),
                &first_class,
                false,
            );
            add_typed_file_define_member(
                analyzer.db,
                owner.clone(),
                second_member,
                key.clone(),
                second_class.clone(),
            );
            add_typed_file_define_member(
                analyzer.db,
                owner,
                third_member,
                key,
                second_class.clone(),
            );

            assert_eq!(
                get_cached_widened_member_assignment_type(
                    analyzer,
                    &LuaTypeOwner::Member(third_member),
                    &second_class,
                    false,
                ),
                None,
                "visible-count mismatch should force the fallback scan"
            );

            let widened = get_widened_member_assignment_type(
                analyzer,
                &LuaTypeOwner::Member(third_member),
                &second_class,
                false,
            )
            .expect("fallback scan should widen incompatible class assignments");
            let expected = TypeOps::Union.apply(analyzer.db, &first_class, &second_class);

            assert_eq!(widened, expected);
            assert_ne!(widened, second_class);
        });
    }

    #[test]
    fn member_assignment_widening_cache_and_fallback_match_plain_scalars() {
        let key = LuaMemberKey::from("field");
        let first_member = member_id_at(1);
        let second_member = member_id_at(3);
        let third_member = member_id_at(5);

        let mut cached_db = DbIndex::new();
        let cached_owner = LuaMemberOwner::Element(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(10), TextSize::new(11)),
        ));
        add_typed_file_define_member(
            &mut cached_db,
            cached_owner.clone(),
            first_member,
            key.clone(),
            LuaType::Integer,
        );
        let cached_widened = with_analyzer(&mut cached_db, |analyzer| {
            record_member_assignment_widening_cache(
                analyzer,
                &LuaTypeOwner::Member(first_member),
                &LuaType::Integer,
                false,
            );
            add_typed_file_define_member(
                analyzer.db,
                cached_owner,
                second_member,
                key.clone(),
                LuaType::String,
            );

            get_cached_widened_member_assignment_type(
                analyzer,
                &LuaTypeOwner::Member(second_member),
                &LuaType::String,
                false,
            )
            .expect("sequential same-key assignment should use cache")
            .expect("cached scalar assignment should widen")
        });

        let mut fallback_db = DbIndex::new();
        let fallback_owner = LuaMemberOwner::Element(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(10), TextSize::new(11)),
        ));
        add_typed_file_define_member(
            &mut fallback_db,
            fallback_owner.clone(),
            first_member,
            key.clone(),
            LuaType::Integer,
        );
        let fallback_widened = with_analyzer(&mut fallback_db, |analyzer| {
            record_member_assignment_widening_cache(
                analyzer,
                &LuaTypeOwner::Member(first_member),
                &LuaType::Integer,
                false,
            );
            add_typed_file_define_member(
                analyzer.db,
                fallback_owner.clone(),
                second_member,
                key.clone(),
                LuaType::String,
            );
            add_typed_file_define_member(
                analyzer.db,
                fallback_owner,
                third_member,
                key,
                LuaType::String,
            );

            assert_eq!(
                get_cached_widened_member_assignment_type(
                    analyzer,
                    &LuaTypeOwner::Member(third_member),
                    &LuaType::String,
                    false,
                ),
                None,
                "visible-count mismatch should force the fallback scan"
            );

            get_widened_member_assignment_type(
                analyzer,
                &LuaTypeOwner::Member(third_member),
                &LuaType::String,
                false,
            )
            .expect("fallback scalar assignment should widen")
        });

        assert_eq!(cached_widened, fallback_widened);
    }

    #[test]
    fn member_collection_assignment_widening_uses_cache_for_sequential_same_key_members() {
        let mut db = DbIndex::new();
        let owner = LuaMemberOwner::Element(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(10), TextSize::new(11)),
        ));
        let key = LuaMemberKey::from("items");
        let first_member = member_id_at(1);
        let second_member = member_id_at(3);
        let first_array = LuaType::Array(LuaArrayType::from_base_type(LuaType::Integer).into());
        let second_array = LuaType::Array(LuaArrayType::from_base_type(LuaType::String).into());
        add_typed_file_define_member(
            &mut db,
            owner.clone(),
            first_member,
            key.clone(),
            first_array.clone(),
        );

        with_analyzer(&mut db, |analyzer| {
            record_member_collection_assignment_widening_cache(
                analyzer,
                &LuaTypeOwner::Member(first_member),
                &first_array,
            );
            add_typed_file_define_member(
                analyzer.db,
                owner,
                second_member,
                key,
                second_array.clone(),
            );
            let (owner, key) = {
                let member_index = analyzer.db.get_member_index();
                (
                    member_index
                        .get_member_owner(&second_member)
                        .expect("member owner")
                        .clone(),
                    member_index
                        .get_member(&second_member)
                        .expect("member")
                        .get_key()
                        .clone(),
                )
            };

            let widened = get_cached_widened_member_collection_assignment_type(
                analyzer,
                &owner,
                &key,
                second_member,
                &LuaType::String,
            )
            .expect("sequential collection cache should be usable")
            .expect("second same-key collection assignment should widen with cached prior type");

            assert_eq!(
                widened,
                LuaType::Array(
                    LuaArrayType::from_base_type(TypeOps::Union.apply(
                        analyzer.db,
                        &LuaType::String,
                        &LuaType::Integer,
                    ))
                    .into()
                )
            );
        });
    }

    #[test]
    fn member_collection_assignment_widening_cache_and_fallback_match_array_base_unions() {
        let key = LuaMemberKey::from("items");
        let first_member = member_id_at(1);
        let second_member = member_id_at(3);
        let third_member = member_id_at(5);
        let first_array = LuaType::Array(LuaArrayType::from_base_type(LuaType::Integer).into());
        let second_array = LuaType::Array(LuaArrayType::from_base_type(LuaType::String).into());

        let mut cached_db = DbIndex::new();
        let cached_owner = LuaMemberOwner::Element(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(10), TextSize::new(11)),
        ));
        add_typed_file_define_member(
            &mut cached_db,
            cached_owner.clone(),
            first_member,
            key.clone(),
            first_array.clone(),
        );
        let cached_widened = with_analyzer(&mut cached_db, |analyzer| {
            record_member_collection_assignment_widening_cache(
                analyzer,
                &LuaTypeOwner::Member(first_member),
                &first_array,
            );
            add_typed_file_define_member(
                analyzer.db,
                cached_owner.clone(),
                second_member,
                key.clone(),
                second_array.clone(),
            );

            get_cached_widened_member_collection_assignment_type(
                analyzer,
                &cached_owner,
                &key,
                second_member,
                &LuaType::String,
            )
            .expect("sequential collection assignment should use cache")
            .expect("cached collection assignment should widen array base")
        });

        let mut fallback_db = DbIndex::new();
        let fallback_owner = LuaMemberOwner::Element(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(10), TextSize::new(11)),
        ));
        add_typed_file_define_member(
            &mut fallback_db,
            fallback_owner.clone(),
            first_member,
            key.clone(),
            first_array.clone(),
        );
        let fallback_widened = with_analyzer(&mut fallback_db, |analyzer| {
            record_member_collection_assignment_widening_cache(
                analyzer,
                &LuaTypeOwner::Member(first_member),
                &first_array,
            );
            add_typed_file_define_member(
                analyzer.db,
                fallback_owner.clone(),
                second_member,
                key.clone(),
                second_array.clone(),
            );
            add_typed_file_define_member(
                analyzer.db,
                fallback_owner,
                third_member,
                key,
                second_array,
            );

            assert_eq!(
                get_cached_widened_member_collection_assignment_type(
                    analyzer,
                    &LuaMemberOwner::Element(InFiled::new(
                        FileId::new(0),
                        TextRange::new(TextSize::new(10), TextSize::new(11)),
                    )),
                    &LuaMemberKey::from("items"),
                    third_member,
                    &LuaType::String,
                ),
                None,
                "visible-count mismatch should force the fallback scan"
            );

            get_widened_member_assignment_collection_type(
                analyzer,
                &LuaTypeOwner::Member(third_member),
                &LuaType::Array(LuaArrayType::from_base_type(LuaType::String).into()),
            )
            .expect("fallback collection assignment should widen array base")
        });

        let expected = LuaType::Array(
            LuaArrayType::from_base_type(TypeOps::Union.apply(
                &fallback_db,
                &LuaType::Integer,
                &LuaType::String,
            ))
            .into(),
        );

        assert_eq!(cached_widened, fallback_widened);
        assert_eq!(fallback_widened, expected);
    }

    #[test]
    fn member_collection_assignment_widening_cache_preserves_first_collection_assignment() {
        let mut db = DbIndex::new();
        let owner = LuaMemberOwner::Element(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(10), TextSize::new(11)),
        ));
        let key = LuaMemberKey::from("items");
        let member_id = member_id_at(1);
        let array = LuaType::Array(LuaArrayType::from_base_type(LuaType::Integer).into());
        add_typed_file_define_member(
            &mut db,
            owner.clone(),
            member_id,
            key.clone(),
            array.clone(),
        );

        with_analyzer(&mut db, |analyzer| {
            let cached = get_cached_widened_member_collection_assignment_type(
                analyzer,
                &owner,
                &key,
                member_id,
                &LuaType::Integer,
            );

            assert_eq!(
                cached,
                Some(None),
                "first visible collection assignment should use the cache path without widening"
            );
        });
    }

    #[test]
    fn member_collection_assignment_widening_fallback_preserves_first_collection_assignment() {
        let mut db = DbIndex::new();
        let owner = LuaMemberOwner::Element(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(10), TextSize::new(11)),
        ));
        let key = LuaMemberKey::from("items");
        let member_id = member_id_at(1);
        let array = LuaType::Array(LuaArrayType::from_base_type(LuaType::Integer).into());
        add_typed_file_define_member(&mut db, owner, member_id, key, array.clone());

        with_analyzer(&mut db, |analyzer| {
            let widened = get_widened_member_assignment_collection_type(
                analyzer,
                &LuaTypeOwner::Member(member_id),
                &array,
            );

            assert_eq!(
                widened, None,
                "fallback scan without prior compatible collection state should not widen"
            );
        });
    }

    #[test]
    fn dynamic_key_collection_widening_flush_is_stable_across_record_order() {
        fn flushed_type_for(order: &[LuaType]) -> LuaType {
            let mut db = DbIndex::new();
            let owner = LuaMemberOwner::Element(InFiled::new(
                FileId::new(0),
                TextRange::new(TextSize::new(10), TextSize::new(11)),
            ));
            let member_id = member_id_at(1);
            let initial_array =
                LuaType::Array(LuaArrayType::from_base_type(LuaType::Integer).into());
            add_typed_file_define_member(
                &mut db,
                owner.clone(),
                member_id,
                LuaMemberKey::from("items"),
                initial_array,
            );

            with_analyzer(&mut db, |analyzer| {
                for additional_base in order {
                    record_pending_dynamic_key_collection_widening(
                        analyzer,
                        owner.clone(),
                        LuaMemberKey::ExprType(LuaType::String),
                        additional_base,
                    );
                }
                flush_pending_dynamic_key_collection_widenings(analyzer);

                analyzer
                    .db
                    .get_type_index()
                    .get_type_cache(&LuaTypeOwner::Member(member_id))
                    .expect("flushed member type")
                    .as_type()
                    .clone()
            })
        }

        let string_then_boolean = flushed_type_for(&[LuaType::String, LuaType::Boolean]);
        let boolean_then_string = flushed_type_for(&[LuaType::Boolean, LuaType::String]);

        assert_eq!(string_then_boolean, boolean_then_string);
    }

    #[test]
    fn member_collection_assignment_widening_cache_respects_load_state_masks() {
        let mut db = DbIndex::new();
        db.get_gmod_infer_index_mut()
            .set_all_realm_file_metadata(std::collections::HashMap::from([
                (
                    FileId::new(0),
                    crate::GmodRealmFileMetadata {
                        load_state_mask: GmodStateMask::CLIENT,
                        ..Default::default()
                    },
                ),
                (
                    FileId::new(1),
                    crate::GmodRealmFileMetadata {
                        load_state_mask: GmodStateMask::SERVER,
                        ..Default::default()
                    },
                ),
            ]));

        let owner = LuaMemberOwner::Element(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(10), TextSize::new(11)),
        ));
        let key = LuaMemberKey::from("items");
        let first_member = member_id_at_file(FileId::new(0), 1);
        let second_member = member_id_at_file(FileId::new(1), 3);
        let first_array = LuaType::Array(LuaArrayType::from_base_type(LuaType::Integer).into());
        let second_array = LuaType::Array(LuaArrayType::from_base_type(LuaType::String).into());
        add_typed_file_define_member(
            &mut db,
            owner.clone(),
            first_member,
            key.clone(),
            first_array.clone(),
        );

        with_analyzer_config(&mut db, true, |analyzer| {
            record_member_collection_assignment_widening_cache(
                analyzer,
                &LuaTypeOwner::Member(first_member),
                &first_array,
            );
            add_typed_file_define_member(
                analyzer.db,
                owner.clone(),
                second_member,
                key.clone(),
                second_array,
            );

            let cached = get_cached_widened_member_collection_assignment_type(
                analyzer,
                &owner,
                &key,
                second_member,
                &LuaType::String,
            );

            assert_eq!(
                cached,
                Some(None),
                "client-only and server-only collection assignments must not be widened together"
            );
        });
    }

    #[test]
    fn member_collection_assignment_widening_cache_tracks_many_same_key_members() {
        let mut db = DbIndex::new();
        let owner = LuaMemberOwner::Element(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(10), TextSize::new(11)),
        ));
        let key = LuaMemberKey::from("items");
        let array = LuaType::Array(LuaArrayType::from_base_type(LuaType::String).into());

        with_analyzer(&mut db, |analyzer| {
            let mut cache_hits = 0;
            for i in 0..512 {
                let member_id = member_id_at(i * 2 + 1);
                add_typed_file_define_member(
                    analyzer.db,
                    owner.clone(),
                    member_id,
                    key.clone(),
                    array.clone(),
                );

                if i > 0 {
                    assert!(
                        get_cached_widened_member_collection_assignment_type(
                            analyzer,
                            &owner,
                            &key,
                            member_id,
                            &LuaType::String,
                        )
                        .is_some(),
                        "collection cache should stay enabled at member {i}"
                    );
                    cache_hits += 1;
                }

                record_member_collection_assignment_widening_cache(
                    analyzer,
                    &LuaTypeOwner::Member(member_id),
                    &array,
                );
            }

            assert_eq!(cache_hits, 511);
        });
    }

    #[test]
    fn expr_key_members_are_detected_as_dynamic_assignments() {
        let mut db = DbIndex::new();
        let member_id = member_id_at(1);
        add_typed_file_define_member(
            &mut db,
            LuaMemberOwner::Element(InFiled::new(
                FileId::new(0),
                TextRange::new(TextSize::new(10), TextSize::new(11)),
            )),
            member_id,
            LuaMemberKey::ExprType(LuaType::String),
            LuaType::Table,
        );

        with_analyzer(&mut db, |analyzer| {
            assert!(is_dynamic_expr_key_member_assignment(
                analyzer,
                &LuaTypeOwner::Member(member_id)
            ));
        });
    }
}
