use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use crate::{
    DbIndex, FileId, InferFailReason, LuaFunctionType, LuaSemanticDeclId, LuaType, TypeOps,
    db_index::gmod_infer::GmodRealm,
};
use glua_parser::{BinaryOperator, LuaAssignStat, LuaAstNode, LuaExpr, PathTrait};
use rowan::TextSize;

use super::LuaMemberId;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LuaMemberIndexItem {
    One(LuaMemberId),
    Many(Vec<LuaMemberId>),
}

impl LuaMemberIndexItem {
    pub fn resolve_type(&self, db: &DbIndex) -> Result<LuaType, InferFailReason> {
        resolve_member_type(db, self)
    }

    pub fn resolve_type_with_realm(
        &self,
        db: &DbIndex,
        caller_file_id: &FileId,
    ) -> Result<LuaType, InferFailReason> {
        resolve_member_type_with_realm(db, self, caller_file_id)
    }

    pub fn resolve_type_with_realm_at_offset(
        &self,
        db: &DbIndex,
        caller_file_id: &FileId,
        caller_position: TextSize,
    ) -> Result<LuaType, InferFailReason> {
        resolve_member_type_with_realm_at_offset(db, self, caller_file_id, caller_position)
    }

    pub fn resolve_semantic_decl(&self, db: &DbIndex) -> Option<LuaSemanticDeclId> {
        resolve_member_semantic_id(db, self)
    }

    pub fn resolve_semantic_decl_with_realm(
        &self,
        db: &DbIndex,
        caller_file_id: &FileId,
    ) -> Option<LuaSemanticDeclId> {
        resolve_member_semantic_id_with_realm(db, self, caller_file_id)
    }

    pub fn resolve_semantic_decl_with_realm_at_offset(
        &self,
        db: &DbIndex,
        caller_file_id: &FileId,
        caller_position: TextSize,
    ) -> Option<LuaSemanticDeclId> {
        resolve_member_semantic_id_with_realm_at_offset(db, self, caller_file_id, caller_position)
    }

    #[allow(unused)]
    pub fn resolve_type_owner_member_id(&self, db: &DbIndex) -> Option<LuaMemberId> {
        resolve_type_owner_member_id(db, self)
    }

    pub fn is_one(&self) -> bool {
        matches!(self, LuaMemberIndexItem::One(_))
    }

    pub fn visible_member_ids_with_realm(
        &self,
        db: &DbIndex,
        caller_file_id: &FileId,
    ) -> Vec<LuaMemberId> {
        let member_ids = self.get_member_ids();
        let priority_tiers = get_member_id_priority_tiers(db, caller_file_id, &member_ids);
        select_member_ids_by_workspace_and_realm(
            db,
            caller_file_id,
            priority_tiers,
            infer_caller_file_realm(db, caller_file_id),
        )
    }

    pub fn visible_member_ids_with_realm_at_offset(
        &self,
        db: &DbIndex,
        caller_file_id: &FileId,
        caller_position: TextSize,
    ) -> Vec<LuaMemberId> {
        if let LuaMemberIndexItem::One(member_id) = self {
            if let Some(visible_member_id) = visible_single_member_id_with_realm_at_offset(
                db,
                *member_id,
                caller_file_id,
                caller_position,
            ) {
                return vec![visible_member_id];
            }
        }

        let member_ids = self.get_member_ids();
        let mut visible_member_ids =
            visible_member_ids_at_offset(db, &member_ids, caller_file_id, caller_position);
        if visible_member_ids.is_empty()
            || should_expand_function_assignment_history(
                db,
                &visible_member_ids,
                caller_file_id,
                caller_position,
            )
        {
            let historical_member_ids =
                expand_member_ids_with_owner_key_history(db, member_ids.clone());
            if historical_member_ids != member_ids {
                visible_member_ids = visible_member_ids_at_offset(
                    db,
                    &historical_member_ids,
                    caller_file_id,
                    caller_position,
                );
            }
        }
        let priority_tiers = get_member_id_priority_tiers(db, caller_file_id, &visible_member_ids);
        select_member_ids_by_workspace_and_realm(
            db,
            caller_file_id,
            priority_tiers,
            db.get_gmod_infer_index()
                .get_realm_at_offset(caller_file_id, caller_position),
        )
    }

    pub fn visible_member_ids_with_realm_at_offset_from_history(
        &self,
        db: &DbIndex,
        caller_file_id: &FileId,
        caller_position: TextSize,
    ) -> Vec<LuaMemberId> {
        let member_ids = self.get_member_ids();
        let visible_member_ids =
            visible_member_ids_at_offset(db, &member_ids, caller_file_id, caller_position);
        let priority_tiers = get_member_id_priority_tiers(db, caller_file_id, &visible_member_ids);
        select_member_ids_by_workspace_and_realm(
            db,
            caller_file_id,
            priority_tiers,
            db.get_gmod_infer_index()
                .get_realm_at_offset(caller_file_id, caller_position),
        )
    }

    pub fn get_member_ids(&self) -> Vec<LuaMemberId> {
        match self {
            LuaMemberIndexItem::One(member_id) => vec![*member_id],
            LuaMemberIndexItem::Many(member_ids) => member_ids.clone(),
        }
    }
}

fn visible_single_member_id_with_realm_at_offset(
    db: &DbIndex,
    member_id: LuaMemberId,
    caller_file_id: &FileId,
    caller_position: TextSize,
) -> Option<LuaMemberId> {
    if !member_visible_at_offset(db, member_id, caller_file_id, caller_position)
        || should_expand_function_assignment_history(
            db,
            &[member_id],
            caller_file_id,
            caller_position,
        )
    {
        return None;
    }

    let module_index = db.get_module_index();
    if let Some(caller_workspace_id) = module_index.get_workspace_id(*caller_file_id) {
        let candidate_workspace_id = module_index
            .get_workspace_id(member_id.file_id)
            .unwrap_or(crate::WorkspaceId::MAIN);
        module_index.workspace_resolution_priority(caller_workspace_id, candidate_workspace_id)?;
    }

    if !db.get_emmyrc().gmod.enabled {
        return Some(member_id);
    }

    let caller_realm = db
        .get_gmod_infer_index()
        .get_realm_at_offset(caller_file_id, caller_position);
    let member_realm = member_effective_realm(db.get_gmod_infer_index(), &member_id);
    if is_realm_compatible(caller_realm, member_realm) {
        Some(member_id)
    } else {
        None
    }
}

fn should_expand_function_assignment_history(
    db: &DbIndex,
    member_ids: &[LuaMemberId],
    caller_file_id: &FileId,
    caller_position: TextSize,
) -> bool {
    member_ids.iter().copied().any(|member_id| {
        is_function_scoped_assignment_file_define(db, member_id)
            && !member_assignment_shares_enclosing_function(
                db,
                member_id,
                caller_file_id,
                caller_position,
            )
    })
}

fn is_function_scoped_assignment_file_define(db: &DbIndex, member_id: LuaMemberId) -> bool {
    let Some(member) = db.get_member_index().get_member(&member_id) else {
        return false;
    };
    member.get_feature().is_file_define()
        && member.get_syntax_id().get_kind() == glua_parser::LuaSyntaxKind::IndexExpr
        && db
            .get_member_index()
            .member_function_scope_range(member_id)
            .is_some()
}

