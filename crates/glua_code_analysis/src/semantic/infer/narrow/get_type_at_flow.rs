use std::{collections::HashSet, ops::Deref};

use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAstNode, LuaBlock, LuaCallExpr, LuaChunk, LuaClosureExpr,
    LuaExpr, LuaFuncStat, LuaIndexExpr, LuaIndexKey, LuaLiteralToken, LuaLocalFuncStat, LuaStat,
    LuaVarExpr, NumberResult, PathTrait, UnaryOperator,
};
use rowan::{TextRange, TextSize};

use crate::{
    AssignVarHint, CacheEntry, DbIndex, FileId, FlowAntecedent, FlowId, FlowNode, FlowNodeKind,
    FlowTree, GlobalId, GmodRealm, InferFailReason, LuaArrayType, LuaDeclId, LuaInferCache,
    LuaMemberId, LuaMemberKey, LuaMemberOwner, LuaSemanticDeclId, LuaSignatureId, LuaType,
    LuaTypeDeclId, LuaTypeOwner, LuaUnionType, TypeOps, infer_expr,
    semantic::cache::FlowOrigin,
    semantic::gmod_call_effect::{GmodCallWriteEffect, gmod_call_write_effect},
    semantic::infer::{
        InferResult, VarRefId, infer_expr_list_value_type_at,
        infer_name::infer_global_type,
        infer_param_with_cache,
        narrow::{
            ResultTypeOrContinue,
            condition_flow::{InferConditionFlow, get_type_at_condition_flow},
            get_single_antecedent,
            get_type_at_cast_flow::get_type_at_cast_flow,
            get_var_ref_type, narrow_direct_name_false_or_nil, narrow_down_type,
            remove_false_or_nil,
            var_ref_id::{
                get_var_expr_var_ref_id, is_immutable_direct_lexical_decl,
                is_untyped_param_rooted_index,
            },
        },
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlowWalkMode {
    Normal,
    ClosureBaseline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FlowWalkPolicy {
    origin: FlowOrigin,
    mode: FlowWalkMode,
}

impl FlowWalkPolicy {
    pub(super) fn normal(origin: FlowOrigin) -> Self {
        Self {
            origin,
            mode: FlowWalkMode::Normal,
        }
    }

    fn with_mode(self, mode: FlowWalkMode) -> Self {
        Self { mode, ..self }
    }

    fn is_normal(self) -> bool {
        self.mode == FlowWalkMode::Normal
    }

    fn is_closure_baseline(self) -> bool {
        self.mode == FlowWalkMode::ClosureBaseline
    }
}

pub fn get_type_at_flow(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_id: FlowId,
) -> InferResult {
    get_type_at_flow_with_origin(db, tree, cache, root, var_ref_id, flow_id, FlowOrigin::Real)
}

pub fn get_type_at_flow_with_origin(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_id: FlowId,
    flow_origin: FlowOrigin,
) -> InferResult {
    let policy = FlowWalkPolicy::normal(flow_origin);
    let query_realm = cache.flow_query_realm.unwrap_or_else(|| {
        db.get_gmod_infer_index()
            .get_realm_at_offset(&cache.get_file_id(), var_ref_id.get_position())
    });
    // Check cache for both success and error results.
    match cache
        .get_flow_cache_with_origin(var_ref_id, flow_id, query_realm, policy.origin)
        .cloned()
    {
        Some(CacheEntry::Cache(narrow_type)) => {
            return Ok(narrow_type);
        }
        Some(CacheEntry::Error(reason)) => {
            return Err(reason);
        }
        _ => {}
    }
    let mut visited_flow_ids = Vec::new();
    let result = get_type_at_flow_walk(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        query_realm,
        flow_id,
        &mut visited_flow_ids,
        policy,
    );

    // RecursiveInfer errors are transient (cycle detection) and must NOT be
    // cached — they'd poison future non-recursive queries.
    match &result {
        Ok(ty) => {
            let entry = CacheEntry::Cache(ty.clone());
            cache.set_flow_cache_with_origin(
                var_ref_id,
                flow_id,
                query_realm,
                policy.origin,
                entry.clone(),
            );
            for visited_flow_id in visited_flow_ids {
                cache.set_flow_cache_with_origin(
                    var_ref_id,
                    visited_flow_id,
                    query_realm,
                    policy.origin,
                    entry.clone(),
                );
            }
        }
        Err(InferFailReason::RecursiveInfer) => {
            // Don't cache — this is a transient cycle-detection signal.
        }
        Err(reason) => {
            let should_cache = match reason {
                InferFailReason::UnResolveDeclType(_) => {
                    cache.get_config().analysis_phase.is_diagnostics()
                }
                _ => true,
            };

            if should_cache {
                let entry = CacheEntry::Error(reason.clone());
                cache.set_flow_cache_with_origin(
                    var_ref_id,
                    flow_id,
                    query_realm,
                    policy.origin,
                    entry.clone(),
                );
                for visited_flow_id in visited_flow_ids {
                    cache.set_flow_cache_with_origin(
                        var_ref_id,
                        visited_flow_id,
                        query_realm,
                        policy.origin,
                        entry.clone(),
                    );
                }
            }
        }
    }

    result
}

pub(super) fn get_type_at_flow_in_mode(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_id: FlowId,
    policy: FlowWalkPolicy,
) -> InferResult {
    match policy.mode {
        FlowWalkMode::Normal => {
            get_type_at_flow_with_origin(db, tree, cache, root, var_ref_id, flow_id, policy.origin)
        }
        FlowWalkMode::ClosureBaseline => {
            let query_realm = cache.flow_query_realm.unwrap_or_else(|| {
                db.get_gmod_infer_index()
                    .get_realm_at_offset(&cache.get_file_id(), var_ref_id.get_position())
            });
            let mut visited_flow_ids = Vec::new();
            get_type_at_flow_walk(
                db,
                tree,
                cache,
                root,
                var_ref_id,
                query_realm,
                flow_id,
                &mut visited_flow_ids,
                policy,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn get_type_at_immutable_closure_condition(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    mut condition: LuaExpr,
    mut condition_flow: InferConditionFlow,
    policy: FlowWalkPolicy,
) -> Result<Option<LuaType>, InferFailReason> {
    if !is_immutable_direct_lexical_decl(db, var_ref_id) {
        return Ok(None);
    }

    loop {
        condition = strip_condition_parens(condition)?;
        let LuaExpr::UnaryExpr(unary_expr) = condition else {
            break;
        };
        if unary_expr
            .get_op_token()
            .is_none_or(|token| token.get_op() != UnaryOperator::OpNot)
        {
            return Ok(None);
        }
        let Some(inner_expr) = unary_expr.get_expr() else {
            return Ok(None);
        };
        condition = inner_expr;
        condition_flow = condition_flow.get_negated();
    }

    match condition {
        LuaExpr::NameExpr(name_expr) => {
            if !condition_name_matches_var_ref(db, cache, name_expr, var_ref_id) {
                return Ok(None);
            }
            let antecedent_type = get_antecedent_type_for_flow_node(
                db, tree, cache, root, var_ref_id, flow_node, policy,
            )?;
            Ok(Some(match condition_flow {
                InferConditionFlow::TrueCondition => remove_false_or_nil(antecedent_type),
                InferConditionFlow::FalseCondition => {
                    narrow_direct_name_false_or_nil(db, antecedent_type)
                }
            }))
        }
        LuaExpr::BinaryExpr(binary_expr) => {
            let Some(op_token) = binary_expr.get_op_token() else {
                return Ok(None);
            };
            let op = op_token.get_op();
            if !matches!(op, BinaryOperator::OpEq | BinaryOperator::OpNe) {
                return Ok(None);
            }
            let Some((left, right)) = binary_expr.get_exprs() else {
                return Ok(None);
            };
            let exact_type = if condition_expr_matches_var_ref(db, cache, left.clone(), var_ref_id)
            {
                falsey_condition_literal_type(right)
            } else if condition_expr_matches_var_ref(db, cache, right.clone(), var_ref_id) {
                falsey_condition_literal_type(left)
            } else {
                None
            };
            let Some(exact_type) = exact_type else {
                return Ok(None);
            };

            let antecedent_type = get_antecedent_type_for_flow_node(
                db, tree, cache, root, var_ref_id, flow_node, policy,
            )?;
            let equality_holds = matches!(condition_flow, InferConditionFlow::TrueCondition)
                == matches!(op, BinaryOperator::OpEq);
            let narrowed = if equality_holds {
                TypeOps::Intersect.apply(db, &antecedent_type, &exact_type)
            } else {
                TypeOps::Remove.apply(db, &antecedent_type, &exact_type)
            };
            Ok(Some(narrowed))
        }
        LuaExpr::CallExpr(call_expr) => match get_type_at_condition_flow(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            LuaExpr::CallExpr(call_expr),
            condition_flow,
            policy,
        )? {
            ResultTypeOrContinue::Result(condition_type) => Ok(Some(condition_type)),
            ResultTypeOrContinue::Continue => Ok(None),
        },
        _ => Ok(None),
    }
}

fn strip_condition_parens(mut expr: LuaExpr) -> Result<LuaExpr, InferFailReason> {
    while let LuaExpr::ParenExpr(paren_expr) = expr {
        expr = paren_expr.get_expr().ok_or(InferFailReason::None)?;
    }
    Ok(expr)
}

fn condition_name_matches_var_ref(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: glua_parser::LuaNameExpr,
    var_ref_id: &VarRefId,
) -> bool {
    get_var_expr_var_ref_id(db, cache, LuaExpr::NameExpr(name_expr))
        .is_some_and(|condition_ref| condition_ref == *var_ref_id)
}

fn condition_expr_matches_var_ref(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: LuaExpr,
    var_ref_id: &VarRefId,
) -> bool {
    let Ok(LuaExpr::NameExpr(name_expr)) = strip_condition_parens(expr) else {
        return false;
    };
    condition_name_matches_var_ref(db, cache, name_expr, var_ref_id)
}

fn falsey_condition_literal_type(expr: LuaExpr) -> Option<LuaType> {
    let Ok(LuaExpr::LiteralExpr(literal_expr)) = strip_condition_parens(expr) else {
        return None;
    };
    match literal_expr.get_literal()? {
        LuaLiteralToken::Nil(_) => Some(LuaType::Nil),
        LuaLiteralToken::Bool(token) if !token.is_true() => Some(LuaType::BooleanConst(false)),
        _ => None,
    }
}

/// Inner walk loop for `get_type_at_flow`.
fn get_type_at_flow_walk(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    query_realm: GmodRealm,
    initial_flow_id: FlowId,
    visited_flow_ids: &mut Vec<FlowId>,
    policy: FlowWalkPolicy,
) -> InferResult {
    let mut antecedent_flow_id = initial_flow_id;
    let pending_branch_types = [];
    loop {
        // Check cache for intermediate flow nodes (both success and error).
        // This is critical for performance in large files where many walks
        // share overlapping flow chains.
        if policy.is_normal() {
            match cache.get_flow_cache_with_origin(
                var_ref_id,
                antecedent_flow_id,
                query_realm,
                policy.origin,
            ) {
                Some(CacheEntry::Cache(cached_type)) => {
                    return finish_flow_walk_result(
                        db,
                        var_ref_id,
                        &pending_branch_types,
                        Ok(cached_type.clone()),
                    );
                }
                Some(CacheEntry::Error(reason)) => return Err(reason.clone()),
                _ => {}
            }
            visited_flow_ids.push(antecedent_flow_id);
        }

        let flow_node = tree
            .get_flow_node(antecedent_flow_id)
            .ok_or(InferFailReason::None)?;
        match &flow_node.kind {
            FlowNodeKind::Start | FlowNodeKind::Unreachable => {
                return finish_flow_walk_result(
                    db,
                    var_ref_id,
                    &pending_branch_types,
                    get_var_ref_type(db, cache, var_ref_id),
                );
            }
            FlowNodeKind::ClosureEntry(_) => {
                let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                return finish_flow_walk_result(
                    db,
                    var_ref_id,
                    &pending_branch_types,
                    get_type_at_flow_walk(
                        db,
                        tree,
                        cache,
                        root,
                        var_ref_id,
                        query_realm,
                        antecedent_flow_id,
                        visited_flow_ids,
                        policy.with_mode(FlowWalkMode::ClosureBaseline),
                    ),
                );
            }
            FlowNodeKind::LoopLabel | FlowNodeKind::Break | FlowNodeKind::Return => {
                if let Some(merged_type) = try_get_multi_antecedent_type(
                    db, tree, cache, root, var_ref_id, flow_node, policy,
                )? {
                    return finish_flow_walk_result(
                        db,
                        var_ref_id,
                        &pending_branch_types,
                        Ok(merged_type),
                    );
                }
                antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
            }
            FlowNodeKind::BranchLabel | FlowNodeKind::NamedLabel(_) => {
                if matches!(flow_node.kind, FlowNodeKind::BranchLabel)
                    && let Some(info) = tree.get_branch_label_info(antecedent_flow_id)
                {
                    let can_skip = !branch_can_narrow_var_ref(db, info, var_ref_id);

                    if can_skip
                        && all_branch_antecedents_alive(tree, flow_node)
                        && !branch_has_relevant_special_call_effects(
                            db,
                            tree,
                            cache,
                            root,
                            flow_node,
                            info.common_predecessor,
                            var_ref_id,
                        )
                    {
                        antecedent_flow_id = info.common_predecessor;
                        continue;
                    }
                }

                return finish_flow_walk_result(
                    db,
                    var_ref_id,
                    &pending_branch_types,
                    merge_antecedent_types(db, tree, cache, root, var_ref_id, flow_node, policy),
                );
            }
            FlowNodeKind::DeclPosition(position) => {
                if *position <= var_ref_id.get_position() {
                    if let Some(decl_id) = var_ref_id.get_decl_id_ref()
                        && should_defer_uninitialized_local_decl_type(db, decl_id)
                    {
                        if policy.is_closure_baseline() {
                            let baseline_type = get_var_ref_type(db, cache, var_ref_id)
                                .map(|typ| TypeOps::Union.apply(db, &typ, &LuaType::Nil));
                            return finish_flow_walk_result(
                                db,
                                var_ref_id,
                                &pending_branch_types,
                                baseline_type,
                            );
                        }
                        return Err(InferFailReason::UnResolveDeclType(decl_id));
                    }

                    match get_decl_position_var_ref_type(db, cache, var_ref_id) {
                        Ok(var_type) => {
                            if var_ref_id
                                .get_decl_id_ref()
                                .map(|decl_id| decl_id.into())
                                .and_then(|owner| db.get_type_index().get_type_cache(&owner))
                                .is_some_and(|type_cache| type_cache.is_doc())
                            {
                                return finish_flow_walk_result(
                                    db,
                                    var_ref_id,
                                    &pending_branch_types,
                                    Ok(var_type),
                                );
                            }

                            if should_retry_decl_initializer_type(&var_type)
                                && let Ok(Some(init_type)) =
                                    try_infer_decl_initializer_type(db, cache, root, var_ref_id)
                                && !should_retry_decl_initializer_type(&init_type)
                            {
                                return finish_flow_walk_result(
                                    db,
                                    var_ref_id,
                                    &pending_branch_types,
                                    Ok(init_type),
                                );
                            }

                            return finish_flow_walk_result(
                                db,
                                var_ref_id,
                                &pending_branch_types,
                                Ok(var_type),
                            );
                        }
                        Err(err) => {
                            if let Some(init_type) =
                                try_infer_decl_initializer_type(db, cache, root, var_ref_id)?
                            {
                                return finish_flow_walk_result(
                                    db,
                                    var_ref_id,
                                    &pending_branch_types,
                                    Ok(init_type),
                                );
                            }

                            return Err(err);
                        }
                    }
                } else {
                    if let Some(merged_type) = try_get_multi_antecedent_type(
                        db, tree, cache, root, var_ref_id, flow_node, policy,
                    )? {
                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(merged_type),
                        );
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                }
            }
            FlowNodeKind::Assignment(assign_ptr, assign_hint) => {
                if let Some(decl_id) = var_ref_id.get_decl_id_ref()
                    && let Some(target) =
                        tree.get_assignment_flow_info(flow_node.id)
                            .and_then(|info| {
                                info.name_targets
                                    .iter()
                                    .find(|target| target.decl_id == decl_id)
                            })
                    && let Some(fact) = db.get_type_index().get_definition_fact(
                        &crate::LuaDefinitionId::Assignment {
                            file_id: decl_id.file_id,
                            assignment: assign_ptr.get_syntax_id(),
                            target_idx: target.target_idx,
                        },
                    )
                {
                    return finish_flow_walk_result(
                        db,
                        var_ref_id,
                        &pending_branch_types,
                        Ok(fact.typ().clone()),
                    );
                }

                let can_match_assignment = matches!(
                    (assign_hint, var_ref_id),
                    (AssignVarHint::Mixed, _)
                        | (AssignVarHint::NameOnly, VarRefId::VarRef(_))
                        | (AssignVarHint::NameOnly, VarRefId::GlobalName(_, _))
                        | (AssignVarHint::NameOnly, VarRefId::SelfRef(_))
                        | (AssignVarHint::NameOnly, VarRefId::IndexRef(_, _))
                        | (AssignVarHint::IndexOnly, VarRefId::IndexRef(_, _))
                ) || (matches!(assign_hint, AssignVarHint::IndexOnly)
                    && numeric_table_index_query(db, cache, root, var_ref_id).is_some());

                if !can_match_assignment {
                    if let Some(merged_type) = try_get_multi_antecedent_type(
                        db, tree, cache, root, var_ref_id, flow_node, policy,
                    )? {
                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(merged_type),
                        );
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    continue;
                }

                if assignment_flow_info_cannot_match(tree, antecedent_flow_id, var_ref_id)
                    && numeric_table_index_query(db, cache, root, var_ref_id).is_none()
                {
                    if let Some(merged_type) = try_get_multi_antecedent_type(
                        db, tree, cache, root, var_ref_id, flow_node, policy,
                    )? {
                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(merged_type),
                        );
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    continue;
                }

                let assign_stat = assign_ptr.to_node(root).ok_or(InferFailReason::None)?;
                let result_or_continue = get_type_at_assign_stat(
                    db,
                    tree,
                    cache,
                    root,
                    var_ref_id,
                    flow_node,
                    assign_stat,
                    policy,
                )?;

                if let ResultTypeOrContinue::Result(assign_type) = result_or_continue {
                    return finish_flow_walk_result(
                        db,
                        var_ref_id,
                        &pending_branch_types,
                        Ok(assign_type),
                    );
                } else {
                    if let Some(merged_type) = try_get_multi_antecedent_type(
                        db, tree, cache, root, var_ref_id, flow_node, policy,
                    )? {
                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(merged_type),
                        );
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                }
            }
            FlowNodeKind::Call(call_ptr) => {
                let call_expr = call_ptr.to_node(root).ok_or(InferFailReason::None)?;
                if call_expr_returns_never(db, cache, call_expr.clone()) {
                    return finish_flow_walk_result(
                        db,
                        var_ref_id,
                        &pending_branch_types,
                        Ok(LuaType::Never),
                    );
                }

                if let Some(effects) = db
                    .get_flow_index()
                    .get_special_call_effects(&cache.get_file_id(), call_expr.get_position())
                {
                    let mut effect_type = None;
                    for effect in effects {
                        if special_call_effect_matches_var_ref(&effect.target, var_ref_id) {
                            effect_type = Some(match effect_type {
                                Some(current_type) => {
                                    TypeOps::Union.apply(db, &current_type, &effect.type_ref)
                                }
                                None => effect.type_ref.clone(),
                            });
                        }
                    }
                    if let Some(effect_type) = effect_type {
                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(effect_type),
                        );
                    }
                }

                if let Some(populated_type) = try_get_numeric_range_table_arg_population_type(
                    db,
                    cache,
                    root,
                    var_ref_id,
                    call_expr.clone(),
                )? {
                    return finish_flow_walk_result(
                        db,
                        var_ref_id,
                        &pending_branch_types,
                        Ok(populated_type),
                    );
                }

                if numeric_table_index_query(db, cache, root, var_ref_id).is_some() {
                    return finish_flow_walk_result(
                        db,
                        var_ref_id,
                        &pending_branch_types,
                        get_var_ref_type(db, cache, var_ref_id),
                    );
                }

                if let Some(merged_type) = try_get_multi_antecedent_type(
                    db, tree, cache, root, var_ref_id, flow_node, policy,
                )? {
                    return finish_flow_walk_result(
                        db,
                        var_ref_id,
                        &pending_branch_types,
                        Ok(merged_type),
                    );
                }
                antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
            }
            FlowNodeKind::ImplFunc(func_ptr) => {
                let func_stat = func_ptr.to_node(root).ok_or(InferFailReason::None)?;
                let Some(func_name) = func_stat.get_func_name() else {
                    if let Some(merged_type) = try_get_multi_antecedent_type(
                        db, tree, cache, root, var_ref_id, flow_node, policy,
                    )? {
                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(merged_type),
                        );
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    continue;
                };

                let Some(ref_id) = get_var_expr_var_ref_id(db, cache, func_name.to_expr()) else {
                    if let Some(merged_type) = try_get_multi_antecedent_type(
                        db, tree, cache, root, var_ref_id, flow_node, policy,
                    )? {
                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(merged_type),
                        );
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    continue;
                };

                if ref_id == *var_ref_id {
                    if matches!(var_ref_id, VarRefId::VarRef(_)) {
                        let Some(closure) = func_stat.get_closure() else {
                            return Err(InferFailReason::None);
                        };

                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(LuaType::Signature(LuaSignatureId::from_closure(
                                cache.get_file_id(),
                                &closure,
                            ))),
                        );
                    }

                    // Only use the func-stat's signature when the member isn't
                    // already declared (origin type is Nil). For Def types with
                    // @field annotations, let the flow continue so the declared
                    // type is preserved instead of being overridden by the
                    // implementation signature.
                    let is_undeclared = cache
                        .get_index_ref_origin_type_cache(var_ref_id)
                        .is_some_and(|entry| matches!(entry, CacheEntry::Cache(t) if t.is_nil()));

                    if is_undeclared {
                        let Some(closure) = func_stat.get_closure() else {
                            return Err(InferFailReason::None);
                        };

                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(LuaType::Signature(LuaSignatureId::from_closure(
                                cache.get_file_id(),
                                &closure,
                            ))),
                        );
                    }

                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                } else {
                    if let Some(merged_type) = try_get_multi_antecedent_type(
                        db, tree, cache, root, var_ref_id, flow_node, policy,
                    )? {
                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(merged_type),
                        );
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                }
            }
            FlowNodeKind::TrueCondition(condition_ptr) => {
                if policy.is_closure_baseline() {
                    if let Some(condition) = condition_ptr.to_node(root)
                        && let Ok(Some(condition_type)) = get_type_at_immutable_closure_condition(
                            db,
                            tree,
                            cache,
                            root,
                            var_ref_id,
                            flow_node,
                            condition,
                            InferConditionFlow::TrueCondition,
                            policy,
                        )
                    {
                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(condition_type),
                        );
                    }
                    if let Some(merged_type) = try_get_multi_antecedent_type(
                        db, tree, cache, root, var_ref_id, flow_node, policy,
                    )? {
                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(merged_type),
                        );
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    continue;
                }

                let condition = condition_ptr.to_node(root).ok_or(InferFailReason::None)?;
                // Errors in condition evaluation (e.g. method not found) must not
                // propagate and corrupt the type of unrelated variables.  Treat them
                // as "condition cannot be used for narrowing" and fall through.
                let result_or_continue = match get_type_at_condition_flow(
                    db,
                    tree,
                    cache,
                    root,
                    var_ref_id,
                    flow_node,
                    condition,
                    InferConditionFlow::TrueCondition,
                    policy,
                ) {
                    Ok(r) => r,
                    Err(_) => ResultTypeOrContinue::Continue,
                };

                if let ResultTypeOrContinue::Result(condition_type) = result_or_continue {
                    return finish_flow_walk_result(
                        db,
                        var_ref_id,
                        &pending_branch_types,
                        Ok(condition_type),
                    );
                } else {
                    if let Some(merged_type) = try_get_multi_antecedent_type(
                        db, tree, cache, root, var_ref_id, flow_node, policy,
                    )? {
                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(merged_type),
                        );
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                }
            }
            FlowNodeKind::FalseCondition(condition_ptr) => {
                if policy.is_closure_baseline() {
                    if let Some(condition) = condition_ptr.to_node(root)
                        && let Ok(Some(condition_type)) = get_type_at_immutable_closure_condition(
                            db,
                            tree,
                            cache,
                            root,
                            var_ref_id,
                            flow_node,
                            condition,
                            InferConditionFlow::FalseCondition,
                            policy,
                        )
                    {
                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(condition_type),
                        );
                    }
                    if let Some(merged_type) = try_get_multi_antecedent_type(
                        db, tree, cache, root, var_ref_id, flow_node, policy,
                    )? {
                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(merged_type),
                        );
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    continue;
                }

                let condition = condition_ptr.to_node(root).ok_or(InferFailReason::None)?;
                // Same defensive handling as TrueCondition above.
                let result_or_continue = match get_type_at_condition_flow(
                    db,
                    tree,
                    cache,
                    root,
                    var_ref_id,
                    flow_node,
                    condition,
                    InferConditionFlow::FalseCondition,
                    policy,
                ) {
                    Ok(r) => r,
                    Err(_) => ResultTypeOrContinue::Continue,
                };

                if let ResultTypeOrContinue::Result(condition_type) = result_or_continue {
                    return finish_flow_walk_result(
                        db,
                        var_ref_id,
                        &pending_branch_types,
                        Ok(condition_type),
                    );
                } else {
                    if let Some(merged_type) = try_get_multi_antecedent_type(
                        db, tree, cache, root, var_ref_id, flow_node, policy,
                    )? {
                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(merged_type),
                        );
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                }
            }
            FlowNodeKind::ForIStat(_) => {
                // todo check for `for i = 1, 10 do end`
                if let Some(merged_type) = try_get_multi_antecedent_type(
                    db, tree, cache, root, var_ref_id, flow_node, policy,
                )? {
                    return finish_flow_walk_result(
                        db,
                        var_ref_id,
                        &pending_branch_types,
                        Ok(merged_type),
                    );
                }
                antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
            }
            FlowNodeKind::TagCast(cast_ast_ptr) => {
                let tag_cast = cast_ast_ptr.to_node(root).ok_or(InferFailReason::None)?;
                let cast_or_continue = get_type_at_cast_flow(
                    db, tree, cache, root, var_ref_id, flow_node, tag_cast, policy,
                )?;

                if let ResultTypeOrContinue::Result(cast_type) = cast_or_continue {
                    return finish_flow_walk_result(
                        db,
                        var_ref_id,
                        &pending_branch_types,
                        Ok(cast_type),
                    );
                } else {
                    if let Some(merged_type) = try_get_multi_antecedent_type(
                        db, tree, cache, root, var_ref_id, flow_node, policy,
                    )? {
                        return finish_flow_walk_result(
                            db,
                            var_ref_id,
                            &pending_branch_types,
                            Ok(merged_type),
                        );
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                }
            }
        }
    }
}

fn finish_flow_walk_result(
    db: &DbIndex,
    var_ref_id: &VarRefId,
    pending_branch_types: &[LuaType],
    result: InferResult,
) -> InferResult {
    result.map(|typ| {
        if pending_branch_types.is_empty() {
            typ
        } else {
            let mut branch_types = Vec::with_capacity(pending_branch_types.len() + 1);
            branch_types.extend_from_slice(pending_branch_types);
            branch_types.push(typ);
            merge_flow_branch_types(db, var_ref_id, branch_types)
        }
    })
}

fn get_decl_position_var_ref_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    var_ref_id: &VarRefId,
) -> InferResult {
    if let Some(decl_id) = var_ref_id.get_decl_id_ref()
        && let Some(decl) = db.get_decl_index().get_decl(&decl_id)
    {
        if decl.is_param()
            && let Some(child_type) = super::unguarded_child_decl_type(db, decl.get_id())
        {
            return Ok(child_type);
        }

        if decl.is_param()
            && let Ok(param_type) = infer_param_with_cache(db, cache, decl)
        {
            return Ok(param_type);
        }

        if decl.is_global()
            && db
                .get_type_index()
                .get_type_cache(&decl.get_id().into())
                .is_some_and(|type_cache| type_cache.as_type().is_nil())
            && let Some(global_type) = typed_global_nil_placeholder_type(db, cache, var_ref_id)
        {
            return Ok(global_type);
        }
    }

    get_var_ref_type(db, cache, var_ref_id)
}

fn should_retry_decl_initializer_type(typ: &LuaType) -> bool {
    typ.is_unknown() || matches!(typ, LuaType::String) || typ.contain_tpl()
}

fn with_flow_query_realm<T>(
    cache: &mut LuaInferCache,
    query_realm: GmodRealm,
    f: impl FnOnce(&mut LuaInferCache) -> T,
) -> T {
    let previous = cache.flow_query_realm.replace(query_realm);
    let result = f(cache);
    cache.flow_query_realm = previous;
    result
}

fn should_treat_unresolved_decl_as_nil(db: &DbIndex, decl_id: crate::LuaDeclId) -> bool {
    let Some(decl) = db.get_decl_index().get_decl(&decl_id) else {
        return false;
    };

    if !matches!(decl.extra, crate::LuaDeclExtra::Local { .. }) {
        return false;
    }

    if decl.has_initializer() {
        return false;
    }

    db.get_type_index()
        .get_type_cache(&decl_id.into())
        .is_none()
        || should_defer_uninitialized_local_decl_type(db, decl_id)
}

fn should_defer_uninitialized_local_decl_type(db: &DbIndex, decl_id: crate::LuaDeclId) -> bool {
    let Some(decl) = db.get_decl_index().get_decl(&decl_id) else {
        return false;
    };

    if !matches!(decl.extra, crate::LuaDeclExtra::Local { .. }) {
        return false;
    }

    if decl.has_initializer() {
        return false;
    }

    if db
        .get_property_index()
        .get_property(&LuaSemanticDeclId::LuaDecl(decl_id))
        .and_then(|property| property.find_attribute_use("lsp_optimization"))
        .and_then(|attr| attr.get_param_by_name("code"))
        .is_some_and(|param| matches!(param, LuaType::DocStringConst(code) if code.as_ref() == "delayed_definition"))
    {
        return false;
    }

    if !db
        .get_reference_index()
        .get_decl_references(&decl_id.file_id, &decl_id)
        .is_some_and(|decl_refs| decl_refs.mutable)
    {
        return false;
    }

    let Some(type_cache) = db.get_type_index().get_type_cache(&decl_id.into()) else {
        return false;
    };

    // Mutable uninitialized locals may get an inferred type from later assignments.
    // At the declaration point this type is not yet guaranteed, so keep the value
    // unresolved and let branch merge handling map it to nil when appropriate.
    type_cache.is_infer() && !type_cache.as_type().is_nil()
}

fn try_get_multi_antecedent_type(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    policy: FlowWalkPolicy,
) -> Result<Option<LuaType>, InferFailReason> {
    match flow_node.antecedent {
        Some(crate::FlowAntecedent::Multiple(_)) => Ok(Some(merge_antecedent_types(
            db, tree, cache, root, var_ref_id, flow_node, policy,
        )?)),
        _ => Ok(None),
    }
}

fn get_antecedent_type_for_flow_node(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    policy: FlowWalkPolicy,
) -> InferResult {
    if let Some(merged_type) =
        try_get_multi_antecedent_type(db, tree, cache, root, var_ref_id, flow_node, policy)?
    {
        return Ok(merged_type);
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    get_type_at_flow_in_mode(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        antecedent_flow_id,
        policy,
    )
}

fn merge_antecedent_types(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    policy: FlowWalkPolicy,
) -> InferResult {
    let single_antecedent;
    let antecedents = match &flow_node.antecedent {
        Some(FlowAntecedent::Single(id)) => {
            single_antecedent = [*id];
            &single_antecedent[..]
        }
        Some(FlowAntecedent::Multiple(multi_id)) => tree
            .get_multi_antecedents(*multi_id)
            .ok_or(InferFailReason::None)?,
        None => return Err(InferFailReason::None),
    };
    let target_realm = cache.flow_query_realm.unwrap_or_else(|| {
        db.get_gmod_infer_index()
            .get_realm_at_offset(&cache.get_file_id(), var_ref_id.get_position())
    });

    let mut accepted_any = false;
    let mut branch_types = Vec::with_capacity(antecedents.len());
    for &flow_id in antecedents {
        let Some(antecedent_node) = tree.get_flow_node(flow_id) else {
            continue;
        };
        if matches!(
            antecedent_node.kind,
            FlowNodeKind::Unreachable | FlowNodeKind::Return | FlowNodeKind::Break
        ) || call_flow_node_returns_never(db, cache, root, antecedent_node)
        {
            continue;
        }

        let antecedent_realm =
            get_or_compute_flow_node_realm(db, cache, root, flow_id, antecedent_node);
        if !realms_can_reach(target_realm, antecedent_realm) {
            continue;
        }

        accepted_any = true;
        let branch_type = with_flow_query_realm(cache, target_realm, |cache| {
            get_merged_flow_type_or_nil(db, tree, cache, root, var_ref_id, flow_id, policy)
        })?;
        if branch_type.is_unknown() {
            return Ok(LuaType::Unknown);
        }
        branch_types.push(branch_type);
    }

    if accepted_any {
        return Ok(merge_flow_branch_types(db, var_ref_id, branch_types));
    }

    let mut branch_types = Vec::with_capacity(antecedents.len());
    for &flow_id in antecedents {
        let Some(antecedent_node) = tree.get_flow_node(flow_id) else {
            continue;
        };
        if matches!(
            antecedent_node.kind,
            FlowNodeKind::Unreachable | FlowNodeKind::Return | FlowNodeKind::Break
        ) || call_flow_node_returns_never(db, cache, root, antecedent_node)
        {
            continue;
        }

        let branch_type = with_flow_query_realm(cache, target_realm, |cache| {
            get_merged_flow_type_or_nil(db, tree, cache, root, var_ref_id, flow_id, policy)
        })?;
        if branch_type.is_unknown() {
            return Ok(LuaType::Unknown);
        }
        branch_types.push(branch_type);
    }

    Ok(merge_flow_branch_types(db, var_ref_id, branch_types))
}

fn call_flow_node_returns_never(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    flow_node: &FlowNode,
) -> bool {
    let FlowNodeKind::Call(call_ptr) = &flow_node.kind else {
        return false;
    };
    let Some(call_expr) = call_ptr.to_node(root) else {
        return false;
    };
    call_expr_returns_never(db, cache, call_expr)
}

fn call_expr_returns_never(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: glua_parser::LuaCallExpr,
) -> bool {
    if call_expr.is_error() {
        return true;
    }

    match call_expr.get_prefix_expr() {
        Some(LuaExpr::NameExpr(name_expr)) => db
            .get_reference_index()
            .get_local_reference(&cache.get_file_id())
            .and_then(|local_ref| local_ref.get_decl_id(&name_expr.get_range()))
            .is_some_and(|decl_id| {
                semantic_decl_returns_never(db, LuaSemanticDeclId::LuaDecl(decl_id))
            }),
        Some(LuaExpr::IndexExpr(index_expr)) => {
            index_call_prefix_returns_never(db, cache, index_expr)
        }
        _ => false,
    }
}

fn index_call_prefix_returns_never(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: LuaIndexExpr,
) -> bool {
    if semantic_decl_returns_never(
        db,
        LuaSemanticDeclId::Member(LuaMemberId::new(
            index_expr.get_syntax_id(),
            cache.get_file_id(),
        )),
    ) {
        return true;
    }

    let Some(LuaExpr::NameExpr(prefix_name)) = index_expr.get_prefix_expr() else {
        return false;
    };
    if !name_expr_is_global_root(db, cache, &prefix_name) {
        return false;
    }

    let Some(global_name) = prefix_name.get_name_text() else {
        return false;
    };
    let Some(member_key) = index_expr
        .get_index_key()
        .and_then(|key| LuaMemberKey::from_index_key(db, cache, &key).ok())
    else {
        return false;
    };

    let owner = LuaMemberOwner::GlobalPath(GlobalId::new(&global_name));
    db.get_member_index()
        .get_member_item(&owner, &member_key)
        .and_then(|member_item| {
            member_item.resolve_semantic_decl_with_realm_at_offset(
                db,
                &cache.get_file_id(),
                index_expr.get_position(),
            )
        })
        .is_some_and(|semantic_decl| semantic_decl_returns_never(db, semantic_decl))
}

fn name_expr_is_global_root(
    db: &DbIndex,
    cache: &LuaInferCache,
    name_expr: &glua_parser::LuaNameExpr,
) -> bool {
    let Some(decl_id) = db
        .get_reference_index()
        .get_var_reference_decl(&cache.get_file_id(), name_expr.get_range())
    else {
        return true;
    };

    db.get_decl_index()
        .get_decl(&decl_id)
        .is_some_and(|decl| decl.is_global() || decl.is_module_scoped())
}

fn semantic_decl_returns_never(db: &DbIndex, semantic_decl: LuaSemanticDeclId) -> bool {
    if let Some(signature_id) = db.get_property_index().get_signature_owner(&semantic_decl) {
        return signature_returns_never(db, signature_id);
    }

    match semantic_decl {
        LuaSemanticDeclId::Signature(signature_id) => signature_returns_never(db, signature_id),
        LuaSemanticDeclId::LuaDecl(decl_id) => db
            .get_type_index()
            .get_type_cache(&decl_id.into())
            .is_some_and(|type_cache| type_returns_never(db, type_cache.as_type())),
        LuaSemanticDeclId::Member(member_id) => db
            .get_type_index()
            .get_type_cache(&member_id.into())
            .is_some_and(|type_cache| type_returns_never(db, type_cache.as_type())),
        LuaSemanticDeclId::TypeDecl(_) => false,
    }
}

fn signature_returns_never(db: &DbIndex, signature_id: LuaSignatureId) -> bool {
    db.get_signature_index()
        .get(&signature_id)
        .is_some_and(|signature| signature.get_return_type().is_never())
}

fn type_returns_never(db: &DbIndex, typ: &LuaType) -> bool {
    match typ {
        LuaType::Signature(signature_id) => signature_returns_never(db, *signature_id),
        LuaType::DocFunction(func) => func.get_ret().is_never(),
        _ => false,
    }
}

fn merge_flow_branch_types(
    db: &DbIndex,
    var_ref_id: &VarRefId,
    mut branch_types: Vec<LuaType>,
) -> LuaType {
    if branch_types.is_empty() {
        return LuaType::Never;
    }

    if !var_ref_has_explicit_any(db, var_ref_id)
        && branch_types.iter().any(is_table_shape_type)
        && branch_types
            .iter()
            .all(is_inferred_any_or_table_shape_branch)
    {
        let concrete_types = branch_types
            .iter()
            .filter(|typ| !is_bare_any_branch(typ))
            .cloned()
            .collect::<Vec<_>>();
        if !concrete_types.is_empty() {
            branch_types = concrete_types;
        }
    }

    let mut result_type = LuaType::Never;
    for branch_type in branch_types {
        result_type = TypeOps::Union.apply(db, &result_type, &branch_type);
    }
    result_type
}

fn var_ref_has_explicit_any(db: &DbIndex, var_ref_id: &VarRefId) -> bool {
    let type_cache = var_ref_id
        .get_decl_id_ref()
        .map(|decl_id| decl_id.into())
        .or_else(|| {
            var_ref_id
                .get_member_id_ref()
                .map(|member_id| member_id.into())
        })
        .and_then(|owner| db.get_type_index().get_type_cache(&owner));

    type_cache.is_some_and(|cache| cache.is_doc() && type_contains_bare_any(cache.as_type()))
}

fn type_contains_bare_any(typ: &LuaType) -> bool {
    match typ {
        LuaType::Any => true,
        LuaType::Union(union) => union.types().any(type_contains_bare_any),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .any(|(typ, _)| type_contains_bare_any(typ)),
        _ => false,
    }
}

fn is_bare_any_branch(typ: &LuaType) -> bool {
    matches!(typ, LuaType::Any)
}

fn is_inferred_any_or_table_shape_branch(typ: &LuaType) -> bool {
    is_bare_any_branch(typ)
        || typ.is_nil()
        || typ.is_never()
        || is_table_shape_type(typ)
        || matches!(typ, LuaType::Union(union) if union
            .types()
            .all(is_inferred_any_or_table_shape_branch))
}

fn is_table_shape_type(typ: &LuaType) -> bool {
    typ.is_table() || matches!(typ, LuaType::Object(_) | LuaType::Instance(_))
}

fn get_merged_flow_type_or_nil(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_id: FlowId,
    policy: FlowWalkPolicy,
) -> InferResult {
    match get_type_at_flow_in_mode(db, tree, cache, root, var_ref_id, flow_id, policy) {
        Ok(t) => Ok(t),
        Err(InferFailReason::UnResolveDeclType(decl_id))
            if should_treat_unresolved_decl_as_nil(db, decl_id) =>
        {
            Ok(LuaType::Nil)
        }
        Err(e) => Err(e),
    }
}

fn get_flow_node_realm(
    db: &DbIndex,
    file_id: crate::FileId,
    root: &LuaChunk,
    flow_node: &FlowNode,
) -> GmodRealm {
    let gmod_infer = db.get_gmod_infer_index();
    let file_realm = gmod_infer.get_realm_at_offset(&file_id, TextSize::new(0));

    let offset = match &flow_node.kind {
        FlowNodeKind::DeclPosition(position) | FlowNodeKind::ClosureEntry(position) => {
            Some(*position)
        }
        FlowNodeKind::Assignment(assign_ptr, _) => {
            assign_ptr.to_node(root).map(|node| node.get_position())
        }
        FlowNodeKind::Call(call_ptr) => call_ptr.to_node(root).map(|node| node.get_position()),
        FlowNodeKind::TrueCondition(condition_ptr)
        | FlowNodeKind::FalseCondition(condition_ptr) => {
            condition_ptr.to_node(root).map(|node| node.get_position())
        }
        FlowNodeKind::ImplFunc(func_ptr) => func_ptr.to_node(root).map(|node| node.get_position()),
        FlowNodeKind::ForIStat(for_stat_ptr) => {
            for_stat_ptr.to_node(root).map(|node| node.get_position())
        }
        FlowNodeKind::TagCast(cast_ptr) => cast_ptr.to_node(root).map(|node| node.get_position()),
        FlowNodeKind::Start
        | FlowNodeKind::Unreachable
        | FlowNodeKind::BranchLabel
        | FlowNodeKind::LoopLabel
        | FlowNodeKind::NamedLabel(_)
        | FlowNodeKind::Break
        | FlowNodeKind::Return => None,
    };

    offset.map_or(file_realm, |position| {
        gmod_infer.get_realm_at_offset(&file_id, position)
    })
}

fn get_or_compute_flow_node_realm(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    flow_id: FlowId,
    flow_node: &FlowNode,
) -> GmodRealm {
    if let Some(realm) = cache.flow_node_realm_cache.get(&flow_id) {
        return *realm;
    }

    let realm = get_flow_node_realm(db, cache.get_file_id(), root, flow_node);
    cache.flow_node_realm_cache.insert(flow_id, realm);
    realm
}

fn realms_can_reach(target: GmodRealm, source: GmodRealm) -> bool {
    match target {
        GmodRealm::Unknown | GmodRealm::Shared => true,
        GmodRealm::Server => matches!(
            source,
            GmodRealm::Server | GmodRealm::Shared | GmodRealm::Unknown
        ),
        GmodRealm::Client => matches!(
            source,
            GmodRealm::Client | GmodRealm::Shared | GmodRealm::Unknown
        ),
        GmodRealm::Menu => matches!(source, GmodRealm::Menu | GmodRealm::Unknown),
    }
}

fn all_branch_antecedents_alive(tree: &FlowTree, flow_node: &FlowNode) -> bool {
    match &flow_node.antecedent {
        Some(FlowAntecedent::Multiple(idx)) => {
            if let Some(antecedents) = tree.get_multi_antecedents(*idx) {
                antecedents.iter().all(|&fid| {
                    tree.get_flow_node(fid).is_some_and(|n| {
                        !matches!(
                            n.kind,
                            FlowNodeKind::Unreachable | FlowNodeKind::Return | FlowNodeKind::Break
                        )
                    })
                })
            } else {
                false
            }
        }
        _ => false,
    }
}

fn branch_has_relevant_special_call_effects(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &LuaInferCache,
    root: &LuaChunk,
    flow_node: &FlowNode,
    stop_at: FlowId,
    var_ref_id: &VarRefId,
) -> bool {
    let Some(FlowAntecedent::Multiple(idx)) = &flow_node.antecedent else {
        return false;
    };
    let Some(antecedents) = tree.get_multi_antecedents(*idx) else {
        return false;
    };

    let mut visited = HashSet::new();
    antecedents.iter().copied().any(|flow_id| {
        antecedent_has_relevant_special_call_effect(
            db,
            tree,
            cache,
            root,
            flow_id,
            stop_at,
            var_ref_id,
            &mut visited,
        )
    })
}

fn branch_can_narrow_var_ref(
    db: &DbIndex,
    info: &crate::BranchLabelInfo,
    var_ref_id: &VarRefId,
) -> bool {
    if info.has_casts_or_implfunc {
        return true;
    }

    match var_ref_id {
        VarRefId::VarRef(decl_id) => {
            let Some(decl) = db.get_decl_index().get_decl(decl_id) else {
                return info.has_name_assigns;
            };
            branch_name_can_be_narrowed(info, decl.get_name())
        }
        VarRefId::SelfRef(_) => branch_name_can_be_narrowed(info, "self"),
        VarRefId::GlobalName(name, _) => info.narrowing_capability.name_can_be_narrowed(name),
        VarRefId::IndexRef(root, path) => {
            info.narrowing_capability.index_path_can_be_narrowed(path)
                || root
                    .as_decl_id()
                    .and_then(|decl_id| db.get_decl_index().get_decl(&decl_id))
                    .is_some_and(|decl| branch_name_can_be_narrowed(info, decl.get_name()))
        }
    }
}

fn branch_name_can_be_narrowed(info: &crate::BranchLabelInfo, name: &str) -> bool {
    info.narrowing_capability.has_opaque_name_target
        || info
            .narrowing_capability
            .referenced_names
            .iter()
            .any(|narrowable_name| narrowable_name.as_str() == name)
}

fn antecedent_has_relevant_special_call_effect(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &LuaInferCache,
    root: &LuaChunk,
    flow_id: FlowId,
    stop_at: FlowId,
    var_ref_id: &VarRefId,
    visited: &mut HashSet<FlowId>,
) -> bool {
    if flow_id == stop_at || !visited.insert(flow_id) {
        return false;
    }

    let Some(flow_node) = tree.get_flow_node(flow_id) else {
        return false;
    };

    if let FlowNodeKind::Call(call_ptr) = &flow_node.kind
        && let Some(call_expr_stat) = call_ptr.to_node(root)
        && let Some(effects) = db
            .get_flow_index()
            .get_special_call_effects(&cache.get_file_id(), call_expr_stat.get_position())
        && effects
            .iter()
            .any(|effect| special_call_effect_matches_var_ref(&effect.target, var_ref_id))
    {
        return true;
    }

    match &flow_node.antecedent {
        Some(FlowAntecedent::Single(prev)) => antecedent_has_relevant_special_call_effect(
            db, tree, cache, root, *prev, stop_at, var_ref_id, visited,
        ),
        Some(FlowAntecedent::Multiple(idx)) => {
            tree.get_multi_antecedents(*idx).is_some_and(|prevs| {
                prevs.iter().copied().any(|prev| {
                    antecedent_has_relevant_special_call_effect(
                        db, tree, cache, root, prev, stop_at, var_ref_id, visited,
                    )
                })
            })
        }
        None => false,
    }
}

fn special_call_effect_matches_var_ref(effect_target: &VarRefId, var_ref_id: &VarRefId) -> bool {
    effect_target == var_ref_id
        || matches!(
            (effect_target, var_ref_id),
            (VarRefId::SelfRef(effect_self), VarRefId::IndexRef(root, _))
                if root.receiver_eq(&effect_self.receiver)
        )
}

fn get_type_at_assign_stat(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    assign_stat: LuaAssignStat,
    policy: FlowWalkPolicy,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let (vars, exprs) = assign_stat.get_var_and_expr_list();
    for (i, var) in vars.iter().cloned().enumerate() {
        if let Some(prefix_collection_type) = maybe_get_collection_append_assignment_type(
            db, tree, cache, root, var_ref_id, flow_node, &var, &exprs, i, policy,
        )? {
            return Ok(ResultTypeOrContinue::Result(prefix_collection_type));
        }

        let Some(maybe_ref_id) = get_var_expr_var_ref_id(db, cache, var.to_expr()) else {
            continue;
        };

        if numeric_table_index_query_key_name(var_ref_id)
            .map(str::to_string)
            .or_else(|| numeric_table_index_query_key_name_from_initializer(db, root, var_ref_id))
            .is_some_and(|key_name| var_ref_is_name(db, &maybe_ref_id, &key_name))
        {
            return Ok(ResultTypeOrContinue::Result(LuaType::Nil));
        }

        if let Some((query_root, _query_index)) =
            numeric_table_index_query(db, cache, root, var_ref_id)
            && var_ref_matches_root(&maybe_ref_id, &query_root)
        {
            return Ok(ResultTypeOrContinue::Result(get_var_ref_type(
                db, cache, var_ref_id,
            )?));
        }

        if let Some((query_root, query_index)) =
            numeric_table_index_query(db, cache, root, var_ref_id)
            && let LuaVarExpr::IndexExpr(index_expr) = &var
            && let VarRefId::IndexRef(assign_root, _) = &maybe_ref_id
            && *assign_root == query_root
            && index_expr_numeric_key_value(db, cache, index_expr) == Some(query_index)
        {
            if assignment_vars_write_root(db, cache, &vars, &query_root) {
                return Ok(ResultTypeOrContinue::Result(get_var_ref_type(
                    db, cache, var_ref_id,
                )?));
            }
            if numeric_table_index_query_key_name(var_ref_id)
                .map(str::to_string)
                .or_else(|| {
                    numeric_table_index_query_key_name_from_initializer(db, root, var_ref_id)
                })
                .is_some_and(|key_name| {
                    assignment_vars_write_dynamic_key_name(db, cache, &vars, &key_name)
                })
            {
                return Ok(ResultTypeOrContinue::Result(LuaType::Nil));
            }
            let Some(expr_type) = infer_expr_list_value_type_at(db, cache, &exprs, i)? else {
                return Ok(ResultTypeOrContinue::Continue);
            };
            return Ok(ResultTypeOrContinue::Result(expr_type));
        }

        if maybe_ref_id != *var_ref_id {
            if var_ref_id.start_with(&maybe_ref_id)
                && let Some(expr_type) = infer_expr_list_value_type_at(db, cache, &exprs, i)?
                && let Some(member_type) =
                    assigned_prefix_member_type(db, cache, var_ref_id, &maybe_ref_id, &expr_type)?
            {
                return Ok(ResultTypeOrContinue::Result(member_type));
            }

            continue;
        }

        if numeric_table_index_query_key_name(var_ref_id)
            .map(str::to_string)
            .or_else(|| numeric_table_index_query_key_name_from_initializer(db, root, var_ref_id))
            .is_some_and(|key_name| {
                assignment_vars_write_dynamic_key_name(db, cache, &vars, &key_name)
            })
        {
            return Ok(ResultTypeOrContinue::Result(LuaType::Nil));
        }

        // Check if there's an explicit type annotation (not just inferred type)
        let type_owner = match &var {
            LuaVarExpr::NameExpr(name_expr) => {
                Some(LuaDeclId::new(cache.get_file_id(), name_expr.get_position()).into())
            }
            LuaVarExpr::IndexExpr(index_expr) => {
                Some(LuaMemberId::new(index_expr.get_syntax_id(), cache.get_file_id()).into())
            }
        };

        let explicit_var_type = type_owner
            .as_ref()
            .and_then(|id| db.get_type_index().get_type_cache(id))
            .filter(|tc| tc.is_doc())
            .map(|tc| tc.as_type().clone());

        let guarded_global_type = exprs.get(i).and_then(|expr| {
            guarded_global_self_assignment_type(db, cache, type_owner.as_ref(), &maybe_ref_id, expr)
        });

        let expr_type = match guarded_global_type {
            Some(typ) => Some(typ),
            None => infer_expr_list_value_type_at(db, cache, &exprs, i)?,
        };
        let Some(expr_type) = expr_type else {
            return Ok(ResultTypeOrContinue::Continue);
        };

        if explicit_var_type.is_none() && expr_type.is_unknown() {
            return Ok(ResultTypeOrContinue::Result(expr_type));
        }

        if explicit_var_type.is_none()
            && expr_type.is_nil()
            && typed_global_nil_placeholder_type(db, cache, &maybe_ref_id).is_some()
        {
            return Ok(ResultTypeOrContinue::Continue);
        }

        if explicit_var_type.is_none()
            && expr_type.is_nil()
            && is_untyped_param_rooted_index(db, &maybe_ref_id)
        {
            return Ok(ResultTypeOrContinue::Continue);
        }

        // Assignment is value REPLACEMENT, not condition refinement. When the RHS is a
        // fresh table-literal constructor, its table identity must replace the
        // antecedent's identity rather than being narrowed against it. Narrowing
        // (`narrow_down_type`) intentionally preserves the antecedent `TableConst`
        // identity for `TableConst -> TableConst`, which is correct for guards like
        // `if type(x) == "table"` but wrong for `x = {}`: it would keep the previous
        // table, collapsing every reassigned region of a reused local onto the first
        // table literal. Bypass narrowing for literal-table assignments (no explicit
        // doc `@type` override) so each region keeps its own table identity.
        let rhs_is_fresh_table_literal = explicit_var_type.is_none()
            && matches!(expr_type, LuaType::TableConst(_))
            && exprs.get(i).is_some_and(expr_is_table_constructor);
        let rhs_is_string_literal = explicit_var_type.is_none()
            && matches!(
                expr_type,
                LuaType::StringConst(_) | LuaType::DocStringConst(_)
            );
        let rhs_is_class_instance =
            explicit_var_type.is_none() && is_class_instance_type(db, &expr_type);
        let rhs_replaces_special_call_effect = explicit_var_type.is_none()
            && antecedent_has_relevant_special_call_effect_before_node(
                db, tree, cache, root, flow_node, var_ref_id,
            );

        let narrowed = if rhs_is_fresh_table_literal
            || rhs_is_string_literal
            || rhs_is_class_instance
            || rhs_replaces_special_call_effect
        {
            Some(expr_type.clone())
        } else {
            let source_type = if let Some(explicit) = explicit_var_type.clone() {
                explicit
            } else {
                match get_antecedent_type_for_flow_node(
                    db, tree, cache, root, var_ref_id, flow_node, policy,
                ) {
                    Ok(ty) => ty,
                    Err(InferFailReason::UnResolveDeclType(decl_id))
                        if should_treat_unresolved_decl_as_nil(db, decl_id) =>
                    {
                        LuaType::Nil
                    }
                    Err(err) => return Err(err),
                }
            };

            if source_type == LuaType::Nil {
                None
            } else {
                let declared = get_var_ref_type(db, cache, var_ref_id)
                    .ok()
                    .and_then(|decl| match decl {
                        LuaType::Def(_) | LuaType::Ref(_) => Some(decl),
                        _ => None,
                    });

                narrow_down_type(db, source_type.clone(), expr_type.clone(), declared)
            }
        };

        let mut result_type = narrowed.unwrap_or(explicit_var_type.unwrap_or(expr_type));

        if let Some(expr) = exprs.get(i) {
            if is_self_coalescing_or_expr(db, cache, &maybe_ref_id, expr) {
                result_type = prefer_table_of_over_bare_table(db, result_type);
            }
        }

        return Ok(ResultTypeOrContinue::Result(result_type));
    }

    Ok(ResultTypeOrContinue::Continue)
}

fn try_get_numeric_range_table_arg_population_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    call_expr: LuaCallExpr,
) -> Result<Option<LuaType>, InferFailReason> {
    let Some((query_root, access_index)) = numeric_table_index_query(db, cache, root, var_ref_id)
    else {
        return Ok(None);
    };
    let key_name = numeric_table_index_query_key_name(var_ref_id)
        .map(str::to_string)
        .or_else(|| numeric_table_index_query_key_name_from_initializer(db, root, var_ref_id));

    let args = call_expr
        .get_args_list()
        .map(|args| args.get_args().collect::<Vec<_>>())
        .unwrap_or_default();
    let Some((arg_index, _)) = args.iter().enumerate().find(|(_, arg)| {
        matches!(
            get_var_expr_var_ref_id(db, cache, (*arg).clone()),
            Some(VarRefId::VarRef(decl_id)) if query_root.as_decl_id() == Some(decl_id)
        )
    }) else {
        return Ok(None);
    };

    let Some(closure) = resolve_same_file_named_function_closure(db, cache, root, &call_expr)
    else {
        return Ok(None);
    };
    let Some(param_name) = closure
        .get_params_list()
        .and_then(|params| params.get_params().nth(arg_index))
        .and_then(|param| param.get_name_token())
        .map(|name| name.get_name_text().to_string())
    else {
        return Ok(None);
    };

    let Some(block) = closure.get_block() else {
        return Ok(None);
    };
    let stats = block.get_stats().collect::<Vec<_>>();
    let [LuaStat::ForStat(for_stat)] = stats.as_slice() else {
        return Ok(None);
    };
    if !numeric_for_bounds_cover_index(db, cache, for_stat, access_index) {
        return Ok(None);
    }

    let Some(for_var_name) = for_stat
        .get_var_name()
        .map(|name| name.get_name_text().to_string())
    else {
        return Ok(None);
    };
    let Some(for_block) = for_stat.get_block() else {
        return Ok(None);
    };
    let for_stats = for_block.get_stats().collect::<Vec<_>>();
    let [LuaStat::AssignStat(assign_stat)] = for_stats.as_slice() else {
        return Ok(None);
    };
    let (vars, exprs) = assign_stat.get_var_and_expr_list();
    let ([LuaVarExpr::IndexExpr(index_expr)], [rhs_expr]) = (vars.as_slice(), exprs.as_slice())
    else {
        return Ok(None);
    };
    if !index_expr_writes_param_at_for_var(index_expr, &param_name, &for_var_name) {
        return Ok(None);
    }
    if expr_calls_may_write_numeric_population_identity(
        db,
        cache,
        root,
        rhs_expr,
        &query_root,
        key_name.as_deref(),
        &mut HashSet::new(),
    ) {
        return Ok(None);
    }

    let rhs_type = infer_expr(db, cache, rhs_expr.clone())?;
    if rhs_type.is_nullable() || rhs_type.is_nil() {
        return Ok(None);
    }

    Ok(Some(rhs_type))
}

fn expr_calls_may_write_numeric_population_identity(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    expr: &LuaExpr,
    query_root: &crate::semantic::infer::narrow::var_ref_id::VarRefRootId,
    key_name: Option<&str>,
    active_closures: &mut HashSet<TextRange>,
) -> bool {
    let expr_range = expr.get_range();
    if let LuaExpr::CallExpr(call_expr) = expr
        && same_file_call_may_write_numeric_population_identity(
            db,
            cache,
            root,
            call_expr,
            query_root,
            key_name,
            active_closures,
        )
    {
        return true;
    }
    expr.descendants::<LuaCallExpr>().any(|call_expr| {
        call_is_inside_nested_closure_expr(&call_expr, expr_range)
            || same_file_call_may_write_numeric_population_identity(
                db,
                cache,
                root,
                &call_expr,
                query_root,
                key_name,
                active_closures,
            )
    })
}

fn same_file_call_may_write_numeric_population_identity(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    call_expr: &LuaCallExpr,
    query_root: &crate::semantic::infer::narrow::var_ref_id::VarRefRootId,
    key_name: Option<&str>,
    active_closures: &mut HashSet<TextRange>,
) -> bool {
    let Some(closure) = resolve_same_file_named_function_closure(db, cache, root, call_expr) else {
        return false;
    };
    let closure_range = closure.get_range();
    if !active_closures.insert(closure_range) {
        return true;
    }

    let may_write = closure_may_write_numeric_population_identity(
        db,
        cache,
        root,
        &closure,
        query_root,
        key_name,
        active_closures,
    );
    active_closures.remove(&closure_range);
    may_write
}

fn closure_may_write_numeric_population_identity(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    closure: &LuaClosureExpr,
    query_root: &crate::semantic::infer::narrow::var_ref_id::VarRefRootId,
    key_name: Option<&str>,
    active_closures: &mut HashSet<TextRange>,
) -> bool {
    let Some(block) = closure.get_block() else {
        return false;
    };

    for assign_stat in block.syntax().descendants().filter_map(LuaAssignStat::cast) {
        if node_is_inside_nested_closure(assign_stat.syntax(), block.syntax()) {
            continue;
        }
        let (vars, _) = assign_stat.get_var_and_expr_list();
        if assignment_vars_write_root(db, cache, &vars, query_root)
            || key_name.is_some_and(|key_name| {
                assignment_vars_write_dynamic_key_name(db, cache, &vars, key_name)
            })
        {
            return true;
        }
    }

    for nested_call in block.syntax().descendants().filter_map(LuaCallExpr::cast) {
        if node_is_inside_nested_closure(nested_call.syntax(), block.syntax()) {
            continue;
        }
        if same_file_call_may_write_numeric_population_identity(
            db,
            cache,
            root,
            &nested_call,
            query_root,
            key_name,
            active_closures,
        ) {
            return true;
        }
    }

    false
}

pub(crate) fn try_get_cross_file_numeric_range_population_type_for_index(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: &LuaIndexExpr,
) -> Option<LuaType> {
    let (table_global, access_index, key_name) =
        numeric_global_table_index_query(db, cache, index_expr)?;
    let reader_file_id = cache.get_file_id();
    let access_position = index_expr.get_position();
    let query_realm = db
        .get_gmod_infer_index()
        .get_realm_at_offset(&reader_file_id, access_position);
    for population in db
        .get_numeric_range_population_index()
        .get_for_global(&table_global)
    {
        if population.file_id == reader_file_id {
            continue;
        }
        if access_index < population.start || access_index > population.end {
            continue;
        }
        if let Some(key_name) = key_name.as_deref()
            && population.write_roots.iter().any(|root| root == key_name)
        {
            continue;
        }
        let Some((pop_pos, reader_pos, roots)) =
            population_load_positions(db, population.file_id, reader_file_id, query_realm)
        else {
            continue;
        };
        if pop_pos >= reader_pos {
            continue;
        }
        let mut mutation_roots = Vec::with_capacity(1 + population.alias_roots.len());
        mutation_roots.push(population.table_global.as_str());
        mutation_roots.extend(population.alias_roots.iter().map(String::as_str));
        if load_ordered_file_between_has_global_table_uncertainty(
            db,
            &roots,
            pop_pos,
            reader_pos,
            &mutation_roots,
        ) {
            continue;
        }
        if file_has_global_table_uncertainty_after(
            db,
            population.file_id,
            &mutation_roots,
            population.call_range.end(),
        ) {
            continue;
        }
        if read_inside_nested_closure(index_expr)
            && current_file_has_any_top_level_global_table_uncertainty(
                db,
                cache,
                index_expr,
                &mutation_roots,
            )
        {
            continue;
        }
        if current_scope_has_prior_global_table_uncertainty(db, cache, index_expr, &mutation_roots)
        {
            continue;
        }
        return Some(population.value_type.clone());
    }
    None
}

fn numeric_global_table_index_query(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: &LuaIndexExpr,
) -> Option<(String, i64, Option<String>)> {
    let LuaExpr::NameExpr(table_name) = index_expr.get_prefix_expr()? else {
        return None;
    };
    let table_global = table_name.get_name_text()?;
    if !db
        .get_numeric_range_population_index()
        .has_global(&table_global)
    {
        return None;
    }
    if !name_expr_is_global_root(db, cache, &table_name) {
        return None;
    }
    let access_index = index_expr_numeric_key_value(db, cache, index_expr)?;
    let key_name = match index_expr.get_index_key()? {
        LuaIndexKey::Expr(LuaExpr::NameExpr(name_expr)) => name_expr.get_name_text(),
        _ => None,
    };
    Some((table_global, access_index, key_name))
}

fn population_load_positions(
    db: &DbIndex,
    population_file_id: FileId,
    reader_file_id: FileId,
    query_realm: GmodRealm,
) -> Option<(
    usize,
    usize,
    Vec<(FileId, crate::GmodLoadRootKind, crate::GmodLoadOrderKey)>,
)> {
    let state = crate::GmodStateMask::from_realm(query_realm).as_caller_compatibility_mask();
    let roots = db.get_gmod_load_index().engine_roots_in_load_order(state);
    let pop_pos = roots
        .iter()
        .position(|(file_id, _, _)| *file_id == population_file_id);
    let reader_pos = roots
        .iter()
        .position(|(file_id, _, _)| *file_id == reader_file_id);
    Some((pop_pos?, reader_pos?, roots))
}

fn load_ordered_file_between_has_global_table_uncertainty(
    db: &DbIndex,
    roots: &[(FileId, crate::GmodLoadRootKind, crate::GmodLoadOrderKey)],
    pop_pos: usize,
    reader_pos: usize,
    mutation_roots: &[&str],
) -> bool {
    for (file_id, _, _) in &roots[pop_pos + 1..reader_pos] {
        let Some(tree) = db.get_vfs().get_syntax_tree(file_id) else {
            return true;
        };
        let Some(chunk) = LuaChunk::cast(tree.get_red_root()) else {
            return true;
        };
        let mut cache = LuaInferCache::new(*file_id, Default::default());
        if file_has_top_level_global_table_uncertainty(db, &mut cache, &chunk, mutation_roots) {
            return true;
        }
    }
    false
}

fn file_has_top_level_global_table_uncertainty(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    mutation_roots: &[&str],
) -> bool {
    let Some(block) = root.get_block() else {
        return true;
    };
    for stat in block.get_stats() {
        if top_level_stat_may_mutate_global_table(db, cache, &stat, mutation_roots) {
            return true;
        }
    }
    false
}

fn file_has_global_table_uncertainty_after(
    db: &DbIndex,
    file_id: FileId,
    mutation_roots: &[&str],
    after: TextSize,
) -> bool {
    let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) else {
        return true;
    };
    let Some(chunk) = LuaChunk::cast(tree.get_red_root()) else {
        return true;
    };
    let Some(block) = chunk.get_block() else {
        return true;
    };
    for stat in block.get_stats().filter(|stat| stat.get_position() > after) {
        let mut cache = LuaInferCache::new(file_id, Default::default());
        if top_level_stat_may_mutate_global_table(db, &mut cache, &stat, mutation_roots) {
            return true;
        }
    }
    false
}

fn read_inside_nested_closure(index_expr: &LuaIndexExpr) -> bool {
    index_expr
        .syntax()
        .ancestors()
        .any(|ancestor| LuaClosureExpr::can_cast(ancestor.kind().into()))
}

fn current_file_has_any_top_level_global_table_uncertainty(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: &LuaIndexExpr,
    mutation_roots: &[&str],
) -> bool {
    let Some(root) = LuaChunk::cast(index_expr.get_root()) else {
        return true;
    };
    file_has_top_level_global_table_uncertainty(db, cache, &root, mutation_roots)
}

fn current_scope_has_prior_global_table_uncertainty(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: &LuaIndexExpr,
    mutation_roots: &[&str],
) -> bool {
    let mut before = index_expr.get_position();
    if enclosing_control_flow_has_prior_call(db, cache, index_expr, before, mutation_roots) {
        return true;
    }
    for block in index_expr.syntax().ancestors().filter_map(LuaBlock::cast) {
        for stat in block
            .get_stats()
            .take_while(|stat| stat.get_position() < before)
        {
            if top_level_stat_may_mutate_global_table(db, cache, &stat, mutation_roots) {
                return true;
            }
        }
        before = block.get_position();
    }
    false
}

fn enclosing_control_flow_has_prior_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: &LuaIndexExpr,
    before: TextSize,
    mutation_roots: &[&str],
) -> bool {
    for stat in index_expr.syntax().ancestors().filter_map(LuaStat::cast) {
        if !matches!(
            stat,
            LuaStat::IfStat(_)
                | LuaStat::WhileStat(_)
                | LuaStat::RepeatStat(_)
                | LuaStat::ForStat(_)
                | LuaStat::ForRangeStat(_)
        ) {
            continue;
        }
        for call_expr in stat.descendants::<LuaCallExpr>() {
            if call_expr.get_position() < before
                && !node_is_inside_nested_closure(call_expr.syntax(), stat.syntax())
                && call_effect_overlaps_mutation_roots(db, cache, &call_expr, mutation_roots)
            {
                return true;
            }
        }
    }
    false
}

