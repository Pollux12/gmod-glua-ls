use rustc_hash::FxHashSet;

use crate::{
    CacheEntry, DbIndex, FileId, GlobalId, InFiled, InferFailReason, LuaInferCache, LuaMemberKey,
    LuaSemanticDeclId, LuaSignatureId, LuaTypeCache, LuaTypeOwner, TypeOps,
    compilation::analyzer::{
        common::{
            DeclWrite, TypeCacheWriteMode, holds_unbound_iter_template, is_nil_literal_expr,
            reads_settling_iter_var, widen_mutable_local_name_copy,
        },
        gmod::name_expr_resolves_to_scoped_authoring_table,
        unresolve::{UnResolveDecl, UnResolveMember},
    },
    db_index::{
        LuaDeclId, LuaMember, LuaMemberFeature, LuaMemberId, LuaMemberOwner, LuaType,
        member_id_sort_key,
    },
    semantic::{merge_open_table_types, pairs_iter_value_registry_path, remove_false_or_nil},
};
use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAstNode, LuaBinaryExpr, LuaClosureExpr, LuaExpr, LuaFuncStat,
    LuaIndexExpr, LuaIndexKey, LuaLiteralToken, LuaLocalFuncStat, LuaLocalStat, LuaNameExpr,
    LuaTableExpr, LuaTableField, LuaVarExpr, PathTrait,
};
use rustc_hash::FxHashMap;

#[cfg(test)]
use crate::LuaArrayType;

use super::{
    AssignmentHoming, AssignmentStage, LuaAnalyzer, MemberHoming, StatementWrite,
    member_write_policy::{
        alias_target_global_path, direct_local_prefix_has_declared_type,
        direct_local_table_prefix_member_owner,
        flush_pending_dynamic_key_collection_widening_for_members, is_collection_append_write,
        resolve_index_expr_member_owner_for_file, widen_existing_member_collection_type,
    },
};

#[cfg(test)]
use super::member_write_policy::record_pending_dynamic_key_collection_widening;

pub fn analyze_local_stat(analyzer: &mut LuaAnalyzer, local_stat: LuaLocalStat) -> Option<()> {
    let name_list: Vec<_> = local_stat.get_local_name_list().collect();
    let expr_list: Vec<_> = local_stat.get_value_exprs().collect();
    let name_count = name_list.len();
    let expr_count = expr_list.len();
    if expr_count == 0 {
        for local_name in name_list {
            let position = local_name.get_position();
            let decl_id = LuaDeclId::new(analyzer.file_id, position);
            // A deferred-definition attribute is set: skip binding the type and bind it on the first assignment
            if has_delayed_definition_attribute(analyzer, decl_id) {
                continue;
            }
            // Skip Nil binding for mutable locals (those with subsequent write-assignments).
            // This prevents false "cannot assign X to never" diagnostics when a local is used
            // as an upvalue inside a closure and assigned before the closure is first called.
            if is_local_mutable(analyzer, decl_id) {
                continue;
            }
            analyzer.write(StatementWrite::WriteTypeCache {
                owner: decl_id.into(),
                cache: LuaTypeCache::InferType(LuaType::Nil),
                mode: TypeCacheWriteMode::InsertOnly,
            });
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

        if should_defer_guarded_index_alias(analyzer, &expr) {
            let unresolve = UnResolveDecl {
                file_id: analyzer.file_id,
                decl_id,
                expr: expr.clone(),
                ret_idx: 0,
            };
            analyzer.context.add_unresolve(unresolve.into());
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
                    analyzer.context.add_unresolve(unresolve.into());
                    continue;
                }
                if holds_unbound_iter_template(&analyzer.db, analyzer.file_id, &expr, &expr_type)
                    || reads_settling_iter_var(&analyzer.db, analyzer.file_id, &expr)
                {
                    let unresolve = UnResolveDecl {
                        file_id: analyzer.file_id,
                        decl_id,
                        expr: expr.clone(),
                        ret_idx: 0,
                    };
                    analyzer.context.add_unresolve(unresolve.into());
                    continue;
                }
                if should_defer_weak_gmod_call_expr(analyzer, &expr, &expr_type) {
                    let unresolve = UnResolveDecl {
                        file_id: analyzer.file_id,
                        decl_id,
                        expr: expr.clone(),
                        ret_idx: 0,
                    };
                    analyzer.context.add_unresolve(unresolve.into());
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
                    analyzer.context.add_unresolve(unresolve.into());
                    continue;
                }

                // When the `call` arguments contain a table, the table may not have been analyzed yet and must be deferred
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
                    analyzer.context.add_unresolve(unresolve.into());
                    continue;
                }

                let retry_uninformative = should_retry_uninformative_initializer(&expr, &expr_type);
                let write = initializer_decl_write(analyzer, decl_id, &expr);
                analyzer.write(StatementWrite::BindDeclWrite {
                    decl_id,
                    cache: LuaTypeCache::InferType(expr_type),
                    write,
                });
                publish_initializer_fact(analyzer, decl_id, &expr);
                if retry_uninformative {
                    let unresolve = UnResolveDecl {
                        file_id: analyzer.file_id,
                        decl_id,
                        expr: expr.clone(),
                        ret_idx: 0,
                    };
                    analyzer.context.add_unresolve(unresolve.into());
                }
            }
            Err(InferFailReason::None) => {
                if should_defer_none_infer_expr(&expr) {
                    let unresolve = UnResolveDecl {
                        file_id: analyzer.file_id,
                        decl_id,
                        expr: expr.clone(),
                        ret_idx: 0,
                    };
                    analyzer.context.add_unresolve(unresolve.into());
                } else {
                    analyzer.write(StatementWrite::WriteTypeCache {
                        owner: decl_id.into(),
                        cache: LuaTypeCache::InferType(LuaType::Nil),
                        mode: TypeCacheWriteMode::InsertOnly,
                    });
                }
            }
            Err(_) => {
                let unresolve = UnResolveDecl {
                    file_id: analyzer.file_id,
                    decl_id,
                    expr: expr.clone(),
                    ret_idx: 0,
                };

                analyzer.context.add_unresolve(unresolve.into());
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
                            let ret_type = ret_type.cloned().unwrap_or(LuaType::Nil);
                            let write = initializer_decl_write(analyzer, decl_id, last_expr);
                            analyzer.write(StatementWrite::BindDeclWrite {
                                decl_id,
                                cache: LuaTypeCache::InferType(ret_type),
                                write,
                            });
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
                            let write = initializer_decl_write(analyzer, decl_id, last_expr);
                            analyzer.write(StatementWrite::BindDeclWrite {
                                decl_id,
                                cache: LuaTypeCache::InferType(typ),
                                write,
                            });
                        }
                        return Some(());
                    }
                }
                Err(_) => {
                    for i in expr_count..name_count {
                        let name = name_list.get(i)?;
                        let position = name.get_position();
                        let decl_id = LuaDeclId::new(analyzer.file_id, position);
                        if last_expr_is_call {
                            let write = initializer_decl_write(analyzer, decl_id, last_expr);
                            analyzer.write(StatementWrite::BindDeclWrite {
                                decl_id,
                                cache: LuaTypeCache::InferType(LuaType::Unknown),
                                write,
                            });
                        }
                        let unresolve = UnResolveDecl {
                            file_id: analyzer.file_id,
                            decl_id,
                            expr: last_expr.clone(),
                            ret_idx: i - expr_count + 1,
                        };

                        analyzer.context.add_unresolve(unresolve.into());
                    }
                }
            }
        } else {
            for i in expr_count..name_count {
                let name = name_list.get(i)?;
                let position = name.get_position();
                let decl_id = LuaDeclId::new(analyzer.file_id, position);
                analyzer.write(StatementWrite::WriteTypeCache {
                    owner: decl_id.into(),
                    cache: LuaTypeCache::InferType(LuaType::Nil),
                    mode: TypeCacheWriteMode::InsertOnly,
                });
            }
        }
    }

    Some(())
}