fn member_assignment_shares_enclosing_function(
    db: &DbIndex,
    member_id: LuaMemberId,
    caller_file_id: &FileId,
    caller_position: TextSize,
) -> bool {
    if member_id.file_id != *caller_file_id {
        return false;
    }

    let member_function = db.get_member_index().member_function_scope_range(member_id);
    let caller_function = db
        .get_member_index()
        .enclosing_function_scope_range(*caller_file_id, caller_position);

    member_function.is_some() && member_function == caller_function
}

fn visible_member_ids_at_offset(
    db: &DbIndex,
    member_ids: &[LuaMemberId],
    caller_file_id: &FileId,
    caller_position: TextSize,
) -> Vec<LuaMemberId> {
    member_ids
        .iter()
        .copied()
        .filter(|member_id| {
            member_visible_at_offset(db, *member_id, caller_file_id, caller_position)
        })
        .collect()
}

fn expand_member_ids_with_owner_key_history(
    db: &DbIndex,
    member_ids: Vec<LuaMemberId>,
) -> Vec<LuaMemberId> {
    let mut expanded = Vec::new();
    let mut seen = HashSet::new();
    let member_index = db.get_member_index();

    for member_id in member_ids {
        push_unique_member_id(&mut expanded, &mut seen, member_id);

        let Some(member) = member_index.get_member(&member_id) else {
            continue;
        };
        let Some(owner) = member_index.get_current_owner(&member_id) else {
            continue;
        };

        for historical_member in
            member_index.get_current_owner_members_for_key(owner, member.get_key())
        {
            push_unique_member_id(&mut expanded, &mut seen, historical_member.get_id());
        }
    }

    expanded
}

fn push_unique_member_id(
    member_ids: &mut Vec<LuaMemberId>,
    seen: &mut HashSet<LuaMemberId>,
    member_id: LuaMemberId,
) {
    if seen.insert(member_id) {
        member_ids.push(member_id);
    }
}

fn member_visible_at_offset(
    db: &DbIndex,
    member_id: LuaMemberId,
    caller_file_id: &FileId,
    caller_position: TextSize,
) -> bool {
    let Some(member) = db.get_member_index().get_member(&member_id) else {
        return false;
    };
    if member_id.file_id != *caller_file_id || !member.get_feature().is_file_define() {
        return true;
    }
    if !member.get_key().is_name() {
        return true;
    }

    let member_range = member.get_range();
    if member_range.contains(caller_position) {
        return true;
    }
    if member_range.start() > caller_position {
        return false;
    }

    !member_hidden_by_enclosing_assignment(db, member_id, caller_position)
}

fn member_hidden_by_enclosing_assignment(
    db: &DbIndex,
    member_id: LuaMemberId,
    caller_position: TextSize,
) -> bool {
    let Some(root) = db.get_vfs().get_syntax_tree(&member_id.file_id) else {
        return false;
    };
    let root = root.get_red_root();
    let Some(member) = db.get_member_index().get_member(&member_id) else {
        return false;
    };
    let member_range = member.get_range();
    let Some(token) = root.token_at_offset(member_range.start()).right_biased() else {
        return false;
    };

    token
        .parent_ancestors()
        .find_map(LuaAssignStat::cast)
        .is_some_and(|assign_stat| {
            assign_stat.get_range().contains(caller_position)
                && !member_range.contains(caller_position)
                && !assignment_rhs_self_coalesces_member(
                    &assign_stat,
                    member_range,
                    caller_position,
                )
        })
}

fn assignment_rhs_self_coalesces_member(
    assign_stat: &LuaAssignStat,
    member_range: rowan::TextRange,
    caller_position: TextSize,
) -> bool {
    let (vars, exprs) = assign_stat.get_var_and_expr_list();
    for (idx, var) in vars.iter().enumerate() {
        if var.get_range() != member_range {
            continue;
        }

        let Some(expr) = exprs.get(idx) else {
            return false;
        };
        let LuaExpr::BinaryExpr(binary_expr) = expr else {
            return false;
        };
        if binary_expr
            .get_op_token()
            .is_none_or(|token| token.get_op() != BinaryOperator::OpOr)
        {
            return false;
        }

        let Some((left_expr, _)) = binary_expr.get_exprs() else {
            return false;
        };
        if !left_expr.get_range().contains(caller_position) {
            return false;
        }

        return expr_access_path(&var.to_expr()) == expr_access_path(&left_expr);
    }

    false
}

fn expr_access_path(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::NameExpr(name_expr) => name_expr.get_access_path(),
        LuaExpr::IndexExpr(index_expr) => index_expr.get_access_path(),
        _ => None,
    }
}