fn top_level_stat_may_mutate_global_table(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    stat: &LuaStat,
    mutation_roots: &[&str],
) -> bool {
    for call_expr in stat.descendants::<LuaCallExpr>() {
        if node_is_inside_nested_closure(call_expr.syntax(), stat.syntax()) {
            continue;
        }
        if call_effect_overlaps_mutation_roots(db, cache, &call_expr, mutation_roots) {
            return true;
        }
    }
    for assign_stat in stat.descendants::<LuaAssignStat>() {
        if node_is_inside_nested_closure(assign_stat.syntax(), stat.syntax()) {
            continue;
        }
        let (vars, _) = assign_stat.get_var_and_expr_list();
        if vars
            .iter()
            .any(|var| var_expr_may_mutate_global_table(var, mutation_roots))
        {
            return true;
        }
    }
    false
}

fn call_effect_overlaps_mutation_roots(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
    mutation_roots: &[&str],
) -> bool {
    let call_overlaps = match gmod_call_write_effect(db, cache, call_expr) {
        GmodCallWriteEffect::Globals(roots) => roots.iter().any(|root| {
            mutation_roots
                .iter()
                .any(|mutation_root| root == mutation_root)
        }),
        GmodCallWriteEffect::Unknown => same_file_call_effect_overlaps_mutation_roots(
            db,
            cache,
            call_expr,
            mutation_roots,
            &mut HashSet::new(),
        ),
    };
    call_overlaps
        || call_expr.get_args_list().is_some_and(|args| {
            args.get_args().any(|arg| {
                if let LuaExpr::CallExpr(ref nested_call) = arg
                    && call_effect_overlaps_mutation_roots(db, cache, &nested_call, mutation_roots)
                {
                    return true;
                }
                arg.descendants::<LuaCallExpr>().any(|nested_call| {
                    !node_is_inside_nested_closure(nested_call.syntax(), arg.syntax())
                        && call_effect_overlaps_mutation_roots(
                            db,
                            cache,
                            &nested_call,
                            mutation_roots,
                        )
                })
            })
        })
}

