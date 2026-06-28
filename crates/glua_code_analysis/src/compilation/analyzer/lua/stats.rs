use std::collections::HashSet;

use crate::{
    CacheEntry, FileId, InFiled, InferFailReason, LuaArrayType, LuaMemberKey, LuaSemanticDeclId,
    LuaSignatureId, LuaTypeCache, LuaTypeOwner, LuaUnionType, TypeOps,
    compilation::{
        analyzer::{
            common::{add_member, bind_type},
            unresolve::{UnResolveDecl, UnResolveMember},
        },
        get_scripted_class_type_decl_id,
    },
    db_index::{LuaDeclId, LuaMember, LuaMemberFeature, LuaMemberId, LuaMemberOwner, LuaType},
    semantic::{member_key_matches_type, merge_open_table_types, remove_false_or_nil},
};
use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAstNode, LuaExpr, LuaFuncStat, LuaIndexExpr, LuaIndexKey,
    LuaLiteralToken, LuaLocalFuncStat, LuaLocalStat, LuaNameExpr, LuaSyntaxKind, LuaTableExpr,
    LuaTableField, LuaVarExpr, NumberResult, PathTrait, UnaryOperator,
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
        let decl_id = LuaDeclId::new(analyzer.file_id, position);
        if is_call_or_index_expr(&expr) {
            analyzer
                .context
                .request_uninformative_local_decl_reinfer(decl_id);
        }

        if let Some(reason) = should_defer_guarded_index_alias(analyzer, &expr) {
            analyzer.context.request_stabilization(analyzer.file_id);
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
                    analyzer.context.request_stabilization(analyzer.file_id);
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
                    analyzer
                        .db
                        .get_type_index_mut()
                        .bind_type(decl_id.into(), LuaTypeCache::InferType(LuaType::Nil));
                }
            }
            Err(reason) => {
                if matches!(reason, InferFailReason::FieldNotFound)
                    && should_defer_gmod_self_index(analyzer, &expr)
                {
                    analyzer.context.request_stabilization(analyzer.file_id);
                }

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
                                // Out of variadic values; per Lua semantics
                                // the missing values are `nil`.
                                analyzer.db.get_type_index_mut().bind_type(
                                    decl_id.into(),
                                    LuaTypeCache::InferType(LuaType::Nil),
                                );
                            }
                        }
                        return Some(());
                    } else {
                        // Single-return or non-variadic evaluated to a single value,
                        // so extra slots receive `any` (legacy convention) instead of unknown.
                        for i in expr_count..name_count {
                            let name = name_list.get(i)?;
                            let position = name.get_position();
                            let decl_id = LuaDeclId::new(analyzer.file_id, position);
                            bind_type(
                                analyzer.db,
                                decl_id.into(),
                                LuaTypeCache::InferType(LuaType::Any),
                            );
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

fn should_defer_gmod_self_index(analyzer: &LuaAnalyzer, expr: &LuaExpr) -> bool {
    if !analyzer.gmod_enabled || !analyzer.db.get_emmyrc().gmod.infer_dynamic_fields {
        return false;
    }

    let LuaExpr::IndexExpr(index_expr) = expr else {
        return false;
    };
    if analyzer.is_scripted_class_scope {
        nested_index_root_is_self(index_expr)
    } else {
        index_expr_prefix_is_self(index_expr)
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

fn nested_index_root_is_self(index_expr: &LuaIndexExpr) -> bool {
    let Some(mut prefix_expr) = index_expr.get_prefix_expr() else {
        return false;
    };
    let mut saw_nested_index = false;
    while let LuaExpr::IndexExpr(prefix_index) = prefix_expr {
        saw_nested_index = true;
        let Some(next_prefix) = prefix_index.get_prefix_expr() else {
            return false;
        };
        prefix_expr = next_prefix;
    }

    saw_nested_index
        && matches!(
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
    let decl_tree = analyzer
        .db
        .get_decl_index()
        .get_decl_tree(&analyzer.file_id)?;
    if name != "self" {
        if !name_expr_resolves_to_seeded_class_local(analyzer, name_expr) {
            return None;
        }

        return scoped_class_global_member_owner(analyzer, &name).map(|owner| (owner, false));
    }

    let self_decl = decl_tree.find_local_decl("self", name_expr.get_position())?;
    if !self_decl.is_implicit_self() {
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
    let class_global = class_name_expr.get_name_text()?;
    if !name_expr_resolves_to_seeded_class_local(analyzer, &class_name_expr) {
        return None;
    }

    scoped_class_global_member_owner(analyzer, &class_global).map(|owner| (owner, false))
}

fn name_expr_resolves_to_seeded_class_local(
    analyzer: &LuaAnalyzer,
    name_expr: &LuaNameExpr,
) -> bool {
    analyzer
        .db
        .get_reference_index()
        .get_local_reference(&analyzer.file_id)
        .and_then(|file_ref| file_ref.get_decl_id(&name_expr.get_range()))
        .and_then(|decl_id| analyzer.db.get_decl_index().get_decl(&decl_id))
        .is_some_and(|decl| decl.is_seeded_class_local())
}

fn scoped_class_global_member_owner(analyzer: &LuaAnalyzer, name: &str) -> Option<LuaMemberOwner> {
    if !analyzer.db.get_emmyrc().gmod.enabled {
        return None;
    }

    let info = analyzer
        .db
        .get_gmod_infer_index()
        .get_scoped_class_info(&analyzer.file_id)?;
    (info.global_name == name).then(|| {
        LuaMemberOwner::Type(get_scripted_class_type_decl_id(
            &info.global_name,
            &info.class_name,
        ))
    })
}

fn apply_index_expr_member_owner(
    analyzer: &mut LuaAnalyzer,
    index_expr: LuaIndexExpr,
    member_owner: LuaMemberOwner,
    set_owner_only: bool,
) -> Option<()> {
    let index_key = index_expr.get_index_key()?;
    let member_id = LuaMemberId::new(index_expr.get_syntax_id(), analyzer.file_id);
    let guarded_table_assignment = is_guarded_table_assignment_index_expr(&index_expr);

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
        if guarded_table_assignment {
            analyzer
                .db
                .get_member_index_mut()
                .mark_non_overwriting_assignment_member(member_id);
            preserve_guarded_table_assignment_members(analyzer, member_id);
        }
        return Some(());
    }

    if set_owner_only {
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
            analyzer
                .db
                .get_member_index_mut()
                .mark_non_overwriting_assignment_member(member_id);
            preserve_guarded_table_assignment_members(analyzer, member_id);
        }
        return Some(());
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
    if guarded_table_assignment {
        analyzer
            .db
            .get_member_index_mut()
            .mark_non_overwriting_assignment_member(member_id);
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

        set_index_expr_owner(analyzer, var.clone());

        let expr_type = match analyzer.infer_expr(expr) {
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
                if matches!(reason, InferFailReason::FieldNotFound)
                    && should_defer_gmod_self_index(analyzer, expr)
                {
                    analyzer.context.request_stabilization(analyzer.file_id);
                }
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

        widen_existing_member_collection_type(analyzer, &var, &expr_type);
        assign_merge_type_owner_and_expr_type(analyzer, type_owner, &expr_type, 0, false);
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

    let Some((owner, _)) = resolve_index_expr_member_owner(&prefix_type) else {
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
        .get_members_for_owner_key(&owner, &member_key)
        .into_iter()
        .any(|member| member.get_id() != member_id)
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

    if let Some(widened_type) =
        get_widened_member_assignment_collection_type(analyzer, &type_owner, &expr_type)
    {
        expr_type = widened_type;
    }

    if let Some(widened_type) = get_widened_member_assignment_type(
        analyzer,
        &type_owner,
        &expr_type,
        preserve_table_literals,
    ) {
        expr_type = widened_type;
    }

    if is_global_decl_owner(analyzer, &type_owner) {
        expr_type = merge_open_table_types(analyzer.db, vec![expr_type]);
    }

    bind_type(
        analyzer.db,
        type_owner.clone(),
        LuaTypeCache::InferType(expr_type),
    );

    if let LuaTypeOwner::Member(member_id) = type_owner
        && is_assignment_file_define_member(analyzer.db, member_id)
    {
        let guarded_table_assignment =
            preserve_table_literals || is_guarded_table_assignment_member(analyzer.db, member_id);
        if guarded_table_assignment {
            analyzer
                .db
                .get_member_index_mut()
                .mark_non_overwriting_assignment_member(member_id);
            preserve_guarded_table_assignment_members(analyzer, member_id);
        } else if !is_member_assignment_in_conditional_branch(analyzer, member_id)
            && analyzer
                .db
                .get_member_index()
                .member_function_scope_range(member_id)
                .is_none()
        {
            analyzer
                .db
                .get_member_index_mut()
                .retain_only_member_for_owner_key(member_id);
        }
    }

    Some(())
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
        if !is_member_realm_compatible(analyzer, *member_id, related_member_id) {
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

    if let Some(class_type) = prefer_class_assignment_type(incoming_type) {
        let mut saw_previous_assignment = false;
        let mut class_bootstrap_compatible = true;

        for related_member in &related_members {
            let related_member_id = related_member.get_id();
            if related_member_id == *member_id {
                continue;
            }
            if !is_member_realm_compatible(analyzer, *member_id, related_member_id) {
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

    let should_widen_table_literals = !preserve_table_literals
        && is_table_assignment_merge_type(incoming_type)
        && related_members
            .iter()
            .filter(|related_member| related_member.get_id() != *member_id)
            .all(|related_member| {
                analyzer
                    .db
                    .get_type_index()
                    .get_type_cache(&related_member.get_id().into())
                    .is_some_and(|cache| {
                        cache.is_doc() || is_table_assignment_merge_type(cache.as_type())
                    })
            });
    let mut doc_type: Option<LuaType> = None;
    let mut widened_type =
        widen_related_assignment_type(incoming_type, should_widen_table_literals);
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

        if existing_cache.is_doc() {
            let existing_type = existing_cache.as_type().clone();
            doc_type = Some(match doc_type {
                Some(current) => TypeOps::Union.apply(analyzer.db, &current, &existing_type),
                None => existing_type,
            });
            continue;
        }

        let existing_type =
            widen_related_assignment_type(existing_cache.as_type(), should_widen_table_literals);
        widened_type = TypeOps::Union.apply(analyzer.db, &widened_type, &existing_type);
    }

    if !saw_previous_assignment {
        return None;
    }

    if let Some(doc_type) = doc_type {
        return Some(doc_type);
    }

    Some(if preserve_table_literals {
        crate::prune_redundant_guarded_table_bootstrap_type(analyzer.db, widened_type)
    } else {
        widened_type
    })
}

fn widen_related_assignment_type(typ: &LuaType, widen_table_literals: bool) -> LuaType {
    match typ {
        LuaType::TableConst(_) if widen_table_literals => LuaType::Table,
        _ => crate::widen_literal_type_for_assignment(typ),
    }
}

fn is_table_assignment_merge_type(typ: &LuaType) -> bool {
    matches!(
        typ,
        LuaType::Table
            | LuaType::TableConst(_)
            | LuaType::Object(_)
            | LuaType::MergedTable(_)
            | LuaType::TableOf(_)
    )
}

fn prefer_class_assignment_type(typ: &LuaType) -> Option<LuaType> {
    match typ {
        LuaType::Def(def_id) => Some(LuaType::Def(def_id.clone())),
        LuaType::Ref(ref_id) => Some(LuaType::Ref(ref_id.clone())),
        LuaType::Instance(instance) => prefer_class_assignment_type(instance.get_base()),
        LuaType::TypeGuard(inner) => prefer_class_assignment_type(inner),
        LuaType::Union(union) => prefer_class_assignment_type_from_iter(union.types()),
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
            .types()
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

fn is_member_realm_compatible(
    analyzer: &LuaAnalyzer,
    current_member_id: LuaMemberId,
    related_member_id: LuaMemberId,
) -> bool {
    if !analyzer.db.get_emmyrc().gmod.enabled {
        return true;
    }

    let infer_index = analyzer.db.get_gmod_infer_index();
    infer_index.are_offsets_compatible(
        &current_member_id.file_id,
        current_member_id.get_position(),
        &related_member_id.file_id,
        related_member_id.get_position(),
    )
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
    let member_key =
        LuaMemberKey::from_index_key_or_unknown(analyzer.db, cache, &index_key).ok()?;
    let members = if member_key.is_expr() {
        let access_key_type = member_key_as_expr_type(&member_key)?;
        analyzer
            .db
            .get_member_index()
            .get_members(&owner)
            .unwrap_or_default()
            .into_iter()
            .filter(|member| {
                member_key_matches_type(analyzer.db, access_key_type, member.get_key())
            })
            .collect::<Vec<_>>()
    } else {
        analyzer
            .db
            .get_member_index()
            .get_members_for_owner_key(&owner, &member_key)
    };

    if members.is_empty() {
        return None;
    }

    Some(members.into_iter().map(|member| member.get_id()).collect())
}

fn member_key_as_expr_type(member_key: &LuaMemberKey) -> Option<&LuaType> {
    match member_key {
        LuaMemberKey::ExprType(typ) => Some(typ),
        _ => None,
    }
}

fn get_member_owner_for_prefix_type(prefix_type: LuaType) -> Option<LuaMemberOwner> {
    resolve_index_expr_member_owner_for_file(&prefix_type, None).map(|(owner, _)| owner)
}

fn resolve_index_expr_member_owner(prefix_type: &LuaType) -> Option<(LuaMemberOwner, bool)> {
    resolve_index_expr_member_owner_for_file(prefix_type, None)
}

fn resolve_index_expr_member_owner_for_file(
    prefix_type: &LuaType,
    preferred_file_id: Option<FileId>,
) -> Option<(LuaMemberOwner, bool)> {
    match prefix_type {
        LuaType::TableConst(in_file_range) => {
            Some((LuaMemberOwner::Element(in_file_range.clone()), false))
        }
        LuaType::Def(def_id) => Some((LuaMemberOwner::Type(def_id.clone()), false)),
        LuaType::Ref(ref_id) => Some((LuaMemberOwner::Type(ref_id.clone()), true)),
        LuaType::Instance(instance) => {
            Some((LuaMemberOwner::Element(instance.get_range().clone()), false))
        }
        LuaType::TableOf(inner) => {
            resolve_index_expr_member_owner_for_file(inner, preferred_file_id)
        }
        LuaType::TypeGuard(inner) => {
            resolve_index_expr_member_owner_for_file(inner, preferred_file_id)
        }
        LuaType::Union(union) => {
            pick_preferred_index_expr_member_owner(union.types(), preferred_file_id)
        }
        LuaType::Intersection(intersection) => pick_preferred_index_expr_member_owner(
            intersection.get_types().iter(),
            preferred_file_id,
        ),
        LuaType::MergedTable(merged_table) => pick_preferred_index_expr_member_owner(
            merged_table.get_types().iter(),
            preferred_file_id,
        ),
        LuaType::MultiLineUnion(union) => pick_preferred_index_expr_member_owner(
            union.get_unions().iter().map(|(typ, _)| typ),
            preferred_file_id,
        ),
        _ => None,
    }
}

fn pick_preferred_index_expr_member_owner<'a>(
    types: impl Iterator<Item = &'a LuaType>,
    preferred_file_id: Option<FileId>,
) -> Option<(LuaMemberOwner, bool)> {
    let mut exact_type_owner = None;
    let mut fallback_owner = None;
    for typ in types {
        let Some(owner_info) = resolve_index_expr_member_owner_for_file(typ, preferred_file_id)
        else {
            continue;
        };

        if owner_matches_preferred_file(&owner_info.0, preferred_file_id) {
            return Some(owner_info);
        }

        if matches!(&owner_info.0, LuaMemberOwner::Type(_)) && !owner_info.1 {
            if exact_type_owner.is_none() {
                exact_type_owner = Some(owner_info);
            }
            continue;
        }

        if fallback_owner.is_none() {
            fallback_owner = Some(owner_info);
        }
    }

    exact_type_owner.or(fallback_owner)
}

fn owner_matches_preferred_file(owner: &LuaMemberOwner, preferred_file_id: Option<FileId>) -> bool {
    let Some(preferred_file_id) = preferred_file_id else {
        return false;
    };

    matches!(owner, LuaMemberOwner::Element(range) if range.file_id == preferred_file_id)
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
    let signature_type =
        LuaType::Signature(LuaSignatureId::from_closure(analyzer.file_id, &closure));
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
                set_index_expr_owner(analyzer, var);
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

fn infer_guarded_table_assignment_type(
    analyzer: &mut LuaAnalyzer,
    binary_expr: &LuaExpr,
    left: &LuaExpr,
    right: &LuaExpr,
) -> Result<LuaType, InferFailReason> {
    let right_type = analyzer.infer_expr(right)?;
    let left_type = match analyzer.infer_expr(left) {
        Ok(left_type) => left_type,
        Err(reason) if reason.is_need_resolve() => LuaType::Nil,
        Err(reason) => return Err(reason),
    };

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
    use rowan::{TextRange, TextSize};

    use crate::{FileId, InFiled, LuaMergedTableType, LuaUnionType};

    use super::*;

    fn table_const(start: u32, end: u32) -> LuaType {
        LuaType::TableConst(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(start), TextSize::new(end)),
        ))
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
}