fn resolve_member_type(
    db: &DbIndex,
    member_item: &LuaMemberIndexItem,
) -> Result<LuaType, InferFailReason> {
    match member_item {
        LuaMemberIndexItem::One(member_id) => {
            let member_type_cache = db.get_type_index().get_type_cache(&(*member_id).into());
            match member_type_cache {
                Some(cache) => Ok(cache.as_type().clone()),
                None => Err(InferFailReason::UnResolveMemberType(*member_id)),
            }
        }
        LuaMemberIndexItem::Many(member_ids) => {
            let mut resolve_state = MemberTypeResolveState::All;
            let mut members = vec![];
            for member_id in member_ids {
                if let Some(member) = db.get_member_index().get_member(member_id) {
                    members.push(member);
                } else {
                    return Err(InferFailReason::None);
                }
            }
            let all_file_defines = members
                .iter()
                .all(|member| member.get_feature().is_file_define());
            let should_prefer_doc_file_defines = all_file_defines
                && members.iter().any(|member| {
                    db.get_type_index()
                        .get_type_cache(&member.get_id().into())
                        .is_some_and(|cache| cache.is_doc())
                });
            let should_widen_file_defines =
                !should_prefer_doc_file_defines && members.len() > 1 && all_file_defines;
            let all_non_overwriting_assignment_file_defines = all_file_defines
                && members.iter().all(|member| {
                    db.get_member_index()
                        .is_non_overwriting_assignment_member(member.get_id())
                });
            let should_widen_table_literals = should_widen_file_defines
                && !all_non_overwriting_assignment_file_defines
                && members.iter().all(|member| {
                    db.get_type_index()
                        .get_type_cache(&member.get_id().into())
                        .is_some_and(|cache| {
                            cache.is_doc() || is_table_assignment_merge_type(cache.as_type())
                        })
                });
            if db.get_emmyrc().strict.meta_override_file_define {
                for member in &members {
                    let feature = member.get_feature();
                    if feature.is_meta_decl() {
                        resolve_state = MemberTypeResolveState::Meta;
                        break;
                    } else if feature.is_file_decl() {
                        resolve_state = MemberTypeResolveState::FileDecl;
                    }
                }
            }

            match resolve_state {
                MemberTypeResolveState::All => {
                    let mut typ = LuaType::Unknown;
                    for member in &members {
                        let member_type_cache = db
                            .get_type_index()
                            .get_type_cache(&member.get_id().into())
                            .ok_or(InferFailReason::UnResolveMemberType(member.get_id()))?;
                        if should_prefer_doc_file_defines && !member_type_cache.is_doc() {
                            continue;
                        }

                        let member_type = member_type_cache.as_type();
                        let member_type = if should_widen_file_defines {
                            widen_file_define_member_type(member_type, should_widen_table_literals)
                        } else {
                            member_type.clone()
                        };
                        typ = TypeOps::Union.apply(db, &typ, &member_type);
                    }
                    if let Some(adapters) =
                        build_generic_arity_adapters_for_overrides(db, &typ, &members)
                    {
                        let base_typ = prune_non_generic_callables_for_adapter_merge(db, &typ);
                        typ = TypeOps::Union.apply(db, &base_typ, &adapters);
                    }
                    if all_non_overwriting_assignment_file_defines
                        && !should_prefer_doc_file_defines
                    {
                        typ = crate::prune_redundant_guarded_table_bootstrap_type(db, typ);
                    }
                    Ok(typ)
                }
                MemberTypeResolveState::Meta => {
                    let mut typ = LuaType::Unknown;
                    for member in &members {
                        let feature = member.get_feature();
                        if feature.is_meta_decl() {
                            typ = TypeOps::Union.apply(
                                db,
                                &typ,
                                db.get_type_index()
                                    .get_type_cache(&member.get_id().into())
                                    .ok_or(InferFailReason::UnResolveMemberType(member.get_id()))?
                                    .as_type(),
                            );
                        }
                    }
                    if let Some(adapters) =
                        build_generic_arity_adapters_for_overrides(db, &typ, &members)
                    {
                        let base_typ = prune_non_generic_callables_for_adapter_merge(db, &typ);
                        typ = TypeOps::Union.apply(db, &base_typ, &adapters);
                    }

                    Ok(typ)
                }
                MemberTypeResolveState::FileDecl => {
                    let mut typ = LuaType::Unknown;
                    for member in &members {
                        let feature = member.get_feature();
                        if feature.is_file_decl() {
                            typ = TypeOps::Union.apply(
                                db,
                                &typ,
                                db.get_type_index()
                                    .get_type_cache(&member.get_id().into())
                                    .ok_or(InferFailReason::UnResolveMemberType(member.get_id()))?
                                    .as_type(),
                            );
                        }
                    }
                    if let Some(adapters) =
                        build_generic_arity_adapters_for_overrides(db, &typ, &members)
                    {
                        let base_typ = prune_non_generic_callables_for_adapter_merge(db, &typ);
                        typ = TypeOps::Union.apply(db, &base_typ, &adapters);
                    }
                    Ok(typ)
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct OverrideCallableShape {
    max_required_params: usize,
    has_variadic: bool,
    callable_member_count: usize,
    has_non_generic_callable: bool,
}

fn build_generic_arity_adapters_for_overrides(
    db: &DbIndex,
    typ: &LuaType,
    members: &[&crate::LuaMember],
) -> Option<LuaType> {
    let override_shape = collect_override_callable_shape(db, members);
    if override_shape.callable_member_count < 2 || !override_shape.has_non_generic_callable {
        return None;
    }
    if override_shape.max_required_params == 0 && !override_shape.has_variadic {
        return None;
    }

    let generic_callables = extract_generic_callables(db, typ);
    if generic_callables.is_empty() {
        return None;
    }

    let adapters = generic_callables
        .into_iter()
        .filter_map(|generic_callable| {
            if !override_shape.has_variadic
                && override_shape.max_required_params <= generic_callable.get_params().len()
            {
                return None;
            }

            let mut params = generic_callable.get_params().to_vec();
            while params.len() < override_shape.max_required_params {
                params.push(("__override_extra".to_string(), Some(LuaType::Any)));
            }

            Some(LuaType::DocFunction(Arc::new(LuaFunctionType::new(
                generic_callable.get_async_state(),
                generic_callable.is_colon_define(),
                generic_callable.is_variadic() || override_shape.has_variadic,
                params,
                generic_callable.get_ret().clone(),
            ))))
        })
        .collect::<Vec<_>>();

    if adapters.is_empty() {
        None
    } else {
        Some(LuaType::from_vec(adapters))
    }
}

fn collect_override_callable_shape(
    db: &DbIndex,
    members: &[&crate::LuaMember],
) -> OverrideCallableShape {
    fn type_has_callable(typ: &LuaType) -> bool {
        match typ {
            LuaType::DocFunction(_) | LuaType::Signature(_) => true,
            LuaType::Union(union_type) => union_type.types().any(type_has_callable),
            _ => false,
        }
    }

    fn type_has_non_generic_callable(db: &DbIndex, typ: &LuaType) -> bool {
        match typ {
            LuaType::DocFunction(function_type) => !function_type.contain_tpl(),
            LuaType::Signature(signature_id) => db
                .get_signature_index()
                .get(signature_id)
                .is_some_and(|signature| {
                    !signature.is_generic()
                        && !signature.has_special_call_params()
                        && !signature.to_doc_func_type().contain_tpl()
                }),
            LuaType::Union(union_type) => union_type
                .types()
                .any(|union_member| type_has_non_generic_callable(db, union_member)),
            _ => false,
        }
    }

    fn collect_from_type(db: &DbIndex, typ: &LuaType, shape: &mut OverrideCallableShape) {
        match typ {
            LuaType::DocFunction(function_type) => {
                if function_type.is_variadic() {
                    shape.has_variadic = true;
                } else {
                    shape.max_required_params = shape
                        .max_required_params
                        .max(function_type.get_params().len());
                }
            }
            LuaType::Signature(signature_id) => {
                if let Some(signature) = db.get_signature_index().get(signature_id) {
                    if signature.is_vararg {
                        shape.has_variadic = true;
                    } else {
                        shape.max_required_params = shape
                            .max_required_params
                            .max(signature.get_type_params().len());
                    }
                }
            }
            LuaType::Union(union_type) => {
                for union_member in union_type.types() {
                    collect_from_type(db, union_member, shape);
                }
            }
            _ => {}
        }
    }

    let mut shape = OverrideCallableShape::default();
    for member in members {
        let feature = member.get_feature();
        if !feature.is_file_decl() && !feature.is_file_define() {
            continue;
        }
        let Some(cache) = db.get_type_index().get_type_cache(&member.get_id().into()) else {
            continue;
        };
        let member_type = cache.as_type();
        if type_has_callable(member_type) {
            shape.callable_member_count += 1;
        }
        if type_has_non_generic_callable(db, member_type) {
            shape.has_non_generic_callable = true;
        }
        collect_from_type(db, member_type, &mut shape);
    }
    shape
}

fn extract_generic_callables(db: &DbIndex, typ: &LuaType) -> Vec<Arc<LuaFunctionType>> {
    fn collect(db: &DbIndex, typ: &LuaType, out: &mut Vec<Arc<LuaFunctionType>>) {
        match typ {
            LuaType::DocFunction(function_type) => {
                if function_type.contain_tpl() {
                    out.push(function_type.clone());
                }
            }
            LuaType::Signature(signature_id) => {
                if let Some(signature) = db.get_signature_index().get(signature_id) {
                    let function_type = Arc::new(
                        LuaFunctionType::new(
                            signature.async_state,
                            signature.is_colon_define,
                            signature.is_vararg,
                            signature.get_type_params(),
                            signature.get_return_type(),
                        )
                        .with_optional_params(signature.get_param_optional_flags()),
                    );
                    if signature.is_generic()
                        || signature.has_special_call_params()
                        || function_type.contain_tpl()
                    {
                        out.push(function_type);
                    }
                }
            }
            LuaType::Union(union_type) => {
                for union_member in union_type.types() {
                    collect(db, union_member, out);
                }
            }
            _ => {}
        }
    }

    let mut callables = Vec::new();
    collect(db, typ, &mut callables);
    callables
}

fn prune_non_generic_callables_for_adapter_merge(db: &DbIndex, typ: &LuaType) -> LuaType {
    fn is_generic_callable(db: &DbIndex, typ: &LuaType) -> bool {
        match typ {
            LuaType::DocFunction(function_type) => function_type.contain_tpl(),
            LuaType::Signature(signature_id) => db
                .get_signature_index()
                .get(signature_id)
                .is_some_and(|signature| {
                    signature.is_generic()
                        || signature.has_special_call_params()
                        || signature.to_doc_func_type().contain_tpl()
                }),
            _ => false,
        }
    }

    match typ {
        LuaType::Union(union_type) => {
            let pruned_members = union_type
                .types()
                .filter(|union_member| {
                    if matches!(
                        union_member,
                        LuaType::DocFunction(_) | LuaType::Signature(_)
                    ) {
                        is_generic_callable(db, union_member)
                    } else {
                        true
                    }
                })
                .cloned()
                .collect::<Vec<_>>();
            if pruned_members.is_empty() {
                typ.clone()
            } else {
                LuaType::from_vec(pruned_members)
            }
        }
        _ => typ.clone(),
    }
}

fn resolve_member_type_with_realm(
    db: &DbIndex,
    member_item: &LuaMemberIndexItem,
    caller_file_id: &FileId,
) -> Result<LuaType, InferFailReason> {
    let visible_member_ids = member_item.visible_member_ids_with_realm(db, caller_file_id);
    if visible_member_ids.is_empty() {
        return resolve_member_type(db, &LuaMemberIndexItem::Many(vec![]));
    }

    resolve_member_type(db, &member_item_from_ids(visible_member_ids))
}

fn resolve_member_type_with_realm_at_offset(
    db: &DbIndex,
    member_item: &LuaMemberIndexItem,
    caller_file_id: &FileId,
    caller_position: TextSize,
) -> Result<LuaType, InferFailReason> {
    let visible_member_ids =
        member_item.visible_member_ids_with_realm_at_offset(db, caller_file_id, caller_position);
    if visible_member_ids.is_empty() {
        return resolve_member_type(db, &LuaMemberIndexItem::Many(vec![]));
    }

    resolve_member_type(db, &member_item_from_ids(visible_member_ids))
}

fn resolve_member_semantic_id_with_realm(
    db: &DbIndex,
    member_item: &LuaMemberIndexItem,
    caller_file_id: &FileId,
) -> Option<LuaSemanticDeclId> {
    let visible_member_ids = member_item.visible_member_ids_with_realm(db, caller_file_id);

    resolve_member_semantic_id(db, &member_item_from_ids(visible_member_ids))
}

fn resolve_member_semantic_id_with_realm_at_offset(
    db: &DbIndex,
    member_item: &LuaMemberIndexItem,
    caller_file_id: &FileId,
    caller_position: TextSize,
) -> Option<LuaSemanticDeclId> {
    let visible_member_ids =
        member_item.visible_member_ids_with_realm_at_offset(db, caller_file_id, caller_position);

    resolve_member_semantic_id(db, &member_item_from_ids(visible_member_ids))
}

fn infer_caller_file_realm(db: &DbIndex, caller_file_id: &FileId) -> GmodRealm {
    db.get_gmod_infer_index()
        .get_realm_file_metadata(caller_file_id)
        .map(|metadata| metadata.inferred_realm)
        .unwrap_or(GmodRealm::Unknown)
}

fn get_member_id_priority_tiers(
    db: &DbIndex,
    caller_file_id: &FileId,
    member_ids: &[LuaMemberId],
) -> Vec<(u8, Vec<LuaMemberId>)> {
    let module_index = db.get_module_index();
    let Some(caller_workspace_id) = module_index.get_workspace_id(*caller_file_id) else {
        return vec![(0, member_ids.to_vec())];
    };

    let mut priority_tiers = BTreeMap::new();
    for member_id in member_ids {
        let candidate_workspace_id = module_index
            .get_workspace_id(member_id.file_id)
            .unwrap_or(crate::WorkspaceId::MAIN);
        let Some(priority) =
            module_index.workspace_resolution_priority(caller_workspace_id, candidate_workspace_id)
        else {
            continue;
        };

        priority_tiers
            .entry(priority)
            .or_insert_with(Vec::new)
            .push(*member_id);
    }

    priority_tiers.into_iter().collect()
}

fn select_member_ids_by_workspace_and_realm(
    db: &DbIndex,
    _caller_file_id: &FileId,
    priority_tiers: Vec<(u8, Vec<LuaMemberId>)>,
    caller_realm: GmodRealm,
) -> Vec<LuaMemberId> {
    if !db.get_emmyrc().gmod.enabled {
        return priority_tiers
            .first()
            .map(|(_, member_ids)| {
                let mut member_ids = member_ids.clone();
                sort_member_ids_for_caller(db, caller_realm, &mut member_ids);
                member_ids
            })
            .unwrap_or_default();
    }

    let fallback_member_ids = priority_tiers
        .first()
        .map(|(_, member_ids)| {
            let mut member_ids = member_ids.clone();
            sort_member_ids_for_caller(db, caller_realm, &mut member_ids);
            member_ids
        })
        .unwrap_or_default();

    let member_index = db.get_member_index();
    let infer_index = db.get_gmod_infer_index();

    let mut result = Vec::new();
    let mut found_first_compatible = false;
    let mut all_seen_are_non_meta = true;

    for (_, tier_member_ids) in priority_tiers {
        let compatible_member_ids = tier_member_ids
            .into_iter()
            .filter(|member_id| {
                let member_realm = member_effective_realm(infer_index, member_id);
                is_realm_compatible(caller_realm, member_realm)
            })
            .collect::<Vec<_>>();

        if compatible_member_ids.is_empty() {
            continue;
        }

        if !found_first_compatible {
            // First compatible tier: include all its realm-compatible members.
            let has_meta = compatible_member_ids.iter().any(|mid| {
                member_index
                    .get_member(mid)
                    .is_some_and(|m| m.get_feature().is_meta_decl())
            });
            result.extend(compatible_member_ids);
            found_first_compatible = true;
            all_seen_are_non_meta = !has_meta;
        } else if all_seen_are_non_meta {
            // We have only non-meta members so far. Supplement with any
            // meta (annotated) members from this lower-priority tier so that
            // resolve_member_type can prefer them via meta_override_file_define.
            let meta_members: Vec<_> = compatible_member_ids
                .into_iter()
                .filter(|mid| {
                    member_index
                        .get_member(mid)
                        .is_some_and(|m| m.get_feature().is_meta_decl())
                })
                .collect();
            if !meta_members.is_empty() {
                result.extend(meta_members);
                all_seen_are_non_meta = false;
            }
        }
    }

    if result.is_empty() && caller_realm.state_mask().is_empty() {
        return fallback_member_ids;
    }

    supplement_function_assignment_shape_members(db, &fallback_member_ids, &mut result);

    sort_member_ids_for_caller(db, caller_realm, &mut result);
    result
}

fn supplement_function_assignment_shape_members(
    db: &DbIndex,
    fallback_member_ids: &[LuaMemberId],
    result: &mut Vec<LuaMemberId>,
) {
    if result.is_empty()
        || !result
            .iter()
            .all(|member_id| member_type_is_uninformative(db, *member_id))
    {
        return;
    }

    let existing = result.iter().copied().collect::<HashSet<_>>();
    let supplements = fallback_member_ids
        .iter()
        .copied()
        .filter(|member_id| !existing.contains(member_id))
        .filter(|member_id| is_function_scoped_assignment_file_define(db, *member_id))
        .filter(|member_id| !member_type_is_uninformative(db, *member_id))
        .collect::<Vec<_>>();
    result.extend(supplements);
}

fn member_type_is_uninformative(db: &DbIndex, member_id: LuaMemberId) -> bool {
    db.get_type_index()
        .get_type_cache(&member_id.into())
        .is_none_or(|cache| type_is_uninformative(cache.as_type()))
}

fn type_is_uninformative(typ: &LuaType) -> bool {
    match typ {
        LuaType::Any | LuaType::Unknown | LuaType::Nil | LuaType::Never => true,
        LuaType::Union(union) => union.types().all(type_is_uninformative),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .all(|(typ, _)| type_is_uninformative(typ)),
        _ => false,
    }
}

fn sort_member_ids_for_caller(
    db: &DbIndex,
    caller_realm: GmodRealm,
    member_ids: &mut [LuaMemberId],
) {
    let infer_index = db.get_gmod_infer_index();
    member_ids.sort_by_key(|member_id| {
        let member_realm = member_effective_realm(infer_index, member_id);
        (
            realm_match_rank(caller_realm, member_realm),
            member_id.file_id.id,
            u32::from(member_id.get_position()),
            u32::from(member_id.get_syntax_id().get_range().end()),
            member_id.get_syntax_id().get_kind() as u16,
        )
    });
}

fn member_effective_realm(
    infer_index: &crate::GmodInferIndex,
    member_id: &LuaMemberId,
) -> GmodRealm {
    infer_index
        .get_member_annotation_realm_at_offset(&member_id.file_id, member_id.get_position())
        .unwrap_or_else(|| {
            infer_index.get_realm_at_offset(&member_id.file_id, member_id.get_position())
        })
}

fn realm_match_rank(caller_realm: GmodRealm, member_realm: GmodRealm) -> u8 {
    if caller_realm == member_realm {
        0
    } else if member_realm == GmodRealm::Unknown {
        2
    } else if caller_realm.is_compatible_with(member_realm) {
        1
    } else {
        3
    }
}

fn widen_file_define_member_type(typ: &LuaType, widen_table_literals: bool) -> LuaType {
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

fn member_item_from_ids(member_ids: Vec<LuaMemberId>) -> LuaMemberIndexItem {
    match member_ids.len() {
        0 => LuaMemberIndexItem::Many(vec![]),
        1 => LuaMemberIndexItem::One(member_ids[0]),
        _ => LuaMemberIndexItem::Many(member_ids),
    }
}

fn is_realm_compatible(call_realm: GmodRealm, decl_realm: GmodRealm) -> bool {
    call_realm.is_compatible_with(decl_realm)
}

fn resolve_type_owner_member_id(
    db: &DbIndex,
    member_item: &LuaMemberIndexItem,
) -> Option<LuaMemberId> {
    match member_item {
        LuaMemberIndexItem::One(member_id) => Some(*member_id),
        LuaMemberIndexItem::Many(member_ids) => {
            let member_index = db.get_member_index();
            let mut resolve_state = MemberTypeResolveState::All;
            let mut members = member_ids
                .iter()
                .map(|id| member_index.get_member(id))
                .collect::<Option<Vec<_>>>()?;
            members.sort_by_key(|member| {
                let member_id = member.get_id();
                let syntax_id = member_id.get_syntax_id();
                (
                    member_id.file_id.id,
                    u32::from(member_id.get_position()),
                    u32::from(syntax_id.get_range().end()),
                    syntax_id.get_kind() as u16,
                )
            });
            for member in &members {
                let feature = member.get_feature();
                if feature.is_meta_decl() {
                    resolve_state = MemberTypeResolveState::Meta;
                    break;
                } else if feature.is_file_decl() {
                    resolve_state = MemberTypeResolveState::FileDecl;
                }
            }

            match resolve_state {
                MemberTypeResolveState::All => {
                    for member in members {
                        let member_type_cache = db
                            .get_type_index()
                            .get_type_cache(&member.get_id().into())?;
                        if member_type_cache.as_type().is_member_owner() {
                            return Some(member.get_id());
                        }
                    }

                    None
                }
                MemberTypeResolveState::Meta => {
                    for member in &members {
                        let feature = member.get_feature();
                        if feature.is_meta_decl() {
                            return Some(member.get_id());
                        }
                    }

                    None
                }
                MemberTypeResolveState::FileDecl => {
                    for member in &members {
                        let feature = member.get_feature();
                        if feature.is_file_decl() {
                            return Some(member.get_id());
                        }
                    }

                    None
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberTypeResolveState {
    All,
    Meta,
    FileDecl,
}

fn resolve_member_semantic_id(
    db: &DbIndex,
    member_item: &LuaMemberIndexItem,
) -> Option<LuaSemanticDeclId> {
    match member_item {
        LuaMemberIndexItem::One(member_id) => Some(LuaSemanticDeclId::Member(*member_id)),
        LuaMemberIndexItem::Many(member_ids) => {
            let mut resolve_state = MemberSemanticDeclResolveState::MetaOrNone;
            let mut members = member_ids
                .iter()
                .map(|id| db.get_member_index().get_member(id))
                .collect::<Option<Vec<_>>>()?;
            members.sort_by_key(|member| {
                let member_id = member.get_id();
                let syntax_id = member_id.get_syntax_id();
                (
                    member_id.file_id.id,
                    u32::from(member_id.get_position()),
                    u32::from(syntax_id.get_range().end()),
                    syntax_id.get_kind() as u16,
                )
            });
            for member in &members {
                let feature = member.get_feature();
                if feature.is_file_define() {
                    resolve_state = MemberSemanticDeclResolveState::FirstDefine;
                } else if feature.is_file_decl() {
                    resolve_state = MemberSemanticDeclResolveState::FileDecl;
                    break;
                }
            }

            match resolve_state {
                MemberSemanticDeclResolveState::MetaOrNone => {
                    let mut last_valid_member =
                        LuaSemanticDeclId::Member(members.first()?.get_id());
                    for member in &members {
                        let feature = member.get_feature();
                        if feature.is_meta_decl() {
                            let semantic_id = LuaSemanticDeclId::Member(member.get_id());
                            last_valid_member = semantic_id.clone();
                            if check_member_version(db, semantic_id.clone()) {
                                return Some(semantic_id);
                            }
                        }
                    }

                    Some(last_valid_member)
                }
                MemberSemanticDeclResolveState::FirstDefine => {
                    resolve_file_define_semantic_member(db, &members)
                }
                MemberSemanticDeclResolveState::FileDecl => {
                    for member in &members {
                        let feature = member.get_feature();
                        if feature.is_file_decl() {
                            return Some(LuaSemanticDeclId::Member(member.get_id()));
                        }
                    }

                    None
                }
            }
        }
    }
}

fn resolve_file_define_semantic_member(
    db: &DbIndex,
    members: &[&crate::LuaMember],
) -> Option<LuaSemanticDeclId> {
    let file_defines = members
        .iter()
        .copied()
        .filter(|member| member.get_feature().is_file_define())
        .collect::<Vec<_>>();

    let informative = file_defines
        .iter()
        .copied()
        .find(|member| !member_type_is_uninformative(db, member.get_id()));
    informative
        .or_else(|| file_defines.first().copied())
        .map(|member| LuaSemanticDeclId::Member(member.get_id()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberSemanticDeclResolveState {
    MetaOrNone,
    FirstDefine,
    FileDecl,
}

fn check_member_version(db: &DbIndex, semantic_id: LuaSemanticDeclId) -> bool {
    let Some(property) = db.get_property_index().get_property(&semantic_id) else {
        return true;
    };

    if let Some(version) = property.version_conds() {
        let version_number = db.get_emmyrc().runtime.version.to_lua_version_number();
        return version.iter().any(|cond| cond.check(&version_number));
    }

    true
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    use super::{
        LuaMemberIndexItem, get_member_id_priority_tiers, select_member_ids_by_workspace_and_realm,
    };
    use std::sync::Arc;

    use crate::{
        DbIndex, Emmyrc, FileId, GmodRealm, GmodRealmFileMetadata, GmodRealmRange,
        LuaSemanticDeclId, LuaTypeDeclId, WorkspaceId,
        db_index::{
            LuaMember, LuaMemberFeature, LuaMemberId, LuaMemberKey, LuaMemberOwner, WorkspaceKind,
        },
    };

    fn make_db() -> DbIndex {
        let mut db = DbIndex::new();
        db.get_module_index_mut()
            .set_module_extract_patterns(["?.lua".to_string(), "?/init.lua".to_string()].to_vec());
        db
    }

    fn make_member_id(file_id: FileId, start: u32) -> LuaMemberId {
        make_member_id_with_kind(file_id, start, LuaSyntaxKind::NameExpr)
    }

    fn make_member_id_with_kind(file_id: FileId, start: u32, kind: LuaSyntaxKind) -> LuaMemberId {
        let range = TextRange::new(TextSize::new(start), TextSize::new(start + 1));
        LuaMemberId::new(LuaSyntaxId::new(kind.into(), range), file_id)
    }

    fn set_file_realms(db: &mut DbIndex, file_realms: &[(FileId, GmodRealm)]) {
        db.get_gmod_infer_index_mut().set_all_realm_file_metadata(
            file_realms
                .iter()
                .map(|(file_id, realm)| {
                    (
                        *file_id,
                        GmodRealmFileMetadata {
                            inferred_realm: *realm,
                            ..Default::default()
                        },
                    )
                })
                .collect(),
        );
    }

    #[test]
    fn member_id_priority_tiers_keep_workspace_priority_order() {
        let mut db = make_db();
        let module_index = db.get_module_index_mut();

        let workspace_a = WorkspaceId::MAIN;
        let workspace_b = WorkspaceId { id: 3 };
        let library_workspace = WorkspaceId { id: 4 };

        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA").into(),
            workspace_a,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectB").into(),
            workspace_b,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA/lua/lib").into(),
            library_workspace,
            WorkspaceKind::Library,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/.lua/std").into(),
            WorkspaceId::STD,
            WorkspaceKind::Std,
        );

        let caller_file = FileId::new(1);
        module_index.add_module_by_path(caller_file, "C:/Users/username/ProjectA/init.lua");

        let library_file = FileId::new(2);
        module_index.add_module_by_path(
            library_file,
            "C:/Users/username/ProjectA/lua/lib/shared.lua",
        );

        let std_file = FileId::new(3);
        module_index.add_module_by_path(std_file, "C:/Users/username/.lua/std/math.lua");

        let other_main_file = FileId::new(4);
        module_index.add_module_by_path(other_main_file, "C:/Users/username/ProjectB/init.lua");

        let library_member = make_member_id(library_file, 1);
        let std_member = make_member_id(std_file, 2);
        let other_main_member = make_member_id(other_main_file, 3);

        let tiers = get_member_id_priority_tiers(
            &db,
            &caller_file,
            &[other_main_member, std_member, library_member],
        );

        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0], (1, vec![library_member]));
        assert_eq!(tiers[1], (2, vec![std_member]));
    }

    #[test]
    fn member_id_priority_tiers_include_other_main_when_isolation_disabled() {
        let mut db = make_db();
        let module_index = db.get_module_index_mut();

        let workspace_a = WorkspaceId::MAIN;
        let workspace_b = WorkspaceId { id: 3 };

        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA").into(),
            workspace_a,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectB").into(),
            workspace_b,
            WorkspaceKind::Main,
        );

        let mut emmyrc = Emmyrc::default();
        emmyrc.workspace.enable_isolation = false;
        module_index.update_config(Arc::new(emmyrc));

        let caller_file = FileId::new(10);
        module_index.add_module_by_path(caller_file, "C:/Users/username/ProjectA/init.lua");

        let other_main_file = FileId::new(11);
        module_index.add_module_by_path(other_main_file, "C:/Users/username/ProjectB/init.lua");

        let caller_member = make_member_id(caller_file, 1);
        let other_main_member = make_member_id(other_main_file, 2);

        let tiers =
            get_member_id_priority_tiers(&db, &caller_file, &[caller_member, other_main_member]);

        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0], (0, vec![caller_member, other_main_member]));
    }

    #[test]
    fn select_member_ids_by_workspace_and_realm_uses_first_compatible_tier() {
        let mut db = make_db();
        let tier_one_member = make_member_id(FileId::new(10), 1);
        let tier_two_member = make_member_id(FileId::new(11), 2);

        set_file_realms(
            &mut db,
            &[
                (tier_one_member.file_id, GmodRealm::Shared),
                (tier_two_member.file_id, GmodRealm::Unknown),
            ],
        );

        let selected = select_member_ids_by_workspace_and_realm(
            &db,
            &FileId::new(100),
            vec![(0, vec![tier_one_member]), (1, vec![tier_two_member])],
            GmodRealm::Client,
        );

        assert_eq!(selected, vec![tier_one_member]);
    }

    #[test]
    fn select_member_ids_by_workspace_and_realm_does_not_fallback_to_opposite_strict_realm() {
        let mut db = make_db();
        let server_member = make_member_id(FileId::new(20), 1);
        let unknown_member = make_member_id(FileId::new(21), 2);

        set_file_realms(
            &mut db,
            &[
                (server_member.file_id, GmodRealm::Server),
                (unknown_member.file_id, GmodRealm::Server),
            ],
        );

        let selected = select_member_ids_by_workspace_and_realm(
            &db,
            &FileId::new(101),
            vec![(0, vec![server_member]), (1, vec![unknown_member])],
            GmodRealm::Client,
        );

        assert!(selected.is_empty());
    }

    #[test]
    fn select_member_ids_by_workspace_and_realm_applies_stable_tiebreaker_for_equivalent_matches() {
        let mut db = make_db();
        let caller_file = FileId::new(30);
        let other_file = FileId::new(31);
        let same_file_member = make_member_id(caller_file, 20);
        let other_file_member = make_member_id(other_file, 1);

        set_file_realms(
            &mut db,
            &[
                (caller_file, GmodRealm::Server),
                (other_file, GmodRealm::Server),
            ],
        );

        let selected = select_member_ids_by_workspace_and_realm(
            &db,
            &caller_file,
            vec![(0, vec![other_file_member, same_file_member])],
            GmodRealm::Server,
        );

        assert_eq!(selected, vec![same_file_member, other_file_member]);
    }

    #[test]
    fn select_member_ids_by_workspace_and_realm_is_stable_when_sort_fields_tie() {
        let mut db = make_db();
        let caller_file = FileId::new(32);
        let first_member = make_member_id_with_kind(caller_file, 40, LuaSyntaxKind::NameExpr);
        let second_member = make_member_id_with_kind(caller_file, 40, LuaSyntaxKind::IndexExpr);

        set_file_realms(&mut db, &[(caller_file, GmodRealm::Server)]);

        let selected_forward = select_member_ids_by_workspace_and_realm(
            &db,
            &caller_file,
            vec![(0, vec![first_member, second_member])],
            GmodRealm::Server,
        );
        let selected_reversed = select_member_ids_by_workspace_and_realm(
            &db,
            &caller_file,
            vec![(0, vec![second_member, first_member])],
            GmodRealm::Server,
        );

        assert_eq!(selected_forward, selected_reversed);
    }

    #[test]
    fn resolve_semantic_decl_with_realm_prefers_first_compatible_tier() {
        let mut db = make_db();
        let module_index = db.get_module_index_mut();

        let workspace_a = WorkspaceId::MAIN;
        let workspace_b = WorkspaceId { id: 3 };
        let library_workspace = WorkspaceId { id: 4 };

        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA").into(),
            workspace_a,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectB").into(),
            workspace_b,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA/lua/lib").into(),
            library_workspace,
            WorkspaceKind::Library,
        );

        let caller_file = FileId::new(1);
        module_index.add_module_by_path(caller_file, "C:/Users/username/ProjectA/init.lua");

        let library_file = FileId::new(2);
        module_index.add_module_by_path(
            library_file,
            "C:/Users/username/ProjectA/lua/lib/shared.lua",
        );

        let other_main_file = FileId::new(3);
        module_index.add_module_by_path(other_main_file, "C:/Users/username/ProjectB/init.lua");

        let library_member = make_member_id(library_file, 1);
        let other_main_member = make_member_id(other_main_file, 2);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("Owner"));

        db.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                library_member,
                LuaMemberKey::Name("value".into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        db.get_member_index_mut().add_member(
            owner,
            LuaMember::new(
                other_main_member,
                LuaMemberKey::Name("value".into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );

        set_file_realms(
            &mut db,
            &[
                (caller_file, GmodRealm::Client),
                (library_file, GmodRealm::Shared),
                (other_main_file, GmodRealm::Server),
            ],
        );

        let item = LuaMemberIndexItem::Many(vec![other_main_member, library_member]);
        let semantic_decl = item.resolve_semantic_decl_with_realm(&db, &caller_file);

        assert_eq!(
            semantic_decl,
            Some(LuaSemanticDeclId::Member(library_member))
        );
    }

    #[test]
    fn resolve_semantic_decl_with_realm_at_offset_prefers_branch_compatible_member() {
        let mut db = make_db();
        let caller_file = FileId::new(10);
        let branch_file = FileId::new(11);
        let client_member = make_member_id(branch_file, 1);
        let server_member = make_member_id(branch_file, 20);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("BranchOwner"));

        db.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                client_member,
                LuaMemberKey::Name("branchValue".into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        db.get_member_index_mut().add_member(
            owner,
            LuaMember::new(
                server_member,
                LuaMemberKey::Name("branchValue".into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );

        db.get_gmod_infer_index_mut().set_all_realm_file_metadata(
            [
                (
                    caller_file,
                    GmodRealmFileMetadata {
                        inferred_realm: GmodRealm::Shared,
                        branch_realm_ranges: vec![
                            GmodRealmRange {
                                range: TextRange::new(TextSize::new(0), TextSize::new(10)),
                                realm: GmodRealm::Client,
                            },
                            GmodRealmRange {
                                range: TextRange::new(TextSize::new(10), TextSize::new(30)),
                                realm: GmodRealm::Server,
                            },
                        ],
                        ..Default::default()
                    },
                ),
                (
                    branch_file,
                    GmodRealmFileMetadata {
                        inferred_realm: GmodRealm::Shared,
                        branch_realm_ranges: vec![
                            GmodRealmRange {
                                range: TextRange::new(TextSize::new(0), TextSize::new(10)),
                                realm: GmodRealm::Client,
                            },
                            GmodRealmRange {
                                range: TextRange::new(TextSize::new(10), TextSize::new(30)),
                                realm: GmodRealm::Server,
                            },
                        ],
                        ..Default::default()
                    },
                ),
            ]
            .into_iter()
            .collect(),
        );

        let item = LuaMemberIndexItem::Many(vec![client_member, server_member]);

        let client_decl =
            item.resolve_semantic_decl_with_realm_at_offset(&db, &caller_file, TextSize::new(1));
        let server_decl =
            item.resolve_semantic_decl_with_realm_at_offset(&db, &caller_file, TextSize::new(20));

        assert_eq!(client_decl, Some(LuaSemanticDeclId::Member(client_member)));
        assert_eq!(server_decl, Some(LuaSemanticDeclId::Member(server_member)));
    }

    #[test]
    fn single_member_visibility_matches_many_member_realm_filtering() {
        let mut db = make_db();
        let caller_file = FileId::new(10);
        let server_file = FileId::new(11);
        let server_member = make_member_id(server_file, 1);

        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        db.get_module_index_mut().update_config(Arc::new(emmyrc));
        set_file_realms(
            &mut db,
            &[
                (caller_file, GmodRealm::Client),
                (server_file, GmodRealm::Server),
            ],
        );

        let single = LuaMemberIndexItem::One(server_member)
            .visible_member_ids_with_realm_at_offset(&db, &caller_file, TextSize::new(1));
        let many = LuaMemberIndexItem::Many(vec![server_member])
            .visible_member_ids_with_realm_at_offset(&db, &caller_file, TextSize::new(1));

        assert_eq!(single, many);
        assert!(single.is_empty());
    }

    #[test]
    fn single_member_visibility_matches_many_member_compatible_realm() {
        let mut db = make_db();
        let caller_file = FileId::new(10);
        let shared_file = FileId::new(11);
        let shared_member = make_member_id(shared_file, 1);

        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        db.get_module_index_mut().update_config(Arc::new(emmyrc));
        set_file_realms(
            &mut db,
            &[
                (caller_file, GmodRealm::Client),
                (shared_file, GmodRealm::Shared),
            ],
        );

        let single = LuaMemberIndexItem::One(shared_member)
            .visible_member_ids_with_realm_at_offset(&db, &caller_file, TextSize::new(1));
        let many = LuaMemberIndexItem::Many(vec![shared_member])
            .visible_member_ids_with_realm_at_offset(&db, &caller_file, TextSize::new(1));

        assert_eq!(single, many);
    }

    #[test]
    fn single_member_visibility_matches_many_member_workspace_isolation() {
        let mut db = make_db();
        let module_index = db.get_module_index_mut();

        let workspace_a = WorkspaceId::MAIN;
        let workspace_b = WorkspaceId { id: 3 };
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA").into(),
            workspace_a,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectB").into(),
            workspace_b,
            WorkspaceKind::Main,
        );

        let caller_file = FileId::new(10);
        module_index.add_module_by_path(caller_file, "C:/Users/username/ProjectA/init.lua");
        let isolated_file = FileId::new(11);
        module_index.add_module_by_path(isolated_file, "C:/Users/username/ProjectB/init.lua");
        let isolated_member = make_member_id(isolated_file, 1);

        let single = LuaMemberIndexItem::One(isolated_member)
            .visible_member_ids_with_realm_at_offset(&db, &caller_file, TextSize::new(1));
        let many = LuaMemberIndexItem::Many(vec![isolated_member])
            .visible_member_ids_with_realm_at_offset(&db, &caller_file, TextSize::new(1));

        assert_eq!(single, many);
        assert!(single.is_empty());
    }

    #[test]
    fn visible_member_ids_at_offset_excludes_later_same_file_file_defines() {
        let mut db = make_db();
        let caller_file = FileId::new(10);
        let earlier_member = make_member_id_with_kind(caller_file, 10, LuaSyntaxKind::IndexExpr);
        let later_member = make_member_id_with_kind(caller_file, 30, LuaSyntaxKind::IndexExpr);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OrderSensitiveOwner"));
        let key = LuaMemberKey::Name("field".into());

        db.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                earlier_member,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );
        db.get_member_index_mut().add_member(
            owner,
            LuaMember::new(later_member, key, LuaMemberFeature::FileDefine, None),
        );

        let item = LuaMemberIndexItem::Many(vec![earlier_member, later_member]);
        let visible =
            item.visible_member_ids_with_realm_at_offset(&db, &caller_file, TextSize::new(20));

        assert_eq!(visible, vec![earlier_member]);

        let future_only_member =
            make_member_id_with_kind(caller_file, 50, LuaSyntaxKind::IndexExpr);
        db.get_member_index_mut().add_member(
            LuaMemberOwner::Type(LuaTypeDeclId::global("FutureOnlyOwner")),
            LuaMember::new(
                future_only_member,
                LuaMemberKey::Name("field".into()),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );
        assert!(
            LuaMemberIndexItem::One(future_only_member)
                .visible_member_ids_with_realm_at_offset(&db, &caller_file, TextSize::new(20))
                .is_empty()
        );
    }

    #[test]
    fn visible_member_ids_at_offset_keeps_later_same_file_declarations() {
        let mut db = make_db();
        let caller_file = FileId::new(10);
        let declared_member = make_member_id_with_kind(caller_file, 30, LuaSyntaxKind::DocTagField);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OrderInsensitiveOwner"));

        db.get_member_index_mut().add_member(
            owner,
            LuaMember::new(
                declared_member,
                LuaMemberKey::Name("field".into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );

        let visible = LuaMemberIndexItem::One(declared_member)
            .visible_member_ids_with_realm_at_offset(&db, &caller_file, TextSize::new(20));

        assert_eq!(visible, vec![declared_member]);
    }

    #[test]
    fn visible_member_ids_at_offset_falls_back_to_assignment_history_when_latest_is_future() {
        let mut db = make_db();
        let caller_file = FileId::new(10);
        let earlier_member = make_member_id_with_kind(caller_file, 10, LuaSyntaxKind::IndexExpr);
        let later_member = make_member_id_with_kind(caller_file, 30, LuaSyntaxKind::IndexExpr);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("AssignmentHistoryOwner"));
        let key = LuaMemberKey::Name("field".into());

        db.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                earlier_member,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );
        db.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                later_member,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        let item = db
            .get_member_index()
            .get_member_item(&owner, &key)
            .expect("runtime assignments collapse to the latest member");
        assert_eq!(item, &LuaMemberIndexItem::One(later_member));

        let visible =
            item.visible_member_ids_with_realm_at_offset(&db, &caller_file, TextSize::new(20));

        assert_eq!(visible, vec![earlier_member]);
    }

    #[test]
    fn visible_member_ids_from_history_matches_expanded_assignment_history() {
        let mut db = make_db();
        let caller_file = FileId::new(10);
        let earlier_member = make_member_id_with_kind(caller_file, 10, LuaSyntaxKind::IndexExpr);
        let later_member = make_member_id_with_kind(caller_file, 30, LuaSyntaxKind::IndexExpr);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("PreExpandedHistoryOwner"));
        let key = LuaMemberKey::Name("field".into());

        db.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                earlier_member,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );
        db.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                later_member,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        let latest_item = db
            .get_member_index()
            .get_member_item(&owner, &key)
            .expect("runtime assignments collapse to the latest member");
        let pre_expanded_history = LuaMemberIndexItem::Many(vec![earlier_member, later_member]);

        let expanded_visible = latest_item.visible_member_ids_with_realm_at_offset(
            &db,
            &caller_file,
            TextSize::new(20),
        );
        let history_visible = pre_expanded_history
            .visible_member_ids_with_realm_at_offset_from_history(
                &db,
                &caller_file,
                TextSize::new(20),
            );

        assert_eq!(expanded_visible, vec![earlier_member]);
        assert_eq!(history_visible, expanded_visible);
    }

    #[test]
    fn visible_member_ids_at_offset_keeps_latest_assignment_when_it_is_visible() {
        let mut db = make_db();
        let caller_file = FileId::new(10);
        let earlier_member = make_member_id_with_kind(caller_file, 10, LuaSyntaxKind::IndexExpr);
        let later_member = make_member_id_with_kind(caller_file, 30, LuaSyntaxKind::IndexExpr);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("LatestAssignmentOwner"));
        let key = LuaMemberKey::Name("field".into());

        db.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                earlier_member,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );
        db.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                later_member,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        let item = db
            .get_member_index()
            .get_member_item(&owner, &key)
            .expect("runtime assignments collapse to the latest member");
        let visible =
            item.visible_member_ids_with_realm_at_offset(&db, &caller_file, TextSize::new(40));

        assert_eq!(visible, vec![later_member]);
    }

    #[test]
    fn select_member_ids_includes_annotated_from_lower_tier_when_highest_is_non_meta() {
        let mut db = make_db();
        let module_index = db.get_module_index_mut();

        let workspace_a = WorkspaceId::MAIN;
        let library_workspace = WorkspaceId { id: 4 };

        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA").into(),
            workspace_a,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA/lua/lib").into(),
            library_workspace,
            WorkspaceKind::Library,
        );

        let caller_file = FileId::new(1);
        module_index.add_module_by_path(
            caller_file,
            "C:/Users/username/ProjectA/entities/letter/init.lua",
        );

        // Library file with annotated MetaMethodDecl (same as glua-api-snippets ents.lua)
        let library_file = FileId::new(2);
        module_index
            .add_module_by_path(library_file, "C:/Users/username/ProjectA/lua/lib/ents.lua");

        // Main workspace file with unannotated override (simulating DarkRP sh_workarounds.lua)
        let override_file = FileId::new(3);
        module_index.add_module_by_path(
            override_file,
            "C:/Users/username/ProjectA/gamemode/modules/workarounds/sh_workarounds.lua",
        );

        let library_member = make_member_id(library_file, 1);
        let override_member = make_member_id(override_file, 2);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("ents"));

        // Annotated MetaMethodDecl in library
        db.get_member_index_mut().add_member(
            owner.clone(),
            LuaMember::new(
                library_member,
                LuaMemberKey::Name("Create".into()),
                LuaMemberFeature::MetaMethodDecl,
                None,
            ),
        );
        // Unannotated FileMethodDecl in main (DarkRP override)
        db.get_member_index_mut().add_member(
            owner,
            LuaMember::new(
                override_member,
                LuaMemberKey::Name("Create".into()),
                LuaMemberFeature::FileMethodDecl,
                None,
            ),
        );

        // Caller file is shared realm (entities/letter/ pattern)
        // Library file is shared realm (top-level ents.lua)
        // Override file is server realm (inside `if SERVER then`)
        set_file_realms(
            &mut db,
            &[
                (caller_file, GmodRealm::Shared),
                (library_file, GmodRealm::Shared),
                (override_file, GmodRealm::Server),
            ],
        );

        // GMod must be enabled for realm filtering to work
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        db.get_module_index_mut().update_config(Arc::new(emmyrc));

        let selected = select_member_ids_by_workspace_and_realm(
            &db,
            &caller_file,
            vec![(0, vec![override_member]), (1, vec![library_member])],
            GmodRealm::Shared,
        );

        // Verify BOTH members are returned — the annotated library member
        // must be included even though it's in a lower-priority tier, because
        // the higher-priority tier has only unannotated members.
        assert!(
            selected.contains(&library_member),
            "annotated MetaMethodDecl from library workspace should be included \
             even when an unannotated FileMethodDecl exists in the main workspace"
        );
        assert!(
            selected.contains(&override_member),
            "main-workspace FileMethodDecl override should still be included"
        );
    }
}