fn same_file_call_effect_overlaps_mutation_roots(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
    mutation_roots: &[&str],
    active_closures: &mut HashSet<TextRange>,
) -> bool {
    let Some(root) = LuaChunk::cast(call_expr.get_root()) else {
        return true;
    };
    let Some(closure) = resolve_same_file_named_function_closure(db, cache, &root, call_expr)
    else {
        return true;
    };
    let closure_range = closure.get_range();
    if !active_closures.insert(closure_range) {
        return true;
    }

    let overlaps = closure_effect_overlaps_mutation_roots(
        db,
        cache,
        &root,
        &closure,
        mutation_roots,
        active_closures,
    );
    active_closures.remove(&closure_range);
    overlaps
}

fn closure_effect_overlaps_mutation_roots(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    closure: &LuaClosureExpr,
    mutation_roots: &[&str],
    active_closures: &mut HashSet<TextRange>,
) -> bool {
    let Some(block) = closure.get_block() else {
        return true;
    };

    for assign_stat in block.syntax().descendants().filter_map(LuaAssignStat::cast) {
        if node_is_inside_nested_closure(assign_stat.syntax(), block.syntax()) {
            continue;
        }
        let (vars, _) = assign_stat.get_var_and_expr_list();
        if vars
            .iter()
            .any(|var| var_expr_may_mutate_global_table(var, mutation_roots))
        {
            return true;
        }
    }

    for nested_call in block.syntax().descendants().filter_map(LuaCallExpr::cast) {
        if node_is_inside_nested_closure(nested_call.syntax(), block.syntax()) {
            continue;
        }
        let call_overlaps = match gmod_call_write_effect(db, cache, &nested_call) {
            GmodCallWriteEffect::Globals(roots) => roots.iter().any(|root| {
                mutation_roots
                    .iter()
                    .any(|mutation_root| root == mutation_root)
            }),
            GmodCallWriteEffect::Unknown => {
                let Some(closure) =
                    resolve_same_file_named_function_closure(db, cache, root, &nested_call)
                else {
                    return true;
                };
                let closure_range = closure.get_range();
                if !active_closures.insert(closure_range) {
                    return true;
                }
                let overlaps = closure_effect_overlaps_mutation_roots(
                    db,
                    cache,
                    root,
                    &closure,
                    mutation_roots,
                    active_closures,
                );
                active_closures.remove(&closure_range);
                overlaps
            }
        };
        if call_overlaps {
            return true;
        }
    }

    false
}