fn should_defer_guarded_index_alias(analyzer: &mut LuaAnalyzer, expr: &LuaExpr) -> bool {
    let Some(left) = guarded_index_or_empty_table_left(expr) else {
        return false;
    };
    match analyzer.infer_expr(&left) {
        Ok(ty) => ty.is_unknown() || ty.is_nil(),
        Err(reason) => reason.is_need_resolve(),
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
        LuaType::Union(union) => union
            .nullable_inner()
            .is_some_and(|inner| inner.is_any() || inner.is_unknown()),
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
        if matches!(cache.expr_cache.peek(&syntax_id), Some(CacheEntry::Cache(typ)) if typ.is_nil() || is_weak_dynamic_index_alias_type(typ))
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

/// Whether the write goes through a computed index (`t[k].field = v`).
///
/// Which entry `t[k]` names is not knowable statically, so the declaration walk
/// files such a write under the path's placeholder segment (`t.[]`). Re-homing
/// it onto whichever concrete entry the prefix *type* happened to resolve to
/// makes ownership a function of how far inference had run when the write was
/// analysed, which differs between a cold build and a warm re-index.
fn prefix_path_has_computed_segment(prefix_expr: &LuaExpr) -> bool {
    let mut current = prefix_expr.clone();
    loop {
        let LuaExpr::IndexExpr(index_expr) = current else {
            return false;
        };
        if matches!(
            index_expr.get_index_key(),
            Some(LuaIndexKey::Expr(_) | LuaIndexKey::Idx(_))
        ) {
            return true;
        }
        match index_expr.get_prefix_expr() {
            Some(next) => current = next,
            None => return false,
        }
    }
}

fn set_index_expr_owner(analyzer: &mut LuaAnalyzer, var_expr: LuaVarExpr) -> Option<()> {
    let index_expr = LuaIndexExpr::cast(var_expr.syntax().clone())?;
    if !analyzer.homed_members.insert(LuaMemberId::new(
        index_expr.get_syntax_id(),
        analyzer.file_id,
    )) {
        return Some(());
    }
    let prefix_expr = index_expr.get_prefix_expr()?;
    if prefix_path_has_computed_segment(&prefix_expr) {
        let member_id = LuaMemberId::new(index_expr.get_syntax_id(), analyzer.file_id);
        if matches!(
            analyzer.db.get_member_index().get_member_owner(&member_id),
            Some(LuaMemberOwner::GlobalPath(_))
        ) {
            return Some(());
        }
    }
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

    // A write through the value variable of `for _, v in pairs(REG)` states a
    // field on *every* value of `REG`, so it belongs on the registry's wildcard
    // segment rather than on whichever entry the prefix type had settled to.
    if let Some(path) = pairs_iter_value_registry_path(&analyzer.db, analyzer.file_id, &prefix_expr)
    {
        let member_owner = LuaMemberOwner::GlobalPath(GlobalId::new(&path));
        apply_index_expr_member_owner(analyzer, index_expr, member_owner, false);
        return Some(());
    }

    match analyzer.infer_expr(&prefix_expr.clone()) {
        Ok(prefix_type) => {
            if should_skip_ambiguous_unknown_key_table_owner(analyzer, &prefix_type, &index_expr) {
                return Some(());
            }
            let (member_owner, set_owner_only) = resolve_index_expr_member_owner_for_file(
                &analyzer.db,
                &prefix_type,
                Some(analyzer.file_id),
            )?;
            apply_index_expr_member_owner(analyzer, index_expr, member_owner, set_owner_only);
        }
        Err(InferFailReason::None) => {}
        Err(_) => {
            // Every other branch above reaches
            // `apply_index_expr_member_owner`, which *creates* the
            // `LuaMember` and then attaches it. This branch cannot: the
            // prefix is not inferable yet, so there is no owner to attach
            // to.
            let unresolve_member = UnResolveMember {
                file_id: analyzer.file_id,
                member_id: LuaMemberId::new(var_expr.get_syntax_id(), analyzer.file_id),
                expr: None,
                prefix: Some(prefix_expr),
                ret_idx: 0,
            };
            analyzer.context.add_unresolve(unresolve_member.into());
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
    let Ok(member_key) = LuaMemberKey::from_index_key_or_unknown(&analyzer.db, cache, &index_key)
    else {
        return false;
    };
    if !matches!(member_key, LuaMemberKey::ExprType(ref typ) if typ.is_unknown()) {
        return false;
    }

    has_multiple_distinct_index_expr_member_owners(prefix_type)
}

pub(in crate::compilation::analyzer) fn has_multiple_distinct_index_expr_member_owners(
    typ: &LuaType,
) -> bool {
    let mut owners = FxHashSet::default();
    collect_distinct_index_expr_member_owners(typ, &mut owners);
    owners.len() > 1
}

fn collect_distinct_index_expr_member_owners(
    typ: &LuaType,
    owners: &mut FxHashSet<LuaMemberOwner>,
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
        // The settled form of one slot's several bootstrap literals is one
        // table, not a choice between alternatives: a write through it lands on
        // the arm the writing file prefers. Counting every arm made the answer
        // depend on whether the slot had settled yet -- the walk reads the one
        // literal it has reached on a cold build and the merge on a re-index.
        LuaType::MergedTable(merged_table) => merged_table
            .get_types()
            .iter()
            .find_map(representative_index_expr_member_owner)
            .is_some_and(|owner| insert_index_expr_member_owner(owners, owner)),
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

/// The first owner a type names, in the type's canonical arm order.
fn representative_index_expr_member_owner(typ: &LuaType) -> Option<LuaMemberOwner> {
    match typ {
        LuaType::TableConst(in_file_range) => Some(LuaMemberOwner::Element(in_file_range.clone())),
        LuaType::Def(type_id) | LuaType::Ref(type_id) => {
            Some(LuaMemberOwner::Type(type_id.clone()))
        }
        LuaType::Instance(instance) => Some(LuaMemberOwner::Element(instance.get_range().clone())),
        LuaType::TableOf(inner) => representative_index_expr_member_owner(inner),
        LuaType::TypeGuard(inner) => representative_index_expr_member_owner(inner),
        LuaType::Union(union) => union
            .types()
            .find_map(representative_index_expr_member_owner),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .find_map(representative_index_expr_member_owner),
        LuaType::MergedTable(merged_table) => merged_table
            .get_types()
            .iter()
            .find_map(representative_index_expr_member_owner),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .find_map(|(typ, _)| representative_index_expr_member_owner(typ)),
        _ => None,
    }
}

fn insert_index_expr_member_owner(
    owners: &mut FxHashSet<LuaMemberOwner>,
    owner: LuaMemberOwner,
) -> bool {
    owners.insert(owner);
    owners.len() > 1
}

fn try_resolve_scoped_class_prefix_member_owner(
    analyzer: &LuaAnalyzer,
    prefix_expr: &LuaExpr,
) -> Option<(LuaMemberOwner, bool)> {
    scoped_class_prefix_member_owner(&analyzer.db, analyzer.file_id, prefix_expr)
}

/// The walk's scoped-class fast path, shared with the owner-settle pass: a
/// `SWEP`/`ENT`/`GM`-style prefix (or `self` inside one of its colon methods)
/// names the file's authoring class without any inference, so the answer does
/// not move between a cold build and a warm re-index.
pub(crate) fn scoped_class_prefix_member_owner(
    db: &DbIndex,
    file_id: FileId,
    prefix_expr: &LuaExpr,
) -> Option<(LuaMemberOwner, bool)> {
    let LuaExpr::NameExpr(name_expr) = prefix_expr else {
        return None;
    };

    let name = name_expr.get_name_text()?;
    if name != "self" {
        let class_decl_id = name_expr_resolves_to_scoped_authoring_table(db, file_id, name_expr)?;
        return Some((LuaMemberOwner::Type(class_decl_id), false));
    }

    if !name_expr_resolves_to_implicit_self(db, file_id, name_expr) {
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
    let class_decl_id =
        name_expr_resolves_to_scoped_authoring_table(db, file_id, &class_name_expr)?;
    Some((LuaMemberOwner::Type(class_decl_id), false))
}

fn name_expr_resolves_to_implicit_self(
    db: &DbIndex,
    file_id: FileId,
    name_expr: &LuaNameExpr,
) -> bool {
    db.get_reference_index()
        .get_local_reference(&file_id)
        .and_then(|file_ref| file_ref.get_decl_id(&name_expr.get_range()))
        .and_then(|decl_id| db.get_decl_index().get_decl(&decl_id))
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
    // for provenance purposes, so the member follows the path's `---@class`
    // flips. Only computed when the owner resolved to a class: path and
    // Element owners already carry their provenance through the homing note.
    let alias_path = match &member_owner {
        LuaMemberOwner::Type(_) => index_expr
            .get_prefix_expr()
            .and_then(|prefix| alias_target_global_path(&analyzer.db, analyzer.file_id, &prefix)),
        _ => None,
    };

    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let Ok(member_key) = LuaMemberKey::from_index_key_or_unknown(&analyzer.db, cache, &index_key)
    else {
        return Some(());
    };
    let new_member = if analyzer
        .db
        .get_member_index()
        .get_member(&member_id)
        .is_none()
    {
        let decl_feature = if analyzer.context.metas.contains(&analyzer.file_id) {
            LuaMemberFeature::MetaDefine
        } else {
            LuaMemberFeature::FileDefine
        };
        Some(LuaMember::new(
            member_id,
            member_key.clone(),
            decl_feature,
            None,
        ))
    } else {
        None
    };
    analyzer.write(StatementWrite::HomeMember(MemberHoming {
        member_id,
        key: member_key,
        owner: member_owner,
        new_member,
        set_owner_only,
        guarded_table_assignment,
        alias_path,
    }));

    Some(())
}

// assign stat is toooooooooo complex
/// Records where a call or index initializer's value came from, when the
/// declaration holds exactly that value.
fn publish_initializer_fact(analyzer: &mut LuaAnalyzer, decl_id: LuaDeclId, expr: &LuaExpr) {
    if !is_call_or_index_expr(expr) {
        return;
    }
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let fact = crate::semantic::infer_expr_fact_with_cache(&analyzer.db, cache, expr.clone());
    let fact = match fact.typ() {
        LuaType::Variadic(multi) => match multi.get_type(0) {
            Some(first) => fact.with_runtime_type(first.clone()),
            None => return,
        },
        _ => fact,
    };
    analyzer.write(StatementWrite::PublishInitializerFact { decl_id, fact });
}

pub fn analyze_assign_stat(analyzer: &mut LuaAnalyzer, assign_stat: LuaAssignStat) -> Option<()> {
    if let AssignmentStage::Homing(_) = analyzer.assignment_stage {
        return home_assignment_targets(analyzer, assign_stat);
    }
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

        let skipped = match &analyzer.assignment_stage {
            AssignmentStage::Values(homing) => homing.skipped.get(i).copied().unwrap_or(false),
            _ => skips_assignment(analyzer, &var, expr),
        };
        if skipped {
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

        let read_before_homing = match &analyzer.assignment_stage {
            AssignmentStage::Values(homing) => {
                homing.declared_empty_table_types.get(i).cloned().flatten()
            }
            _ => None,
        };
        let declared_empty_table_type = match read_before_homing {
            Some(declared) => declared,
            None => declared_empty_table_assignment_type(analyzer, &var, expr),
        };
        set_index_expr_owner(analyzer, var.clone());

        if copies_table_into_itself(analyzer, &var, expr) {
            assign_merge_type_owner_and_expr_type(
                analyzer,
                type_owner.clone(),
                &LuaType::Never,
                0,
                false,
                DeclWrite {
                    position: expr.get_position(),
                    may_improve_after_resolve: false,
                    reads_out_of_decl: false,
                    may_narrow_uninformative: false,
                    resolved_initializer: false,
                    fills_own_default: false,
                },
            );
            continue;
        }

        let expr_type = match declared_empty_table_type
            .map(Ok)
            .unwrap_or_else(|| analyzer.infer_expr(expr))
        {
            Ok(mut expr_type) => {
                if let LuaType::Variadic(multi) = expr_type {
                    expr_type = multi.get_type(0)?.clone();
                }

                if expr_type.is_nil() && should_defer_nil_gmod_expr(analyzer, expr) {
                    add_unresolve_for_assignment(analyzer, type_owner, &var, expr.clone());
                    continue;
                }
                if holds_unbound_iter_template(&analyzer.db, analyzer.file_id, expr, &expr_type)
                    || reads_settling_iter_var(&analyzer.db, analyzer.file_id, expr)
                {
                    add_unresolve_for_assignment(analyzer, type_owner, &var, expr.clone());
                    continue;
                }
                if should_defer_weak_gmod_call_expr(analyzer, expr, &expr_type) {
                    add_unresolve_for_assignment(analyzer, type_owner, &var, expr.clone());
                    continue;
                }
                if should_defer_weak_gmod_member_index_assignment(
                    analyzer,
                    &type_owner,
                    expr,
                    &expr_type,
                ) {
                    add_unresolve_for_assignment(analyzer, type_owner, &var, expr.clone());
                    continue;
                }
                if expr_type.is_unknown() && is_undefined_global_name_expr(analyzer, expr) {
                    // See note in analyze_local_stat: undefined-global RHS
                    // is `nil` at runtime, not "unknown".
                    LuaType::Nil
                } else {
                    if should_retry_uninformative_initializer(expr, &expr_type)
                        || should_retry_narrowing_decl_assignment(
                            analyzer,
                            &type_owner,
                            expr,
                            &expr_type,
                        )
                    {
                        add_unresolve_for_assignment(
                            analyzer,
                            type_owner.clone(),
                            &var,
                            expr.clone(),
                        );
                    }
                    expr_type
                }
            }
            // Reading an undefined global yields `nil` at runtime, so the
            // assignment target's value is `nil` (not unknown). This mirrors
            // the local-stat path above so hover/inference stays consistent.
            Err(InferFailReason::None) => {
                if should_defer_none_infer_expr(expr) {
                    add_unresolve_for_assignment(analyzer, type_owner, &var, expr.clone());
                    continue;
                }
                LuaType::Nil
            }
            Err(_) => {
                add_unresolve_for_assignment(analyzer, type_owner, &var, expr.clone());
                continue;
            }
        };

        if seeds_empty_decl_from_own_read(analyzer, &type_owner, expr) {
            continue;
        }

        // With a deferred-definition attribute, bind the initial definition first
        if let LuaVarExpr::NameExpr(name_expr) = var {
            if let Some(decl_id) = get_delayed_definition_decl_id(analyzer, name_expr) {
                analyzer.write(StatementWrite::BindType {
                    owner: decl_id.into(),
                    cache: LuaTypeCache::InferType(expr_type.clone()),
                });
            }
        }

        let mut expr_type =
            member_assignment_or_source_type(analyzer, &type_owner, expr, expr_type);
        if matches!(type_owner, LuaTypeOwner::Member(_)) && expr_reads_own_path(&var, expr) {
            // `t.k = t.k + 1` holds a new value every time it runs: a literal
            // would be one run's value, and writers reading each other's
            // literal would count up without end.
            expr_type = crate::db_index::widen_literal_type_for_assignment(&expr_type);
        }

        widen_existing_member_collection_type(analyzer, &var, &expr_type);
        assign_merge_type_owner_and_expr_type(
            analyzer,
            type_owner.clone(),
            &expr_type,
            0,
            false,
            DeclWrite {
                position: expr.get_position(),
                may_improve_after_resolve: may_improve_after_resolve(expr),
                reads_out_of_decl: matches!(&type_owner, LuaTypeOwner::Decl(decl_id)
                    if expr_reads_out_of_decl(&analyzer.db, analyzer.file_id, *decl_id, expr)),
                may_narrow_uninformative: is_call_or_index_expr(expr),
                resolved_initializer: false,
                fills_own_default: matches!(&type_owner, LuaTypeOwner::Decl(decl_id)
                    if expr_fills_own_default(&analyzer.db, analyzer.file_id, *decl_id, expr)),
            },
        );
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
                            type_owner.clone(),
                            &last_expr_type,
                            i - expr_count + 1,
                            false,
                            DeclWrite {
                                position: last_expr.get_position(),
                                may_improve_after_resolve: may_improve_after_resolve(last_expr),
                                reads_out_of_decl: matches!(&type_owner, LuaTypeOwner::Decl(decl_id)
                                if expr_reads_out_of_decl(
                                    &analyzer.db,
                                    analyzer.file_id,
                                    *decl_id,
                                    last_expr,
                                )),
                                may_narrow_uninformative: is_call_or_index_expr(last_expr),
                                resolved_initializer: false,
                                fills_own_default: matches!(&type_owner, LuaTypeOwner::Decl(decl_id)
                                if expr_fills_own_default(
                                    &analyzer.db,
                                    analyzer.file_id,
                                    *decl_id,
                                    last_expr,
                                )),
                            },
                        );
                    }
                } else {
                    for i in expr_count..var_count {
                        let var = var_list.get(i)?;
                        let type_owner = get_var_owner(analyzer, var.clone());
                        set_index_expr_owner(analyzer, var.clone());
                        assign_merge_type_owner_and_expr_type(
                            analyzer,
                            type_owner.clone(),
                            &LuaType::Any,
                            0, // Any doesn't need indexing
                            false,
                            DeclWrite {
                                position: last_expr.get_position(),
                                may_improve_after_resolve: may_improve_after_resolve(last_expr),
                                reads_out_of_decl: matches!(&type_owner, LuaTypeOwner::Decl(decl_id)
                                if expr_reads_out_of_decl(
                                    &analyzer.db,
                                    analyzer.file_id,
                                    *decl_id,
                                    last_expr,
                                )),
                                may_narrow_uninformative: is_call_or_index_expr(last_expr),
                                resolved_initializer: false,
                                fills_own_default: matches!(&type_owner, LuaTypeOwner::Decl(decl_id)
                                if expr_fills_own_default(
                                    &analyzer.db,
                                    analyzer.file_id,
                                    *decl_id,
                                    last_expr,
                                )),
                            },
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

/// Whether `var = expr` is a `pairs` copy of a table into itself, which gives
/// no field a value: see [`crate::compilation::analyzer::copies_table_into_itself`].
fn copies_table_into_itself(analyzer: &mut LuaAnalyzer, var: &LuaVarExpr, expr: &LuaExpr) -> bool {
    let LuaVarExpr::IndexExpr(index_expr) = var else {
        return false;
    };
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    crate::compilation::analyzer::copies_table_into_itself(&analyzer.db, cache, index_expr, expr)
}

/// Whether an assignment leaves a value's target alone: a `nil` that only
/// clears a table's shape, or a nullable append to a collection.
fn skips_assignment(analyzer: &mut LuaAnalyzer, var: &LuaVarExpr, expr: &LuaExpr) -> bool {
    should_skip_nil_table_shape_assignment(analyzer, var, expr)
        || should_skip_nullable_collection_append_shape_assignment(analyzer, var, expr)
}

/// The homing stage of an assignment: homes the members whose values are
/// inferred after their homing, recording what the values stage reads from
/// before it. A guarded `x = x or {}` homes only once its value is inferred,
/// in the values stage.
fn home_assignment_targets(analyzer: &mut LuaAnalyzer, assign_stat: LuaAssignStat) -> Option<()> {
    let (var_list, expr_list) = assign_stat.get_var_and_expr_list();
    let mut homing = AssignmentHoming::default();
    for (i, expr) in expr_list.iter().enumerate() {
        let Some(var) = var_list.get(i) else {
            break;
        };
        let skipped = skips_assignment(analyzer, var, expr);
        homing.skipped.push(skipped);
        let declared_empty_table_type = match self_default_arms(var, expr) {
            _ if skipped => None,
            Some((_, LuaExpr::TableExpr(_))) => None,
            Some(_) => {
                set_index_expr_owner(analyzer, var.clone());
                None
            }
            None => {
                let declared = declared_empty_table_assignment_type(analyzer, var, expr);
                set_index_expr_owner(analyzer, var.clone());
                Some(declared)
            }
        };
        homing
            .declared_empty_table_types
            .push(declared_empty_table_type);
    }
    homing.homed = analyzer.homed_members.clone();
    analyzer.assignment_stage = AssignmentStage::Homing(homing);
    Some(())
}

/// The two arms of `x = x or <default>`, when `expr` is one for `var`.
fn self_default_arms(var: &LuaVarExpr, expr: &LuaExpr) -> Option<(LuaExpr, LuaExpr)> {
    let LuaExpr::BinaryExpr(binary_expr) = expr else {
        return None;
    };
    if binary_expr.get_op_token()?.get_op() != BinaryOperator::OpOr {
        return None;
    }
    let (left, right) = binary_expr.get_exprs()?;
    let left_var = LuaVarExpr::cast(left.syntax().clone())?;
    (var.get_access_path()? == left_var.get_access_path()?).then_some((left, right))
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
        crate::infer_index_expr(&analyzer.db, cache, index_expr.clone(), false).ok()?;
    let table_value_type = declared_table_assignment_type(&analyzer.db, &declared_type)?;

    analyzer.write(StatementWrite::WriteTypeCache {
        owner: LuaTypeOwner::SyntaxId(InFiled::new(analyzer.file_id, table_expr.get_syntax_id())),
        cache: LuaTypeCache::InferType(table_value_type.clone()),
        mode: TypeCacheWriteMode::InsertOnly,
    });

    Some(table_value_type)
}

fn declared_table_assignment_type(db: &crate::DbIndex, declared_type: &LuaType) -> Option<LuaType> {
    match declared_type {
        // Another assignment's table literal, a value that declares nothing.
        LuaType::TableConst(_) => None,
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

    let Some(index_key) = index_expr.get_index_key() else {
        return false;
    };
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let member_key = match LuaMemberKey::from_index_key_or_unknown(&analyzer.db, cache, &index_key)
    {
        Ok(member_key) => member_key,
        Err(_) => return index_key.is_expr(),
    };
    if member_key.is_expr() {
        return true;
    }

    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return false;
    };

    let Ok(prefix_type) = analyzer.infer_expr(&prefix_expr) else {
        return false;
    };

    // A prefix that has not settled yet cannot answer this, and the write is a
    // delete either way: `t[k] = nil` removes an entry, it never adds a member
    // typed `nil`. A receiver typed by a `fun(self: T)` callback slot is still
    // `unknown` while its file is walked, so a member attached here would land
    // on the slot every closure filling it shares.
    if matches!(prefix_type, LuaType::Unknown | LuaType::Never) {
        return true;
    }

    if !is_table_shape_cleanup_type(&prefix_type) {
        return false;
    }

    if is_typed_collection_cleanup_type(&prefix_type) {
        return true;
    }

    let Some((owner, _)) = resolve_index_expr_member_owner_for_file(
        &analyzer.db,
        &prefix_type,
        Some(analyzer.file_id),
    ) else {
        return false;
    };

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
    crate::compilation::analyzer::initializer_reads_through_call_or_index(expr)
}

fn may_improve_after_resolve(expr: &LuaExpr) -> bool {
    crate::compilation::analyzer::initializer_may_improve_after_resolve(expr)
}

fn initializer_decl_write(analyzer: &LuaAnalyzer, decl_id: LuaDeclId, expr: &LuaExpr) -> DeclWrite {
    let may_improve = may_improve_after_resolve(expr);
    DeclWrite {
        position: expr.get_position(),
        may_improve_after_resolve: may_improve,
        reads_out_of_decl: expr_reads_out_of_decl(&analyzer.db, analyzer.file_id, decl_id, expr),
        may_narrow_uninformative: may_improve,
        resolved_initializer: false,
        fills_own_default: expr_fills_own_default(&analyzer.db, analyzer.file_id, decl_id, expr),
    }
}

/// Whether an initializer that inferred to a type carrying no information
/// has to be queued for the unresolve pass as well as committed here.
fn should_retry_uninformative_initializer(expr: &LuaExpr, expr_type: &LuaType) -> bool {
    crate::compilation::analyzer::initializer_may_improve_after_resolve(expr)
        && !crate::db_index::is_informative_type(expr_type)
}

/// Whether an assignment that *would* narrow an uninformative decl cache
/// has to go through the unresolve pass instead of being committed here.
fn should_retry_narrowing_decl_assignment(
    analyzer: &LuaAnalyzer,
    type_owner: &LuaTypeOwner,
    expr: &LuaExpr,
    expr_type: &LuaType,
) -> bool {
    if !is_call_or_index_expr(expr) || !crate::db_index::is_informative_type(expr_type) {
        return false;
    }

    matches!(type_owner, LuaTypeOwner::Decl(_))
        && matches!(
            analyzer.db.get_type_index().get_type_cache(type_owner),
            Some(LuaTypeCache::InferType(LuaType::Any | LuaType::Unknown))
        )
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

/// Whether `expr` is the default-value idiom for `decl_id` — `p = p or DEFAULT`.
///
/// The result always includes the declaration's own type, so unlike a plain
/// reassignment it refines the declaration rather than replacing it, and is the
/// one body write a parameter may take its type from.
pub(crate) fn expr_fills_own_default(
    db: &DbIndex,
    file_id: crate::FileId,
    decl_id: LuaDeclId,
    expr: &LuaExpr,
) -> bool {
    let LuaExpr::BinaryExpr(binary_expr) = expr else {
        return false;
    };
    if binary_expr.get_op_token().map(|op| op.get_op()) != Some(BinaryOperator::OpOr) {
        return false;
    }
    let Some((LuaExpr::NameExpr(left), _)) = binary_expr.get_exprs() else {
        return false;
    };
    let Some(name) = left.get_name_text() else {
        return false;
    };

    db.get_decl_index()
        .get_decl_tree(&file_id)
        .and_then(|decl_tree| decl_tree.find_local_decl(&name, left.get_position()))
        .is_some_and(|decl| decl.get_id() == decl_id)
}

/// Whether `expr` reads out of `decl_id` itself: the `x = x.field` shape,
/// and the same read buried in an operand or call argument (`width =
/// bit.bor(bit.lshift(width:byte(1), 24), ...)`). Depth does not change the
/// self-contradiction — the value still cannot be the decl's lifetime type,
/// because it was computed from a read that type would reject.
/// Whether `expr`, outside the closures it holds, reads the path `var`
/// writes.
fn expr_reads_own_path(var: &LuaVarExpr, expr: &LuaExpr) -> bool {
    let Some(path) = var.get_access_path() else {
        return false;
    };
    let expr_range = expr.get_range();
    std::iter::once(expr.syntax().clone())
        .chain(expr.syntax().descendants())
        .filter_map(LuaVarExpr::cast)
        .filter(|read| {
            !read
                .ancestors::<LuaClosureExpr>()
                .any(|closure| expr_range.contains_range(closure.get_range()))
        })
        .any(|read| {
            read.get_access_path()
                .is_some_and(|read_path| read_path == path)
        })
}

pub(crate) fn expr_reads_out_of_decl(
    db: &DbIndex,
    file_id: crate::FileId,
    decl_id: LuaDeclId,
    expr: &LuaExpr,
) -> bool {
    if index_chain_roots_at_decl(db, file_id, decl_id, expr) {
        return true;
    }

    let expr_range = expr.get_range();
    expr.descendants::<LuaIndexExpr>()
        .filter(|index_expr| {
            !index_expr
                .ancestors::<LuaClosureExpr>()
                .any(|closure| expr_range.contains_range(closure.get_range()))
        })
        .any(|index_expr| {
            index_chain_roots_at_decl(db, file_id, decl_id, &LuaExpr::IndexExpr(index_expr))
        })
}
fn index_chain_roots_at_decl(
    db: &DbIndex,
    file_id: crate::FileId,
    decl_id: LuaDeclId,
    expr: &LuaExpr,
) -> bool {
    let mut current = expr.clone();
    loop {
        match current {
            LuaExpr::IndexExpr(index_expr) => match index_expr.get_prefix_expr() {
                Some(prefix) => current = prefix,
                None => return false,
            },
            LuaExpr::NameExpr(name_expr) => {
                return db
                    .get_reference_index()
                    .get_local_reference(&file_id)
                    .and_then(|file_ref| file_ref.get_decl_id(&name_expr.get_range()))
                    .is_some_and(|read| {
                        read == decl_id || local_alias_roots_at_decl(db, file_id, decl_id, read)
                    });
            }
            _ => return false,
        }
    }
}

/// Whether `alias` is a local never written after its declaration whose
/// initializer reads out of `decl_id`: reading the alias reads the decl's
/// slot (`local id = data; data = id`).
fn local_alias_roots_at_decl(
    db: &DbIndex,
    file_id: crate::FileId,
    decl_id: LuaDeclId,
    alias: LuaDeclId,
) -> bool {
    db.get_reference_index()
        .get_decl_references(&file_id, &alias)
        .is_some_and(|references| !references.mutable)
        && crate::semantic::local_initializer_expr(db, alias).is_some_and(
            |(ret_idx, initializer)| {
                ret_idx == 0 && index_chain_roots_at_decl(db, file_id, decl_id, &initializer)
            },
        )
}

/// Whether this assignment would seed an empty decl slot from a read out of
/// that same decl.
fn seeds_empty_decl_from_own_read(
    analyzer: &LuaAnalyzer,
    type_owner: &LuaTypeOwner,
    expr: &LuaExpr,
) -> bool {
    let LuaTypeOwner::Decl(decl_id) = type_owner else {
        return false;
    };

    analyzer
        .db
        .get_type_index()
        .get_type_cache(type_owner)
        .is_none()
        && expr_reads_out_of_decl(&analyzer.db, analyzer.file_id, *decl_id, expr)
}

fn add_unresolve_for_assignment(
    analyzer: &mut LuaAnalyzer,
    type_owner: LuaTypeOwner,
    var: &LuaVarExpr,
    expr: LuaExpr,
) {
    match type_owner {
        LuaTypeOwner::Decl(decl_id) => {
            // A read out of the decl being assigned (`limit =
            // limit.maximum`) must not queue a deferred write. The decl
            // slot is empty until one of the file's deferred writes
            // resolves, and `bind_type` has no acceptance rule for an empty
            // slot, so whichever lands first owns the decl's lifetime type.
            if expr_reads_out_of_decl(&analyzer.db, analyzer.file_id, decl_id, &expr) {
                return;
            }

            let unresolve_decl = UnResolveDecl {
                file_id: analyzer.file_id,
                decl_id,
                expr,
                ret_idx: 0,
            };

            analyzer.context.add_unresolve(unresolve_decl.into());
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
            analyzer.context.add_unresolve(unresolve_member.into());
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
    write: DeclWrite,
) {
    let mut expr_type = expr_type.clone();
    if let LuaType::Variadic(multi) = expr_type {
        expr_type = multi.get_type(idx).unwrap_or(&LuaType::Nil).clone();
    }

    if let LuaTypeOwner::Member(member_id) = type_owner {
        // A self-referential guarded bootstrap (`x.y = x.y or {}`) assigns its
        // own `{}` whatever the self-read resolves to, which is what
        // `special_or_rule` folds it to. The inferred expression type can still
        // carry that self-read when it was memoised before the fold ran, and
        // what the read resolved to is whichever sibling file the batch
        // analysed first.
        let value =
            guarded_table_bootstrap_member_type(&analyzer.db, member_id, true).unwrap_or(expr_type);
        // A repeated `x.y = x.y or {}` guard names one table, however many
        // files open with it: each writer means "reuse it if it is there".
        let preserve_table_literals =
            preserve_table_literals || is_guarded_table_assignment_member(&analyzer.db, member_id);
        analyzer.write(StatementWrite::AssignMember {
            member_id,
            value,
            dynamic_key: is_dynamic_expr_key_member_assignment(analyzer, &type_owner),
            preserve_table_literals,
        });
        return;
    }

    if is_global_decl_owner(analyzer, &type_owner) {
        expr_type = merge_open_table_types(&analyzer.db, vec![expr_type]);
    }

    match &type_owner {
        LuaTypeOwner::Decl(decl_id) => {
            analyzer.write(StatementWrite::BindDeclWrite {
                decl_id: *decl_id,
                cache: LuaTypeCache::InferType(expr_type),
                write,
            });
        }
        _ => {
            analyzer.write(StatementWrite::BindType {
                owner: type_owner,
                cache: LuaTypeCache::InferType(expr_type),
            });
        }
    }
}

fn member_assignment_or_source_type(
    analyzer: &mut LuaAnalyzer,
    type_owner: &LuaTypeOwner,
    expr: &LuaExpr,
    fallback_type: LuaType,
) -> LuaType {
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    member_assignment_or_source_type_with_cache(
        &analyzer.db,
        cache,
        analyzer.file_id,
        type_owner,
        expr,
        fallback_type,
    )
}

pub(in crate::compilation::analyzer) fn member_assignment_or_source_type_with_cache(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    type_owner: &LuaTypeOwner,
    expr: &LuaExpr,
    fallback_type: LuaType,
) -> LuaType {
    if !matches!(type_owner, LuaTypeOwner::Member(_)) {
        return fallback_type;
    }
    let fallback_type = widen_mutable_local_name_copy(db, file_id, expr, fallback_type);

    let Some(arms) = top_level_or_expr_arms(expr) else {
        return fallback_type;
    };

    let Some((last_arm, non_final_arms)) = arms.split_last() else {
        return fallback_type;
    };

    let mut source_type = None;
    for arm in non_final_arms {
        let Ok(arm_type) = crate::infer_expr(db, cache, arm.clone()) else {
            return fallback_type;
        };
        if arm_type.is_unknown() || arm_type.is_any() {
            return fallback_type;
        }
        let arm_type = remove_false_or_nil(arm_type);
        source_type = Some(match source_type {
            Some(current) => TypeOps::Union.apply(db, &current, &arm_type),
            None => arm_type,
        });
    }

    let Ok(last_type) = crate::infer_expr(db, cache, last_arm.clone()) else {
        return fallback_type;
    };
    if last_type.is_unknown() || last_type.is_any() {
        return fallback_type;
    }

    match source_type {
        Some(current) => TypeOps::Union.apply(db, &current, &last_type),
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

/// Whether the write's key is computed.
///
/// Read off the syntax, not off the member's key: a computed key whose
/// expression the walk could resolve is minted under the name it resolved
/// to, and whether it could resolve is a property of how far the batch had
/// got when the write was walked.
fn is_dynamic_expr_key_member_assignment(
    analyzer: &LuaAnalyzer,
    type_owner: &LuaTypeOwner,
) -> bool {
    let LuaTypeOwner::Member(member_id) = type_owner else {
        return false;
    };
    member_write_node(&analyzer.db, *member_id)
        .and_then(LuaIndexExpr::cast)
        .and_then(|index_expr| index_expr.get_index_key())
        .is_some_and(|key| matches!(key, LuaIndexKey::Expr(_)))
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

/// Applies the visibility marks a write earns from its own syntax, for a write
/// the walk did not get to classify.
///
/// The walk marks each assignment as it binds it — guarded bootstrap, or
/// conditional branch — and those marks decide which writers stay visible for
/// the slot. A write whose right-hand side could not be inferred in time is
/// finished by the unresolve pass instead, which binds the type and stops, so
/// the marks are never applied. Both tests read syntax alone, so the answer is
/// the same either way; only whether anything asks was in question, and that is
/// a fact about how far the batch had run.
pub(in crate::compilation::analyzer) fn mark_resolved_member_assignment(
    db: &mut DbIndex,
    member_id: LuaMemberId,
) {
    if !is_assignment_file_define_member(db, member_id) {
        return;
    }
    if is_guarded_table_assignment_member(db, member_id)
        || is_member_assignment_in_conditional_branch(db, member_id)
    {
        db.get_member_index_mut()
            .mark_non_overwriting_assignment_member(member_id);
    }
}

/// Whether this write sits inside a branch or loop body, so it may not run
/// and does not overwrite the writers before it in the same scope.
pub fn is_member_assignment_in_conditional_branch(db: &DbIndex, member_id: LuaMemberId) -> bool {
    let Some(node) = member_write_node(db, member_id) else {
        return false;
    };

    node.ancestors().any(|ancestor| {
        matches!(
            ancestor.kind().into(),
            glua_parser::LuaSyntaxKind::IfStat
                | glua_parser::LuaSyntaxKind::ElseIfClauseStat
                | glua_parser::LuaSyntaxKind::ElseClauseStat
                | glua_parser::LuaSyntaxKind::WhileStat
                | glua_parser::LuaSyntaxKind::RepeatStat
                | glua_parser::LuaSyntaxKind::ForStat
                | glua_parser::LuaSyntaxKind::ForRangeStat
        )
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
        .is_some_and(crate::LuaMember::is_assignment_define)
}

pub(in crate::compilation::analyzer) fn is_guarded_table_assignment_member(
    db: &crate::DbIndex,
    member_id: LuaMemberId,
) -> bool {
    member_write_node(db, member_id)
        .and_then(|node| guarded_bootstrap_range_for_node(node, false))
        .is_some()
}

/// The node that wrote `member_id`. The checks that ask look only at it, the
/// statement holding it and the kinds of its ancestors, so only that is read.
fn member_write_node(db: &DbIndex, member_id: LuaMemberId) -> Option<glua_parser::LuaSyntaxNode> {
    let syntax_id = member_id.get_syntax_id();
    let tree = db
        .get_vfs()
        .syntax_tree_at_statement(&member_id.file_id, syntax_id.get_range())?;
    syntax_id.to_node_from_root(&tree.get_red_root())
}

pub(in crate::compilation::analyzer) fn is_guarded_table_assignment_index_expr(
    index_expr: &LuaIndexExpr,
) -> bool {
    guarded_table_assignment_bootstrap_range(index_expr, false).is_some()
}

pub(crate) fn is_guarded_table_definition_site(
    db: &DbIndex,
    site: &InFiled<rowan::TextRange>,
) -> bool {
    let Some(tree) = db.get_vfs().syntax_tree_around(&site.file_id, site.value) else {
        return false;
    };
    let root = tree.get_red_root();
    let mut node = match root.covering_element(site.value) {
        rowan::NodeOrToken::Node(node) => Some(node),
        rowan::NodeOrToken::Token(token) => token.parent(),
    };

    while let Some(current) = node {
        if current.text_range() == site.value && LuaTableExpr::cast(current.clone()).is_some() {
            node = Some(current);
            break;
        }
        if !current.text_range().contains_range(site.value) {
            return false;
        }
        node = current.parent();
    }

    let Some(mut node) = node else {
        return false;
    };
    while let Some(parent) = node.parent() {
        if LuaClosureExpr::cast(parent.clone()).is_some() {
            return false;
        }
        if let Some(binary_expr) = LuaBinaryExpr::cast(parent.clone())
            && let Some(assign_stat) = binary_expr.get_parent::<LuaAssignStat>()
        {
            let (var_list, expr_list) = assign_stat.get_var_and_expr_list();
            let access_path = var_list
                .iter()
                .zip(expr_list.iter())
                .find(|(_, expr)| expr.get_syntax_id() == binary_expr.get_syntax_id())
                .and_then(|(var, _)| var.get_access_path());
            if access_path.is_some_and(|access_path| {
                guarded_assignment_table_arm_range(
                    &LuaExpr::BinaryExpr(binary_expr),
                    &access_path,
                    false,
                )
                .is_some_and(|guarded_range| guarded_range.contains_range(site.value))
            }) {
                return true;
            }
        }
        node = parent;
    }

    false
}

/// Range of the table a guarded bootstrap of this member names, whichever of
/// the two shapes wrote it.
fn guarded_bootstrap_range_for_node(
    node: glua_parser::LuaSyntaxNode,
    empty_only: bool,
) -> Option<rowan::TextRange> {
    if let Some(index_expr) = LuaIndexExpr::cast(node.clone()) {
        return guarded_table_assignment_bootstrap_range(&index_expr, empty_only);
    }

    guarded_table_literal_field_range(&LuaTableField::cast(node)?, empty_only)
}

/// Range of the table a field of a guarded table literal names.
///
/// `x = x or { y = {} }` creates `x.y` on exactly the condition `x.y = x.y or
/// {}` does — the namespace not existing yet — so the two shapes bootstrap the
/// same slot and have to resolve to one literal between them. Treating only the
/// second as a guarded writer leaves the first looking like a plain write, which
/// makes the whole slot ineligible for canonicalisation.
fn guarded_table_literal_field_range(
    table_field: &LuaTableField,
    empty_only: bool,
) -> Option<rowan::TextRange> {
    let LuaExpr::TableExpr(value) = table_field.get_value_expr()? else {
        return None;
    };
    if empty_only && !value.is_empty() {
        return None;
    }

    let table_expr = LuaTableExpr::cast(table_field.syntax().parent()?)?;
    let binary_expr = LuaBinaryExpr::cast(table_expr.syntax().parent()?)?;
    let assign_stat = binary_expr.get_parent::<LuaAssignStat>()?;
    let (var_list, expr_list) = assign_stat.get_var_and_expr_list();
    let access_path = var_list
        .iter()
        .zip(expr_list.iter())
        .find(|(_, expr)| expr.get_syntax_id() == binary_expr.get_syntax_id())
        .and_then(|(var, _)| var.get_access_path())?;
    guarded_assignment_table_arm_range(&LuaExpr::BinaryExpr(binary_expr), &access_path, false)?;

    Some(value.get_range())
}

/// Range of the table arm of a self-referential guarded bootstrap (`x.y =
/// x.y or {}`), which is what the assignment's type is when the guard falls
/// through.
fn guarded_table_assignment_bootstrap_range(
    index_expr: &LuaIndexExpr,
    empty_only: bool,
) -> Option<rowan::TextRange> {
    let var = LuaVarExpr::cast(index_expr.syntax().clone())?;
    let access_path = var.get_access_path()?;
    let assign_stat = index_expr.get_parent::<LuaAssignStat>()?;
    let syntax_id = index_expr.get_syntax_id();
    let (var_list, expr_list) = assign_stat.get_var_and_expr_list();

    var_list
        .iter()
        .zip(expr_list.iter())
        .find(|(candidate_var, _)| candidate_var.get_syntax_id() == syntax_id)
        .and_then(|(_, expr)| guarded_assignment_table_arm_range(expr, &access_path, empty_only))
}

fn guarded_assignment_table_arm_range(
    expr: &LuaExpr,
    access_path: &str,
    empty_only: bool,
) -> Option<rowan::TextRange> {
    let LuaExpr::BinaryExpr(binary_expr) = expr else {
        return None;
    };
    if binary_expr.get_op_token().map(|op| op.get_op()) != Some(BinaryOperator::OpOr) {
        return None;
    }

    let (left, right) = binary_expr.get_exprs()?;
    let LuaExpr::TableExpr(table_expr) = &right else {
        return None;
    };
    if empty_only && !table_expr.is_empty() {
        return None;
    }

    let left_path = LuaVarExpr::cast(left.syntax().clone())?.get_access_path()?;
    (left_path == access_path).then(|| table_expr.get_range())
}

/// Range of the table arm of the nearest `t[k] = t[k] or {}` bootstrap that
/// dominates a *read* of the same access path in the same function.
pub(crate) fn dominating_guarded_table_bootstrap_range(
    read_index_expr: &LuaIndexExpr,
) -> Option<rowan::TextRange> {
    let access_path = LuaVarExpr::cast(read_index_expr.syntax().clone())?.get_access_path()?;
    let read_start = read_index_expr.get_range().start();
    let mut node = read_index_expr.syntax().clone();

    while let Some(parent) = node.parent() {
        if LuaClosureExpr::cast(parent.clone()).is_some() {
            return None;
        }

        // Only the *nearest* preceding write decides: a plain write between the
        // bootstrap and the read supersedes it, so bail rather than answer.
        let nearest_write = parent
            .children()
            .take_while(|sibling| sibling.text_range().end() <= read_start)
            .filter_map(LuaAssignStat::cast)
            .filter_map(|assign_stat| {
                let (var_list, expr_list) = assign_stat.get_var_and_expr_list();
                var_list
                    .iter()
                    .zip(expr_list.iter())
                    .filter(|(var, _)| {
                        var.get_access_path().as_deref() == Some(access_path.as_str())
                    })
                    .map(|(_, expr)| guarded_assignment_table_arm_range(expr, &access_path, true))
                    .next_back()
            })
            .last();
        if let Some(bootstrap_range) = nearest_write {
            return bootstrap_range;
        }

        node = parent;
    }

    None
}

/// The type a self-referential guarded bootstrap (`x.y = x.y or {}`)
/// assigns, derived from syntax alone.
fn guarded_table_bootstrap_member_type(
    db: &crate::DbIndex,
    member_id: LuaMemberId,
    empty_only: bool,
) -> Option<LuaType> {
    let range = guarded_table_bootstrap_range(db, member_id, empty_only)?;

    Some(LuaType::TableConst(InFiled::new(member_id.file_id, range)))
}

fn guarded_table_bootstrap_range(
    db: &crate::DbIndex,
    member_id: LuaMemberId,
    empty_only: bool,
) -> Option<rowan::TextRange> {
    guarded_bootstrap_range_for_node(member_write_node(db, member_id)?, empty_only)
}

/// Every writer of this member's slot when all of them are `x.y = x.y or {}`
/// guards.
///
/// Walks the history slot unsorted: the caller only takes its minimum. Bails
/// on the first non-guard, so the outcome never depends on the walk order.
fn guarded_table_assignment_member_ids_for_owner_key(
    db: &DbIndex,
    member_id: LuaMemberId,
) -> Option<Vec<LuaMemberId>> {
    let member_index = db.get_member_index();
    let owner = member_index.get_member_owner(&member_id)?.clone();
    let key = member_index.get_member(&member_id)?.get_key().clone();
    let mut member_ids = Vec::new();

    for related_member_id in
        member_index.get_current_owner_member_ids_for_key_unsorted(&owner, &key)
    {
        if !is_guarded_table_assignment_member(db, related_member_id) {
            return None;
        }

        member_ids.push(related_member_id);
    }

    (member_ids.len() >= 2).then_some(member_ids)
}

/// The one table a repeated `x.y = x.y or {}` guard names.
///
/// Every such writer means "reuse it if it is there", so at runtime they are all
/// the same table and only the first to run creates it. Giving each writer its
/// own literal instead makes a file that re-guards the namespace read its own
/// empty table and lose whatever another file attached, so they resolve to the
/// earliest writer's literal — the one that would have won at runtime.
pub(in crate::compilation::analyzer) fn canonical_guarded_table_bootstrap_type(
    db: &crate::DbIndex,
    member_id: LuaMemberId,
) -> Option<LuaType> {
    // No second guard filter: the walk above only returns when every writer
    // passed, so filtering again would re-walk each member's syntax.
    let canonical = guarded_table_assignment_member_ids_for_owner_key(db, member_id)?
        .into_iter()
        .min_by_key(|candidate| member_id_sort_key(*candidate))?;

    guarded_table_bootstrap_member_type(db, canonical, false)
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

            analyzer.context.add_unresolve(unresolve_decl.into());
        }
        LuaTypeOwner::Member(member_id) => {
            let unresolve_member = UnResolveMember {
                file_id: analyzer.file_id,
                member_id,
                expr: Some(expr.clone()),
                prefix: None,
                ret_idx: idx,
            };
            analyzer.context.add_unresolve(unresolve_member.into());
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
    analyzer.write(StatementWrite::WriteTypeCache {
        owner: type_owner,
        cache: LuaTypeCache::InferType(signature_type.clone()),
        mode: TypeCacheWriteMode::InsertOnly,
    });

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
    analyzer.write(StatementWrite::WriteTypeCache {
        owner: decl_id.into(),
        cache: LuaTypeCache::InferType(signature_type.clone()),
        mode: TypeCacheWriteMode::InsertOnly,
    });

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
    let Ok(member_key) = LuaMemberKey::from_index_key(&analyzer.db, cache, &field_key) else {
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
    analyzer.write(StatementWrite::AddMember {
        owner: owner_id,
        member,
    });
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

    let trailing_spread = field.is_trailing_spread();
    if field.is_assign_field() || is_shaped_array_value_field(&field) || trailing_spread {
        let value_expr = field.get_value_expr()?;
        let member_id = LuaMemberId::new(field.get_syntax_id(), analyzer.file_id);
        let inferred = analyzer.infer_expr(&value_expr.clone());
        let value_type = match inferred {
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
            Err(_) => {
                let unresolve = UnResolveMember {
                    file_id: analyzer.file_id,
                    member_id,
                    expr: Some(value_expr.clone()),
                    prefix: None,
                    ret_idx: 0,
                };

                analyzer.context.add_unresolve(unresolve.into());
                return None;
            }
        };
        if trailing_spread && !matches!(value_type, LuaType::Variadic(_)) {
            return None;
        }
        // A table-valued field of a guarded `x = x or { ... }` literal
        // bootstraps the same member `x.y = x.y or {}` does: when every writer
        // of the slot is a guard they all name one table — the earliest
        // writer's literal — so the field binds that literal, not its own.
        // Binding the field literal instead leaves whichever file re-walked
        // last in the cache, and reads taken between the writes disagree with
        // the settled answer.
        let value_type = if is_guarded_table_assignment_member(&analyzer.db, member_id) {
            canonical_guarded_table_bootstrap_type(&analyzer.db, member_id).unwrap_or(value_type)
        } else {
            value_type
        };
        analyzer.write(StatementWrite::BindType {
            owner: member_id.into(),
            cache: LuaTypeCache::InferType(value_type),
        });
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
    let (left, right) = self_default_arms(&var, &expr)?;
    let LuaExpr::BinaryExpr(binary_expr) = expr else {
        return None;
    };

    let guarded_table_expr = matches!(&right, LuaExpr::TableExpr(_));
    let expr_type = if !guarded_table_expr {
        set_index_expr_owner(analyzer, var.clone());
        analyzer.infer_expr(&right)
    } else if defines_own_slot(analyzer, &left) {
        // The left read is the slot this statement defines, which cannot
        // inform its own definition: whatever it holds is written back
        // unchanged, so the statement contributes only its right arm.
        analyzer.infer_expr(&right)
    } else {
        analyzer.infer_expr(&LuaExpr::BinaryExpr(binary_expr.clone()))
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
                        analyzer.write(StatementWrite::AddInferredStringDefault {
                            file_id: analyzer.file_id,
                            decl_id: *decl_id,
                            value: smol_str::SmolStr::new(string_value),
                            range: assign_stat_range,
                        });
                    }
                }
            }

            assign_merge_type_owner_and_expr_type(
                analyzer,
                type_owner,
                &expr_type,
                0,
                guarded_table_expr,
                DeclWrite {
                    position: assign_stat_range.start(),
                    may_improve_after_resolve: false,
                    reads_out_of_decl: false,
                    may_narrow_uninformative: false,
                    resolved_initializer: false,
                    // `x = x or <default>`, matched above.
                    fills_own_default: true,
                },
            );
        }
        Err(_) => return None,
    }

    Some(())
}

/// Whether `left`, the read of `x = x or {…}`, is the shared slot the
/// statement defines: a field or a global. A local or parameter makes the
/// statement a default over the value it already holds.
fn defines_own_slot(analyzer: &LuaAnalyzer, left: &LuaExpr) -> bool {
    match left {
        LuaExpr::IndexExpr(_) => true,
        LuaExpr::NameExpr(name) => name.get_name_text().is_some_and(|text| {
            analyzer
                .db
                .get_decl_index()
                .get_decl_tree(&analyzer.file_id)
                .and_then(|tree| tree.find_local_decl(&text, name.get_position()))
                .is_none_or(|decl| !(decl.is_local() || decl.is_param()))
        }),
        _ => false,
    }
}

fn set_guarded_table_assignment_index_owner(
    analyzer: &mut LuaAnalyzer,
    var: LuaVarExpr,
    left: &LuaExpr,
) -> Option<()> {
    match var {
        LuaVarExpr::IndexExpr(index_expr) => {
            if let Some((owner, set_owner_only)) = guarded_table_assignment_info(analyzer, left) {
                apply_index_expr_member_owner_with_guarded(
                    analyzer,
                    index_expr,
                    owner,
                    set_owner_only,
                    true,
                )
            } else {
                set_index_expr_owner(analyzer, LuaVarExpr::IndexExpr(index_expr))
            }
        }
        other => set_index_expr_owner(analyzer, other),
    }
}

fn guarded_table_assignment_info(
    analyzer: &mut LuaAnalyzer,
    left: &LuaExpr,
) -> Option<(LuaMemberOwner, bool)> {
    let left_var = LuaVarExpr::cast(left.syntax().clone())?;
    let LuaVarExpr::IndexExpr(index_expr) = left_var else {
        return None;
    };
    let prefix_expr = index_expr.get_prefix_expr()?;
    let (owner, set_owner_only) = direct_local_table_prefix_member_owner(analyzer, &prefix_expr)
        .map(|owner| (owner, false))
        .or_else(|| {
            let prefix_type = analyzer.infer_expr(&prefix_expr).ok()?;
            resolve_index_expr_member_owner_for_file(
                &analyzer.db,
                &prefix_type,
                Some(analyzer.file_id),
            )
        })?;
    let index_key = index_expr.get_index_key()?;
    let cache = analyzer
        .context
        .infer_manager
        .get_infer_cache(analyzer.file_id);
    let key = LuaMemberKey::from_index_key_or_unknown(&analyzer.db, cache, &index_key).ok()?;
    (!key.is_expr()).then_some((owner, set_owner_only))
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

// Get the declaration id of the deferred definition
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

    use crate::{DbIndex, FileId, InFiled, LuaMergedTableType, LuaUnionType};

    use super::*;

    /// Every assignment of a local produces the local's slot, the ones whose
    /// value the slot did not take included, so the local's writers run as
    /// one group on every run.
    #[test]
    fn every_assignment_of_a_local_produces_its_slot() {
        use crate::db_index::derivation::{DerivationId, DerivationKind, FactKey};

        let mut ws = crate::VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/autorun/local_writes.lua",
            r#"
            local torque = 1
            if brake then
                torque = torque + 2
            else
                torque = unknown_source
            end
            "#,
        );
        let db = ws.analysis.compilation.get_db();
        let root = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("syntax tree")
            .get_chunk_node();
        let decl_id = root
            .descendants::<glua_parser::LuaLocalName>()
            .find_map(|name| {
                db.get_decl_index()
                    .get_decl(&crate::LuaDeclId::new(file_id, name.get_position()))
                    .map(|decl| decl.get_id())
            })
            .expect("the local");
        let slot = FactKey::TypeCache(crate::LuaTypeOwner::Decl(decl_id));
        let producing = root
            .descendants::<glua_parser::LuaAssignStat>()
            .map(|assign| {
                let id = DerivationId::new(
                    DerivationKind::AssignStat,
                    file_id,
                    LuaSyntaxId::from_node(assign.syntax()),
                );
                db.reader_graph()
                    .outputs_of(id)
                    .any(|key| key.key() == &slot)
            })
            .collect::<Vec<_>>();

        assert_eq!(producing, vec![true, true]);
    }

    fn table_const(start: u32, end: u32) -> LuaType {
        LuaType::TableConst(InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(start), TextSize::new(end)),
        ))
    }

    /// A sibling assignment that has not been analysed carries no type cache, so
    /// the cross-file merge can only keep it by deriving its type from syntax.
    /// Only the self-referential bootstrap has a syntax-determined type.
    #[test]
    fn guarded_table_bootstrap_range_names_only_the_self_referential_arm() {
        let source = "lib.store = lib.store or {}\nlib.other = fetch() or {}\n";
        let tree = glua_parser::LuaParser::parse(source, glua_parser::ParserConfig::default());

        let ranges = tree
            .get_chunk_node()
            .descendants::<LuaIndexExpr>()
            .filter_map(|index_expr| guarded_table_assignment_bootstrap_range(&index_expr, false))
            .collect::<Vec<_>>();

        assert_eq!(ranges.len(), 1, "only the bootstrap assignment qualifies");
        assert_eq!(&source[ranges[0]], "{}");
    }

    fn member_id_at(start: u32) -> LuaMemberId {
        member_id_at_file(FileId::new(0), start)
    }

    fn member_id_at_file(file_id: FileId, start: u32) -> LuaMemberId {
        let range = TextRange::new(TextSize::new(start), TextSize::new(start + 1));
        LuaMemberId::new(
            LuaSyntaxId::new(glua_parser::LuaSyntaxKind::IndexExpr.into(), range),
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

    fn with_analyzer<T>(db: &DbIndex, run: impl FnOnce(&mut LuaAnalyzer<'_>) -> T) -> T {
        with_analyzer_config(db, false, run)
    }

    fn with_analyzer_config<T>(
        db: &DbIndex,
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
    fn merged_bootstrap_literals_are_one_owner() {
        let typ = LuaMergedTableType::new(vec![table_const(1, 2), table_const(3, 4)]).into();

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

            let writes = with_analyzer(&db, |analyzer| {
                for additional_base in order {
                    record_pending_dynamic_key_collection_widening(
                        analyzer,
                        owner.clone(),
                        LuaMemberKey::ExprType(LuaType::String),
                        additional_base,
                    );
                }
                flush_pending_dynamic_key_collection_widenings(analyzer);
                std::mem::take(&mut analyzer.log)
            });
            for write in &writes {
                write.apply(&mut db);
            }

            db.get_type_index()
                .get_type_cache(&LuaTypeOwner::Member(member_id))
                .expect("flushed member type")
                .as_type()
                .clone()
        }

        let string_then_boolean = flushed_type_for(&[LuaType::String, LuaType::Boolean]);
        let boolean_then_string = flushed_type_for(&[LuaType::Boolean, LuaType::String]);

        assert_eq!(string_then_boolean, boolean_then_string);
    }

    /// The classification reads the write's syntax: a computed key the walk
    /// resolved to a name is still a computed key.
    #[test]
    fn expr_key_members_are_detected_as_dynamic_assignments() {
        let mut ws = crate::VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/keys.lua",
            "local t = {}\nlocal k = \"named\"\nt[k] = 1\nt.plain = 2\n",
        );
        let db = ws.analysis.compilation.get_db_mut();
        let chunk = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("the file's tree")
            .get_chunk_node();
        let writes = chunk
            .descendants::<LuaIndexExpr>()
            .map(|index_expr| {
                (
                    index_expr.syntax().text().to_string(),
                    LuaMemberId::new(index_expr.get_syntax_id(), file_id),
                )
            })
            .collect::<Vec<_>>();
        let computed = writes
            .iter()
            .find(|(text, _)| text == "t[k]")
            .expect("t[k]")
            .1;
        let plain = writes
            .iter()
            .find(|(text, _)| text == "t.plain")
            .expect("t.plain")
            .1;

        with_analyzer(db, |analyzer| {
            assert!(is_dynamic_expr_key_member_assignment(
                analyzer,
                &LuaTypeOwner::Member(computed)
            ));
            assert!(!is_dynamic_expr_key_member_assignment(
                analyzer,
                &LuaTypeOwner::Member(plain)
            ));
        });
    }

    /// A write of a local through an alias of itself (`local id = data;
    /// data = id`) derives its type from the slot it fills, like a direct read
    /// of it: it reads out of the decl.
    #[test]
    fn decl_assignment_through_a_local_alias_of_itself_reads_out_of_it() {
        let mut ws = crate::VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/autorun/alias-self-read.lua",
            r#"
            local function handle(read, key)
                local data = read()
                if key then
                    local id = data
                    local first = data[1]
                    data = id
                    data = first
                end
                return data
            end
            "#,
        );

        let db = ws.analysis.compilation.get_db();
        let root = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("syntax tree")
            .get_chunk_node();
        let references = db
            .get_reference_index()
            .get_local_reference(&file_id)
            .expect("local references");
        let data_decl = root
            .descendants::<glua_parser::LuaNameExpr>()
            .find(|name| name.get_name_text().as_deref() == Some("data"))
            .and_then(|name| references.get_decl_id(&name.get_range()))
            .expect("data decl");
        let assigned = root
            .descendants::<glua_parser::LuaAssignStat>()
            .filter_map(|assign| assign.get_var_and_expr_list().1.into_iter().next())
            .collect::<Vec<_>>();

        assert_eq!(assigned.len(), 2);
        assert!(
            assigned
                .iter()
                .all(|expr| { super::expr_reads_out_of_decl(db, file_id, data_decl, expr) })
        );
    }
}