fn node_is_inside_nested_closure(
    node: &glua_parser::LuaSyntaxNode,
    boundary: &glua_parser::LuaSyntaxNode,
) -> bool {
    node.ancestors()
        .take_while(|ancestor| ancestor != boundary)
        .any(|ancestor| LuaClosureExpr::can_cast(ancestor.kind().into()))
}

fn var_expr_may_mutate_global_table(var: &LuaVarExpr, mutation_roots: &[&str]) -> bool {
    match var {
        LuaVarExpr::NameExpr(name_expr) => name_expr
            .get_name_text()
            .is_some_and(|name| mutation_roots.iter().any(|root| *root == name)),
        LuaVarExpr::IndexExpr(index_expr) => index_expr_global_root_name(index_expr)
            .is_some_and(|name| mutation_roots.iter().any(|root| *root == name)),
    }
}

fn index_expr_global_root_name(index_expr: &LuaIndexExpr) -> Option<String> {
    let mut prefix = index_expr.get_prefix_expr()?;
    while let LuaExpr::IndexExpr(parent_index) = prefix {
        prefix = parent_index.get_prefix_expr()?;
    }
    let LuaExpr::NameExpr(name_expr) = prefix else {
        return None;
    };
    name_expr.get_name_text()
}

fn numeric_table_index_query(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
) -> Option<(
    crate::semantic::infer::narrow::var_ref_id::VarRefRootId,
    i64,
)> {
    if let VarRefId::IndexRef(query_root, query_path) = var_ref_id
        && let Some(index_name) = dynamic_bracket_index_name(query_path.deref())
        && let Some(index) =
            unambiguous_integer_const_name_value(db, cache, index_name, var_ref_id.get_position())
    {
        return Some((query_root.clone(), index));
    }

    if let VarRefId::IndexRef(query_decl_root, _) = var_ref_id
        && let Some(decl_id) = query_decl_root.as_decl_id()
        && let Some(query) =
            numeric_table_index_query_from_decl_initializer(db, cache, root, decl_id)
    {
        return Some(query);
    }

    let decl_id = var_ref_id.get_decl_id_ref()?;
    numeric_table_index_query_from_decl_initializer(db, cache, root, decl_id)
}

fn numeric_table_index_query_from_decl_initializer(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    decl_id: LuaDeclId,
) -> Option<(
    crate::semantic::infer::narrow::var_ref_id::VarRefRootId,
    i64,
)> {
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let initializer = decl.get_initializer()?;
    if initializer.get_ret_idx() != 0 {
        return None;
    }
    let expr = initializer
        .get_expr_syntax_id()
        .to_node_from_root(root.syntax())
        .and_then(LuaExpr::cast)?;
    let LuaExpr::IndexExpr(index_expr) = expr else {
        return None;
    };
    let access_index = index_expr_numeric_key_value(db, cache, &index_expr)?;
    let prefix_expr = index_expr.get_prefix_expr()?;
    let query_root = index_expr_root_id(db, cache, prefix_expr)?;
    Some((query_root, access_index))
}

fn index_expr_root_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    mut prefix_expr: LuaExpr,
) -> Option<crate::semantic::infer::narrow::var_ref_id::VarRefRootId> {
    while let LuaExpr::IndexExpr(index_expr) = prefix_expr {
        prefix_expr = index_expr.get_prefix_expr()?;
    }

    let LuaExpr::NameExpr(name_expr) = prefix_expr else {
        return None;
    };
    match get_var_expr_var_ref_id(db, cache, LuaExpr::NameExpr(name_expr))? {
        VarRefId::SelfRef(self_ref_id) => {
            Some(crate::semantic::infer::narrow::var_ref_id::VarRefRootId::SelfRef(self_ref_id))
        }
        VarRefId::VarRef(decl_id) => {
            Some(crate::semantic::infer::narrow::var_ref_id::VarRefRootId::Decl(decl_id))
        }
        _ => None,
    }
}

fn resolve_same_file_named_function_closure(
    db: &DbIndex,
    cache: &LuaInferCache,
    root: &LuaChunk,
    call_expr: &LuaCallExpr,
) -> Option<LuaClosureExpr> {
    let LuaExpr::NameExpr(name_expr) = call_expr.get_prefix_expr()? else {
        return None;
    };
    if let Some(decl_id) = db
        .get_reference_index()
        .get_var_reference_decl(&cache.get_file_id(), name_expr.get_range())
        && let Some(decl) = db.get_decl_index().get_decl(&decl_id)
        && let Some(token) = decl.get_syntax_id().to_token_from_root(root.syntax())
        && let Some(parent) = token.parent()
    {
        for ancestor in parent.ancestors() {
            if let Some(func_stat) = LuaFuncStat::cast(ancestor.clone()) {
                return func_stat.get_closure();
            }
            if let Some(local_func_stat) = LuaLocalFuncStat::cast(ancestor) {
                return local_func_stat.get_closure();
            }
        }
    }

    let name_text = name_expr.get_name_text()?;
    for block in call_expr.ancestors::<LuaBlock>() {
        let mut matched = block
            .get_stats()
            .take_while(|stat| stat.get_position() < call_expr.get_position())
            .filter_map(|stat| match stat {
                LuaStat::LocalFuncStat(local_func) => local_func
                    .get_local_name()
                    .and_then(|name| name.get_name_token())
                    .is_some_and(|token| token.get_name_text() == name_text)
                    .then(|| local_func.get_closure())
                    .flatten(),
                LuaStat::FuncStat(func_stat) => {
                    simple_func_stat_name_matches(&func_stat, &name_text)
                        .then(|| func_stat.get_closure())
                        .flatten()
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut matched = matched.drain(..);
        let Some(closure) = matched.next() else {
            continue;
        };
        if matched.next().is_some() {
            return None;
        }
        return Some(closure);
    }
    None
}

fn simple_func_stat_name_matches(func_stat: &LuaFuncStat, name: &str) -> bool {
    let Some(LuaVarExpr::NameExpr(name_expr)) = func_stat.get_func_name() else {
        return false;
    };
    name_expr
        .get_name_text()
        .is_some_and(|name_text| name_text == name)
}

fn index_expr_writes_param_at_for_var(
    index_expr: &LuaIndexExpr,
    param_name: &str,
    for_var_name: &str,
) -> bool {
    let Some(prefix) = index_expr.get_prefix_expr() else {
        return false;
    };
    let LuaExpr::NameExpr(prefix_name) = prefix else {
        return false;
    };
    if prefix_name.get_name_text().as_deref() != Some(param_name) {
        return false;
    }

    let Some(LuaIndexKey::Expr(LuaExpr::NameExpr(key_name))) = index_expr.get_index_key() else {
        return false;
    };
    key_name.get_name_text().as_deref() == Some(for_var_name)
}

fn numeric_for_bounds_cover_index(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    for_stat: &glua_parser::LuaForStat,
    access_index: i64,
) -> bool {
    let iter_exprs = for_stat.get_iter_expr().collect::<Vec<_>>();
    let [start_expr, end_expr] = iter_exprs.as_slice() else {
        return false;
    };
    let Some(start) = integer_const_expr_value(db, cache, start_expr) else {
        return false;
    };
    let Some(end) = integer_const_expr_value(db, cache, end_expr) else {
        return false;
    };
    access_index >= start && access_index <= end
}

fn integer_const_expr_value(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: &LuaExpr,
) -> Option<i64> {
    match infer_expr(db, cache, expr.clone()).ok()? {
        LuaType::IntegerConst(value) | LuaType::DocIntegerConst(value) => Some(value),
        _ => None,
    }
}

fn numeric_table_index_query_key_name(var_ref_id: &VarRefId) -> Option<&str> {
    let VarRefId::IndexRef(_, query_path) = var_ref_id else {
        return None;
    };
    dynamic_bracket_index_name(query_path.deref())
}

fn numeric_table_index_query_key_name_from_initializer(
    db: &DbIndex,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
) -> Option<String> {
    let decl_id = match var_ref_id {
        VarRefId::IndexRef(query_root, _) => query_root.as_decl_id(),
        _ => var_ref_id.get_decl_id_ref(),
    }?;
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let initializer = decl.get_initializer()?;
    if initializer.get_ret_idx() != 0 {
        return None;
    }
    let expr = initializer
        .get_expr_syntax_id()
        .to_node_from_root(root.syntax())
        .and_then(LuaExpr::cast)?;
    let LuaExpr::IndexExpr(index_expr) = expr else {
        return None;
    };
    let LuaIndexKey::Expr(LuaExpr::NameExpr(name_expr)) = index_expr.get_index_key()? else {
        return None;
    };
    name_expr.get_name_text()
}

fn dynamic_bracket_index_name(path: &str) -> Option<&str> {
    path.rsplit('.')
        .next()?
        .strip_prefix('[')?
        .strip_suffix(']')
}

fn unambiguous_integer_const_name_value(
    db: &DbIndex,
    cache: &LuaInferCache,
    name: &str,
    before: TextSize,
) -> Option<i64> {
    let file_id = cache.get_file_id();
    let decl_tree = db.get_decl_index().get_decl_tree(&file_id)?;
    let mut value = None;
    for decl in decl_tree
        .get_decls()
        .values()
        .filter(|decl| decl.get_name() == name && decl.get_position() <= before)
    {
        let type_cache = db.get_type_index().get_type_cache(&decl.get_id().into())?;
        let decl_value = match type_cache.as_type() {
            LuaType::IntegerConst(value) | LuaType::DocIntegerConst(value) => *value,
            _ => return None,
        };
        match value {
            Some(existing) if existing != decl_value => return None,
            Some(_) => {}
            None => value = Some(decl_value),
        }
    }
    value
}

fn var_ref_matches_root(
    var_ref_id: &VarRefId,
    root: &crate::semantic::infer::narrow::var_ref_id::VarRefRootId,
) -> bool {
    match var_ref_id {
        VarRefId::VarRef(decl_id) => root.as_decl_id() == Some(*decl_id),
        VarRefId::SelfRef(self_ref_id) => root.receiver_eq(&self_ref_id.receiver),
        VarRefId::IndexRef(index_root, _) => index_root == root,
        _ => false,
    }
}

fn var_ref_is_name(db: &DbIndex, var_ref_id: &VarRefId, name: &str) -> bool {
    let Some(decl_id) = var_ref_id.get_decl_id_ref() else {
        return false;
    };
    db.get_decl_index()
        .get_decl(&decl_id)
        .is_some_and(|decl| decl.get_name() == name)
}

fn assignment_vars_write_root(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    vars: &[LuaVarExpr],
    root: &crate::semantic::infer::narrow::var_ref_id::VarRefRootId,
) -> bool {
    vars.iter().any(|var| {
        get_var_expr_var_ref_id(db, cache, var.to_expr())
            .is_some_and(|var_ref_id| var_ref_matches_root(&var_ref_id, root))
    })
}

fn assignment_vars_write_dynamic_key_name(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    vars: &[LuaVarExpr],
    key_name: &str,
) -> bool {
    vars.iter().any(|var| {
        if let LuaVarExpr::NameExpr(name_expr) = var
            && name_expr.get_name_text().as_deref() == Some(key_name)
        {
            return true;
        }

        get_var_expr_var_ref_id(db, cache, var.to_expr())
            .is_some_and(|var_ref_id| var_ref_is_name(db, &var_ref_id, key_name))
    })
}

fn call_is_inside_nested_closure_expr(call_expr: &LuaCallExpr, expr_range: TextRange) -> bool {
    call_expr.ancestors::<LuaClosureExpr>().any(|closure| {
        let closure_range = closure.get_range();
        closure_range.start() >= expr_range.start() && closure_range.end() <= expr_range.end()
    })
}

fn index_expr_numeric_key_value(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: &LuaIndexExpr,
) -> Option<i64> {
    match index_expr.get_index_key()? {
        LuaIndexKey::Expr(key_expr) => integer_const_expr_value(db, cache, &key_expr),
        LuaIndexKey::Integer(number) => match number.get_number_value() {
            NumberResult::Int(value) => Some(value),
            _ => None,
        },
        LuaIndexKey::Idx(value) => Some(value as i64),
        _ => None,
    }
}

fn assigned_prefix_member_type(
    db: &DbIndex,
    cache: &LuaInferCache,
    query_ref_id: &VarRefId,
    assigned_ref_id: &VarRefId,
    assigned_type: &LuaType,
) -> Result<Option<LuaType>, InferFailReason> {
    let VarRefId::IndexRef(_, path) = query_ref_id else {
        return Ok(None);
    };

    if path.contains('.') {
        return Ok(None);
    }

    let key = LuaMemberKey::Name(path.to_string().into());
    let Some(type_id) = assigned_member_type_id(assigned_type) else {
        return Ok(None);
    };

    resolve_assigned_type_member(db, cache, &type_id, &key, assigned_ref_id.get_position())
        .map(Some)
}

fn assigned_member_type_id(assigned_type: &LuaType) -> Option<LuaTypeDeclId> {
    match assigned_type {
        LuaType::Instance(instance) => assigned_member_type_id(instance.get_base()),
        LuaType::Union(union) => {
            let mut resolved = None;
            for typ in union.types().filter(|typ| !typ.is_nil()) {
                let type_id = assigned_member_type_id(typ)?;
                if resolved
                    .as_ref()
                    .is_some_and(|existing| existing != &type_id)
                {
                    return None;
                }
                resolved = Some(type_id);
            }
            resolved
        }
        LuaType::Def(type_id) | LuaType::Ref(type_id) => Some(type_id.clone()),
        _ => None,
    }
}

fn is_class_instance_type(db: &DbIndex, typ: &LuaType) -> bool {
    let Some(type_id) = assigned_member_type_id(typ) else {
        return false;
    };

    db.get_type_index()
        .get_type_decl(&type_id)
        .is_some_and(|decl| decl.is_class())
}

fn resolve_assigned_type_member(
    db: &DbIndex,
    cache: &LuaInferCache,
    type_id: &LuaTypeDeclId,
    key: &LuaMemberKey,
    access_position: TextSize,
) -> InferResult {
    let type_index = db.get_type_index();
    let type_decl = type_index
        .get_type_decl(type_id)
        .ok_or(InferFailReason::None)?;

    let owner = LuaMemberOwner::Type(type_id.clone());
    if let Some(member_item) = db.get_member_index().get_member_item(&owner, key) {
        return member_item.resolve_type_with_realm_at_offset(
            db,
            &cache.get_file_id(),
            access_position,
        );
    }

    let global_owner = LuaMemberOwner::GlobalPath(GlobalId::new(type_id.get_name()));
    if let Some(member_item) = db.get_member_index().get_member_item(&global_owner, key) {
        return member_item.resolve_type_with_realm_at_offset(
            db,
            &cache.get_file_id(),
            access_position,
        );
    }

    if type_decl.is_class()
        && let Some(super_types) = type_index.get_super_types(type_id)
    {
        for super_type in super_types {
            let Some(super_type_id) = assigned_member_type_id(&super_type) else {
                continue;
            };
            match resolve_assigned_type_member(db, cache, &super_type_id, key, access_position) {
                Ok(member_type) => return Ok(member_type),
                Err(InferFailReason::FieldNotFound) | Err(InferFailReason::None) => {}
                Err(err) => return Err(err),
            }
        }
    }

    Err(InferFailReason::FieldNotFound)
}

fn antecedent_has_relevant_special_call_effect_before_node(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &LuaInferCache,
    root: &LuaChunk,
    flow_node: &FlowNode,
    var_ref_id: &VarRefId,
) -> bool {
    let mut visited = HashSet::new();
    match flow_node.antecedent {
        Some(FlowAntecedent::Single(prev)) => antecedent_has_relevant_special_call_effect(
            db,
            tree,
            cache,
            root,
            prev,
            flow_node.id,
            var_ref_id,
            &mut visited,
        ),
        Some(FlowAntecedent::Multiple(idx)) => {
            tree.get_multi_antecedents(idx).is_some_and(|prevs| {
                prevs.iter().copied().any(|prev| {
                    antecedent_has_relevant_special_call_effect(
                        db,
                        tree,
                        cache,
                        root,
                        prev,
                        flow_node.id,
                        var_ref_id,
                        &mut visited,
                    )
                })
            })
        }
        None => false,
    }
}

fn typed_global_nil_placeholder_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    var_ref_id: &VarRefId,
) -> Option<LuaType> {
    let declared_type = match var_ref_id {
        VarRefId::GlobalName(name, position) => {
            infer_global_type(db, Some(cache.get_file_id()), Some(*position), name).ok()
        }
        VarRefId::VarRef(decl_id) => db.get_decl_index().get_decl(decl_id).and_then(|decl| {
            decl.is_global().then(|| {
                infer_global_type(db, Some(cache.get_file_id()), None, decl.get_name()).ok()
            })?
        }),
        _ => None,
    };
    declared_type.filter(|typ| {
        !typ.is_nil()
            && !typ.is_nullable()
            && !typ.is_unknown()
            && !matches!(typ, LuaType::Any | LuaType::Never)
    })
}

/// Returns true when `expr` is a table-constructor literal `{ ... }`, unwrapping
/// redundant parentheses (e.g. `({})`). Used to detect value-replacement
/// assignments where the RHS introduces a fresh table identity.
fn expr_is_table_constructor(expr: &LuaExpr) -> bool {
    match expr {
        LuaExpr::TableExpr(_) => true,
        LuaExpr::ParenExpr(paren_expr) => paren_expr
            .get_expr()
            .is_some_and(|inner| expr_is_table_constructor(&inner)),
        _ => false,
    }
}

fn guarded_global_self_assignment_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    type_owner: Option<&LuaTypeOwner>,
    var_ref_id: &VarRefId,
    expr: &LuaExpr,
) -> Option<LuaType> {
    if !is_self_coalescing_or_expr(db, cache, var_ref_id, expr) {
        return None;
    }

    let Some(LuaTypeOwner::Decl(decl_id)) = type_owner else {
        return None;
    };

    let decl = db.get_decl_index().get_decl(decl_id)?;
    if !decl.is_global() {
        return None;
    }

    let type_cache = db
        .get_type_index()
        .get_type_cache(&LuaTypeOwner::Decl(*decl_id))?;
    if !type_cache.is_infer() || !type_cache.as_type().is_table() {
        return None;
    }

    Some(type_cache.as_type().clone())
}

fn assignment_flow_info_cannot_match(
    tree: &FlowTree,
    flow_id: FlowId,
    var_ref_id: &VarRefId,
) -> bool {
    let VarRefId::IndexRef(_, query_path) = var_ref_id else {
        return false;
    };

    let Some(info) = tree.get_assignment_flow_info(flow_id) else {
        return false;
    };
    if info.is_empty() {
        return false;
    }
    if info.has_unknown_index_target {
        return false;
    }

    !info
        .index_paths
        .iter()
        .any(|path| path.deref().as_str() == query_path.deref().as_str())
}

fn maybe_get_collection_append_assignment_type(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    var: &LuaVarExpr,
    exprs: &[LuaExpr],
    idx: usize,
    policy: FlowWalkPolicy,
) -> Result<Option<LuaType>, InferFailReason> {
    let LuaVarExpr::IndexExpr(index_expr) = var else {
        return Ok(None);
    };
    if !is_collection_append_write(index_expr) {
        return Ok(None);
    }

    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return Ok(None);
    };
    let LuaExpr::IndexExpr(prefix_index_expr) = prefix_expr else {
        return Ok(None);
    };
    let Some(prefix_var_ref_id) =
        get_var_expr_var_ref_id(db, cache, LuaExpr::IndexExpr(prefix_index_expr.clone()))
    else {
        return Ok(None);
    };
    if prefix_var_ref_id != *var_ref_id {
        return Ok(None);
    }
    if !is_inferred_member_collection_expr(db, cache, &prefix_index_expr)? {
        return Ok(None);
    }

    let source_type = match get_antecedent_type_for_flow_node(
        db, tree, cache, root, var_ref_id, flow_node, policy,
    ) {
        Ok(ty) => ty,
        Err(InferFailReason::UnResolveDeclType(decl_id))
            if should_treat_unresolved_decl_as_nil(db, decl_id) =>
        {
            LuaType::Nil
        }
        Err(err) => return Err(err),
    };
    let Some(source_base) = infer_collection_base_type(db, &source_type) else {
        return Ok(None);
    };

    let value_type = infer_expr_list_value_type_at(db, cache, exprs, idx)?;
    let Some(value_type) = value_type else {
        return Ok(None);
    };

    let widened_base = TypeOps::Union.apply(db, &source_base, &value_type);
    Ok(Some(LuaType::Array(
        LuaArrayType::from_base_type(widened_base).into(),
    )))
}

fn is_collection_append_write(index_expr: &LuaIndexExpr) -> bool {
    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return false;
    };
    let Some(LuaIndexKey::Expr(index_key_expr)) = index_expr.get_index_key() else {
        return false;
    };
    let LuaExpr::BinaryExpr(binary_expr) = index_key_expr else {
        return false;
    };
    if binary_expr
        .get_op_token()
        .is_none_or(|token| token.get_op() != BinaryOperator::OpAdd)
    {
        return false;
    }

    let Some((left, right)) = binary_expr.get_exprs() else {
        return false;
    };
    if !is_literal_integer_one(&right) {
        return false;
    }

    let LuaExpr::UnaryExpr(unary_expr) = left else {
        return false;
    };
    if unary_expr
        .get_op_token()
        .is_none_or(|token| token.get_op() != UnaryOperator::OpLen)
    {
        return false;
    }

    let Some(len_expr) = unary_expr.get_expr() else {
        return false;
    };
    expr_access_path(&prefix_expr) == expr_access_path(&len_expr)
}

fn is_inferred_member_collection_expr(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: &LuaIndexExpr,
) -> Result<bool, InferFailReason> {
    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return Ok(false);
    };
    let prefix_type = infer_expr(db, cache, prefix_expr)?;
    let Some(owner) = get_member_owner_for_prefix_type(prefix_type) else {
        return Ok(false);
    };
    let Some(index_key) = index_expr.get_index_key() else {
        return Ok(false);
    };
    let member_key = LuaMemberKey::from_index_key(db, cache, &index_key)?;
    let members = db
        .get_member_index()
        .get_members_for_owner_key(&owner, &member_key);
    if members.is_empty() {
        return Ok(false);
    }

    let mut saw_collection = false;
    for member in members {
        let Some(type_cache) = db.get_type_index().get_type_cache(&member.get_id().into()) else {
            continue;
        };
        if !type_cache.is_infer() {
            return Ok(false);
        }
        if normalize_infer_collection_type(db, type_cache.as_type()).is_none() {
            return Ok(false);
        }
        saw_collection = true;
    }

    Ok(saw_collection)
}

fn get_member_owner_for_prefix_type(prefix_type: LuaType) -> Option<LuaMemberOwner> {
    match prefix_type {
        LuaType::TableConst(in_file_range) => Some(LuaMemberOwner::Element(in_file_range)),
        LuaType::Def(def_id) | LuaType::Ref(def_id) => Some(LuaMemberOwner::Type(def_id)),
        LuaType::Instance(instance) => Some(LuaMemberOwner::Element(instance.get_range().clone())),
        _ => None,
    }
}

fn normalize_infer_collection_type(db: &DbIndex, typ: &LuaType) -> Option<()> {
    infer_collection_base_type(db, typ).map(|_| ())
}

fn infer_collection_base_type(db: &DbIndex, typ: &LuaType) -> Option<LuaType> {
    match typ {
        LuaType::Array(array) => Some(array.get_base().clone()),
        LuaType::Tuple(tuple) if tuple.is_infer_resolve() => Some(tuple.cast_down_array_base(db)),
        LuaType::TableConst(range) => crate::table_const_array_base(db, range),
        LuaType::TypeGuard(inner) => infer_collection_base_type(db, inner),
        LuaType::Union(union) => infer_collection_base_types(db, union.types()),
        LuaType::Intersection(intersection) => {
            infer_collection_base_types(db, intersection.get_types().iter())
        }
        LuaType::MergedTable(merged_table) => {
            infer_collection_base_types(db, merged_table.get_types().iter())
        }
        LuaType::MultiLineUnion(union) => {
            infer_collection_base_types(db, union.get_unions().iter().map(|(typ, _)| typ))
        }
        _ => None,
    }
}

fn infer_collection_base_types<'a>(
    db: &DbIndex,
    types: impl Iterator<Item = &'a LuaType>,
) -> Option<LuaType> {
    let mut base_type = None;
    for typ in types {
        if typ.is_never() {
            continue;
        }

        let collection_base = infer_collection_base_type(db, typ)?;
        base_type = Some(match base_type {
            Some(current) => TypeOps::Union.apply(db, &current, &collection_base),
            None => collection_base,
        });
    }

    base_type
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

fn try_infer_decl_initializer_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
) -> Result<Option<LuaType>, InferFailReason> {
    let Some(decl_id) = var_ref_id.get_decl_id_ref() else {
        return Ok(None);
    };

    let decl = db
        .get_decl_index()
        .get_decl(&decl_id)
        .ok_or(InferFailReason::None)?;

    let Some(initializer) = decl.get_initializer() else {
        return Ok(None);
    };

    let Some(node) = initializer
        .get_expr_syntax_id()
        .to_node_from_root(root.syntax())
    else {
        return Ok(None);
    };

    let Some(expr) = LuaExpr::cast(node) else {
        return Ok(None);
    };

    let ret_idx = initializer.get_ret_idx();
    let initializer_is_call = matches!(&expr, LuaExpr::CallExpr(_));
    let init_type = match infer_expr(db, cache, expr)? {
        LuaType::Variadic(variadic) => variadic.get_type(ret_idx).cloned().unwrap_or(LuaType::Nil),
        ty if ret_idx == 0 => ty,
        LuaType::Unknown if initializer_is_call => LuaType::Unknown,
        _ => LuaType::Nil,
    };

    Ok(Some(init_type))
}

fn is_self_coalescing_or_expr(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    var_ref_id: &VarRefId,
    expr: &LuaExpr,
) -> bool {
    if let LuaExpr::BinaryExpr(bin_expr) = expr {
        if let Some(op_token) = bin_expr.get_op_token() {
            if op_token.get_op() == BinaryOperator::OpOr {
                if let Some(left) = bin_expr.get_left_expr() {
                    if let Some(left_ref_id) = get_var_expr_var_ref_id(db, cache, left) {
                        return left_ref_id == *var_ref_id;
                    }
                }
            }
        }
    }
    false
}

fn prefer_table_of_over_bare_table(_db: &DbIndex, ty: LuaType) -> LuaType {
    match ty {
        LuaType::Union(u) => {
            let mut types = u.into_vec();
            let has_table_of = types.iter().any(|t| matches!(t, LuaType::TableOf(_)));
            let has_bare_table = types.iter().any(|t| matches!(t, LuaType::Table));
            if has_table_of && has_bare_table {
                types.retain(|t| !matches!(t, LuaType::Table));
                if types.len() == 1 {
                    types.into_iter().next().unwrap_or(LuaType::Unknown)
                } else {
                    LuaUnionType::from_vec(types).into()
                }
            } else {
                LuaType::Union(u)
            }
        }
        _ => ty,
    }
}

/// Check whether an explicit `---@param x string = "literal"` default is
/// still live at a given use site by walking the flow graph backward.
///
/// Returns `true` only when the declaration origin for `decl_id` is reachable
/// from `use_flow_id` without passing through a killing assignment.  A
/// self-coalescing `x = x or ...` assignment is NOT a kill — it is a fallback
/// that the explicit default takes precedence over.  Any other assignment to
/// the same variable kills the explicit default.
pub fn explicit_param_string_default_reaches_flow(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    decl_id: LuaDeclId,
    use_flow_id: FlowId,
) -> bool {
    let var_ref_id = VarRefId::VarRef(decl_id);
    let mut visited = HashSet::new();
    explicit_default_reaches_inner(
        db,
        tree,
        cache,
        root,
        &var_ref_id,
        use_flow_id,
        &mut visited,
    )
}

fn explicit_default_reaches_inner(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_id: FlowId,
    visited: &mut HashSet<FlowId>,
) -> bool {
    // Guard against infinite loops in cyclic flow graphs.
    if !visited.insert(flow_id) {
        return false;
    }

    let Some(flow_node) = tree.get_flow_node(flow_id) else {
        return false;
    };

    match &flow_node.kind {
        // Reached the declaration origin for our target variable —
        // explicit default is proven valid.
        FlowNodeKind::DeclPosition(position) => {
            if matches!(var_ref_id.get_decl_id_ref(), Some(decl_id) if decl_id.position == *position)
            {
                true
            } else {
                // DeclPosition for another variable — walk past it.
                walk_antecedents_for_explicit_default(
                    db, tree, cache, root, var_ref_id, flow_node, visited,
                )
            }
        }
        // For parameters, the flow tree may not have a DeclPosition node;
        // reaching Start without encountering a killing assignment proves
        // the explicit default is still live.
        FlowNodeKind::Start => true,
        // Dead paths.
        FlowNodeKind::Unreachable | FlowNodeKind::Return | FlowNodeKind::Break => false,
        FlowNodeKind::Assignment(assign_ptr, assign_hint) => {
            let can_match = matches!(
                (assign_hint, var_ref_id),
                (AssignVarHint::Mixed, _)
                    | (AssignVarHint::NameOnly, VarRefId::VarRef(_))
                    | (AssignVarHint::NameOnly, VarRefId::GlobalName(_, _))
                    | (AssignVarHint::NameOnly, VarRefId::SelfRef(_))
                    | (AssignVarHint::IndexOnly, VarRefId::IndexRef(_, _))
            );

            if can_match {
                if let Some(assign_stat) = assign_ptr.to_node(root) {
                    let (vars, _) = assign_stat.get_var_and_expr_list();
                    for var in vars.iter() {
                        if let Some(ref_id) = get_var_expr_var_ref_id(db, cache, var.to_expr()) {
                            if ref_id == *var_ref_id {
                                // Any assignment to the variable kills the
                                // explicit default — including self-coalescing
                                // assignments like `x = x or "literal"`.
                                // After such an assignment, the inferred-default
                                // path takes over for downstream use sites.
                                return false;
                            }
                        }
                    }
                }
            }

            walk_antecedents_for_explicit_default(
                db, tree, cache, root, var_ref_id, flow_node, visited,
            )
        }
        _ => walk_antecedents_for_explicit_default(
            db, tree, cache, root, var_ref_id, flow_node, visited,
        ),
    }
}

/// Walk backward through antecedents of a flow node, requiring the explicit
/// default to reach on ALL live paths (conjunction).
///
/// Mirrors the realm-filtering approach from `merge_antecedent_types`:
/// wrong-realm antecedents are skipped on the first pass.  If filtering
/// removes ALL live antecedents, a second pass without realm checks
/// preserves conservative behaviour.
fn walk_antecedents_for_explicit_default(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    visited: &mut HashSet<FlowId>,
) -> bool {
    match &flow_node.antecedent {
        Some(FlowAntecedent::Single(antecedent_id)) => explicit_default_reaches_inner(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            *antecedent_id,
            visited,
        ),
        Some(FlowAntecedent::Multiple(idx)) => {
            if let Some(antecedents) = tree.get_multi_antecedents(*idx) {
                let target_realm = cache.flow_query_realm.unwrap_or_else(|| {
                    db.get_gmod_infer_index()
                        .get_realm_at_offset(&cache.get_file_id(), var_ref_id.get_position())
                });

                // First pass: realm-filtered.
                let base_visited = visited.clone();
                let mut any_live = false;
                for &antecedent_id in antecedents {
                    let Some(ante_node) = tree.get_flow_node(antecedent_id) else {
                        continue;
                    };
                    if ante_node.kind.is_unreachable() || ante_node.kind.is_change_flow() {
                        continue;
                    }

                    let ante_realm =
                        get_or_compute_flow_node_realm(db, cache, root, antecedent_id, ante_node);
                    if !realms_can_reach(target_realm, ante_realm) {
                        continue;
                    }

                    any_live = true;
                    let mut path_visited = base_visited.clone();
                    if !explicit_default_reaches_inner(
                        db,
                        tree,
                        cache,
                        root,
                        var_ref_id,
                        antecedent_id,
                        &mut path_visited,
                    ) {
                        return false;
                    }
                    visited.extend(path_visited);
                }

                if any_live {
                    return true;
                }

                // Fallback: no live antecedents after realm filtering —
                // retry without realm checks for conservative behaviour.
                let mut any_live = false;
                for &antecedent_id in antecedents {
                    let Some(ante_node) = tree.get_flow_node(antecedent_id) else {
                        continue;
                    };
                    if ante_node.kind.is_unreachable() || ante_node.kind.is_change_flow() {
                        continue;
                    }
                    any_live = true;
                    let mut path_visited = base_visited.clone();
                    if !explicit_default_reaches_inner(
                        db,
                        tree,
                        cache,
                        root,
                        var_ref_id,
                        antecedent_id,
                        &mut path_visited,
                    ) {
                        return false;
                    }
                    visited.extend(path_visited);
                }
                any_live
            } else {
                false
            }
        }
        None => false,
    }
}

/// Check whether an inferred string default (from `x = x or "literal"`) is
/// still live at a given use site by walking the flow graph backward.
///
/// Returns `true` only when the self-coalescing assignment at
/// `default_source_range` is the **last** assignment to `decl_id` that
/// dominates the use — any later write to the same variable kills it.
pub fn inferred_string_default_reaches_flow(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    decl_id: LuaDeclId,
    use_flow_id: FlowId,
    default_source_range: rowan::TextRange,
) -> bool {
    let var_ref_id = VarRefId::VarRef(decl_id);
    let mut visited = HashSet::new();
    inferred_string_default_reaches_inner(
        db,
        tree,
        cache,
        root,
        &var_ref_id,
        use_flow_id,
        default_source_range,
        &mut visited,
    )
}

fn inferred_string_default_reaches_inner(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_id: FlowId,
    default_source_range: rowan::TextRange,
    visited: &mut HashSet<FlowId>,
) -> bool {
    // Guard against infinite loops in cyclic flow graphs.
    if !visited.insert(flow_id) {
        return false;
    }

    let Some(flow_node) = tree.get_flow_node(flow_id) else {
        return false;
    };

    match &flow_node.kind {
        FlowNodeKind::Start => false,
        FlowNodeKind::Unreachable | FlowNodeKind::Return | FlowNodeKind::Break => false,
        FlowNodeKind::DeclPosition(_) => walk_antecedents_for_default(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            default_source_range,
            visited,
        ),
        FlowNodeKind::Assignment(assign_ptr, assign_hint) => {
            let can_match = matches!(
                (assign_hint, var_ref_id),
                (AssignVarHint::Mixed, _)
                    | (AssignVarHint::NameOnly, VarRefId::VarRef(_))
                    | (AssignVarHint::NameOnly, VarRefId::GlobalName(_, _))
                    | (AssignVarHint::NameOnly, VarRefId::SelfRef(_))
                    | (AssignVarHint::IndexOnly, VarRefId::IndexRef(_, _))
            );

            if can_match {
                if let Some(assign_stat) = assign_ptr.to_node(root) {
                    let (vars, _exprs) = assign_stat.get_var_and_expr_list();
                    for var in vars {
                        if let Some(ref_id) = get_var_expr_var_ref_id(db, cache, var.to_expr()) {
                            if ref_id == *var_ref_id {
                                let assign_range = assign_stat.get_range();
                                // Same range → matching assignment (default proven).
                                // Different range → later write kills the default.
                                return assign_range == default_source_range;
                            }
                        }
                    }
                }
            }

            walk_antecedents_for_default(
                db,
                tree,
                cache,
                root,
                var_ref_id,
                flow_node,
                default_source_range,
                visited,
            )
        }
        _ => walk_antecedents_for_default(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            default_source_range,
            visited,
        ),
    }
}

/// Walk backward through antecedents of a flow node, requiring the default
/// to reach on ALL live paths (conjunction).
///
/// Mirrors the realm-filtering approach from `merge_antecedent_types`:
/// wrong-realm antecedents are skipped on the first pass.  If filtering
/// removes ALL live antecedents, a second pass without realm checks
/// preserves conservative behaviour.
fn walk_antecedents_for_default(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    default_source_range: rowan::TextRange,
    visited: &mut HashSet<FlowId>,
) -> bool {
    match &flow_node.antecedent {
        Some(FlowAntecedent::Single(antecedent_id)) => inferred_string_default_reaches_inner(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            *antecedent_id,
            default_source_range,
            visited,
        ),
        Some(FlowAntecedent::Multiple(idx)) => {
            if let Some(antecedents) = tree.get_multi_antecedents(*idx) {
                let target_realm = cache.flow_query_realm.unwrap_or_else(|| {
                    db.get_gmod_infer_index()
                        .get_realm_at_offset(&cache.get_file_id(), var_ref_id.get_position())
                });

                // First pass: realm-filtered.
                let base_visited = visited.clone();
                let mut any_live = false;
                for &antecedent_id in antecedents {
                    let Some(ante_node) = tree.get_flow_node(antecedent_id) else {
                        continue;
                    };
                    if ante_node.kind.is_unreachable() || ante_node.kind.is_change_flow() {
                        continue;
                    }

                    let ante_realm =
                        get_or_compute_flow_node_realm(db, cache, root, antecedent_id, ante_node);
                    if !realms_can_reach(target_realm, ante_realm) {
                        continue;
                    }

                    any_live = true;
                    let mut path_visited = base_visited.clone();
                    if !inferred_string_default_reaches_inner(
                        db,
                        tree,
                        cache,
                        root,
                        var_ref_id,
                        antecedent_id,
                        default_source_range,
                        &mut path_visited,
                    ) {
                        return false;
                    }
                    visited.extend(path_visited);
                }

                if any_live {
                    return true;
                }

                // Fallback: no live antecedents after realm filtering —
                // retry without realm checks for conservative behaviour.
                let mut any_live = false;
                for &antecedent_id in antecedents {
                    let Some(ante_node) = tree.get_flow_node(antecedent_id) else {
                        continue;
                    };
                    if ante_node.kind.is_unreachable() || ante_node.kind.is_change_flow() {
                        continue;
                    }
                    any_live = true;
                    let mut path_visited = base_visited.clone();
                    if !inferred_string_default_reaches_inner(
                        db,
                        tree,
                        cache,
                        root,
                        var_ref_id,
                        antecedent_id,
                        default_source_range,
                        &mut path_visited,
                    ) {
                        return false;
                    }
                    visited.extend(path_visited);
                }
                any_live
            } else {
                false
            }
        }
        None => {
            // No antecedents — reached the implicit start without ever
            // encountering the recorded assignment.  The default was NOT
            // proven on this path.
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use glua_parser::{LuaNameExpr, LuaParser, ParserConfig};
    use internment::ArcIntern;
    use rowan::TextSize;
    use smol_str::SmolStr;

    use super::*;
    use crate::{AssignmentFlowInfo, FileNarrowingCapability};

    #[test]
    fn normal_recursive_antecedent_lookup_preserves_counterfactual_origin() {
        let file_id = FileId::new(1);
        let parser = LuaParser::parse("", ParserConfig::default());
        let root = parser.get_chunk_node();
        let db = DbIndex::new();
        let flow_tree = FlowTree::new(
            HashMap::new(),
            vec![
                FlowNode {
                    id: FlowId(0),
                    kind: FlowNodeKind::Start,
                    antecedent: None,
                },
                FlowNode {
                    id: FlowId(1),
                    kind: FlowNodeKind::Start,
                    antecedent: None,
                },
                FlowNode {
                    id: FlowId(2),
                    kind: FlowNodeKind::BranchLabel,
                    antecedent: Some(FlowAntecedent::Multiple(0)),
                },
            ],
            vec![vec![FlowId(0), FlowId(1)]],
            HashMap::new(),
            HashMap::new(),
            vec![AssignmentFlowInfo::default(); 3],
            FileNarrowingCapability::default(),
        );
        let var_ref_id =
            VarRefId::GlobalName(ArcIntern::from(SmolStr::new("value")), TextSize::new(0));
        let query_realm = GmodRealm::Shared;
        let mut cache = LuaInferCache::new(file_id, Default::default());
        cache.flow_query_realm = Some(query_realm);

        for antecedent in [FlowId(0), FlowId(1)] {
            cache.set_flow_cache_with_origin(
                &var_ref_id,
                antecedent,
                query_realm,
                FlowOrigin::Real,
                CacheEntry::Cache(LuaType::String),
            );
            cache.set_flow_cache_with_origin(
                &var_ref_id,
                antecedent,
                query_realm,
                FlowOrigin::NilCounterfactual,
                CacheEntry::Cache(LuaType::Number),
            );
        }

        let counterfactual_result = get_type_at_flow_with_origin(
            &db,
            &flow_tree,
            &mut cache,
            &root,
            &var_ref_id,
            FlowId(2),
            FlowOrigin::NilCounterfactual,
        )
        .expect("counterfactual branch flow should resolve");
        let real_result = get_type_at_flow_with_origin(
            &db,
            &flow_tree,
            &mut cache,
            &root,
            &var_ref_id,
            FlowId(2),
            FlowOrigin::Real,
        )
        .expect("real branch flow should resolve");

        assert_eq!(
            (counterfactual_result, real_result),
            (LuaType::Number, LuaType::String)
        );
        assert!(matches!(
            cache.get_flow_cache_with_origin(
                &var_ref_id,
                FlowId(2),
                query_realm,
                FlowOrigin::NilCounterfactual,
            ),
            Some(CacheEntry::Cache(LuaType::Number))
        ));
        assert!(matches!(
            cache
                .get_flow_cache_with_origin(&var_ref_id, FlowId(2), query_realm, FlowOrigin::Real,),
            Some(CacheEntry::Cache(LuaType::String))
        ));
    }

    #[test]
    fn condition_recursive_antecedent_lookup_preserves_counterfactual_origin() {
        let file_id = FileId::new(2);
        let parser = LuaParser::parse("value", ParserConfig::default());
        let root = parser.get_chunk_node();
        let condition = root
            .clone()
            .descendants::<LuaNameExpr>()
            .next()
            .map(LuaExpr::NameExpr)
            .expect("condition name expression");
        let db = DbIndex::new();
        let flow_tree = FlowTree::new(
            HashMap::new(),
            vec![
                FlowNode {
                    id: FlowId(0),
                    kind: FlowNodeKind::Start,
                    antecedent: None,
                },
                FlowNode {
                    id: FlowId(1),
                    kind: FlowNodeKind::TrueCondition(condition.to_ptr()),
                    antecedent: Some(FlowAntecedent::Single(FlowId(0))),
                },
            ],
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            vec![AssignmentFlowInfo::default(); 2],
            FileNarrowingCapability::default(),
        );
        let var_ref_id =
            VarRefId::GlobalName(ArcIntern::from(SmolStr::new("value")), TextSize::new(0));
        let query_realm = GmodRealm::Shared;
        let mut cache = LuaInferCache::new(file_id, Default::default());
        cache.flow_query_realm = Some(query_realm);
        cache.set_flow_cache_with_origin(
            &var_ref_id,
            FlowId(0),
            query_realm,
            FlowOrigin::Real,
            CacheEntry::Cache(LuaType::String),
        );
        cache.set_flow_cache_with_origin(
            &var_ref_id,
            FlowId(0),
            query_realm,
            FlowOrigin::NilCounterfactual,
            CacheEntry::Cache(LuaType::Number),
        );

        let counterfactual_result = get_type_at_flow_with_origin(
            &db,
            &flow_tree,
            &mut cache,
            &root,
            &var_ref_id,
            FlowId(1),
            FlowOrigin::NilCounterfactual,
        )
        .expect("counterfactual condition flow should resolve");
        let real_result = get_type_at_flow_with_origin(
            &db,
            &flow_tree,
            &mut cache,
            &root,
            &var_ref_id,
            FlowId(1),
            FlowOrigin::Real,
        )
        .expect("real condition flow should resolve");

        assert_eq!(
            (counterfactual_result, real_result),
            (LuaType::Number, LuaType::String)
        );
        assert!(matches!(
            cache.get_flow_cache_with_origin(
                &var_ref_id,
                FlowId(1),
                query_realm,
                FlowOrigin::NilCounterfactual,
            ),
            Some(CacheEntry::Cache(LuaType::Number))
        ));
        assert!(matches!(
            cache
                .get_flow_cache_with_origin(&var_ref_id, FlowId(1), query_realm, FlowOrigin::Real,),
            Some(CacheEntry::Cache(LuaType::String))
        ));
    }
}
