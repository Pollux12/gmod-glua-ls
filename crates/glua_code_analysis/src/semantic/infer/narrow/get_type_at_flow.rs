use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAstNode, LuaChunk, LuaExpr, LuaIndexExpr, LuaIndexKey,
    LuaLiteralToken, LuaVarExpr, NumberResult, PathTrait, UnaryOperator,
};
use rowan::TextSize;

use crate::{
    AssignVarHint, CacheEntry, DbIndex, FlowAntecedent, FlowId, FlowNode, FlowNodeKind, FlowTree,
    GmodRealm, InferFailReason, LuaArrayType, LuaDeclId, LuaInferCache, LuaMemberId, LuaMemberKey,
    LuaMemberOwner, LuaSemanticDeclId, LuaSignatureId, LuaType, TypeOps, infer_expr,
    semantic::infer::{
        InferResult, VarRefId, infer_expr_list_value_type_at,
        infer_name::infer_param,
        narrow::{
            ResultTypeOrContinue,
            condition_flow::{InferConditionFlow, get_type_at_condition_flow},
            get_multi_antecedents, get_single_antecedent,
            get_type_at_cast_flow::get_type_at_cast_flow,
            get_var_ref_type, narrow_down_type,
            var_ref_id::get_var_expr_var_ref_id,
        },
    },
};

pub fn get_type_at_flow(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_id: FlowId,
) -> InferResult {
    cache.prof_flow_calls += 1;
    // If the per-file flow budget has been exhausted, skip narrowing entirely
    // and return the declared/base type. This prevents pathologically large
    // files from dominating analysis time.
    if cache.flow_budget_exhausted() {
        return get_var_ref_type(db, cache, var_ref_id);
    }

    let query_realm = cache.flow_query_realm.unwrap_or_else(|| {
        db.get_gmod_infer_index()
            .get_realm_at_offset(&cache.get_file_id(), var_ref_id.get_position())
    });
    let key = (var_ref_id.clone(), flow_id, query_realm);
    // Check cache for both success and error results.
    match cache.flow_node_cache.get(&key) {
        Some(CacheEntry::Cache(narrow_type)) => {
            cache.prof_flow_hits += 1;
            return Ok(narrow_type.clone());
        }
        Some(CacheEntry::Error(reason)) => {
            cache.prof_flow_hits += 1;
            return Err(reason.clone());
        }
        _ => {}
    }

    // Track all flow IDs we walk through so we can cache the result for
    // each of them, preventing redundant walks for the same var in overlapping
    // flow chains.
    let mut visited_flow_ids = vec![flow_id];

    let result = get_type_at_flow_walk(
        db,
        tree,
        cache,
        root,
        var_ref_id,
        query_realm,
        flow_id,
        &mut visited_flow_ids,
    );

    // Cache the result (success OR error) for all intermediate flow IDs we
    // walked through.  This is critical for performance: without error
    // caching, every failed walk for the same variable through an overlapping
    // chain repeats the entire traversal.  With caching, subsequent walks
    // hit the cached error immediately.
    //
    // RecursiveInfer errors are transient (cycle detection) and must NOT be
    // cached — they'd poison future non-recursive queries.
    match &result {
        Ok(ty) => {
            let entry = CacheEntry::Cache(ty.clone());
            for &fid in &visited_flow_ids {
                cache
                    .flow_node_cache
                    .insert((var_ref_id.clone(), fid, query_realm), entry.clone());
            }
        }
        Err(InferFailReason::RecursiveInfer) => {
            // Don't cache — this is a transient cycle-detection signal.
        }
        Err(reason) => {
            let entry = CacheEntry::Error(reason.clone());
            for &fid in &visited_flow_ids {
                cache
                    .flow_node_cache
                    .insert((var_ref_id.clone(), fid, query_realm), entry.clone());
            }
        }
    }

    result
}

/// Inner walk loop for `get_type_at_flow`.  Returns the inferred type or an
/// error.  All flow IDs visited during the linear backward walk are pushed
/// into `visited` so the caller can bulk-cache the result.
fn get_type_at_flow_walk(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    query_realm: GmodRealm,
    initial_flow_id: FlowId,
    visited: &mut Vec<FlowId>,
) -> InferResult {
    let mut antecedent_flow_id = initial_flow_id;
    loop {
        // Check cache for intermediate flow nodes (both success and error).
        // This is critical for performance in large files where many walks
        // share overlapping flow chains.
        let intermediate_key = (var_ref_id.clone(), antecedent_flow_id, query_realm);
        match cache.flow_node_cache.get(&intermediate_key) {
            Some(CacheEntry::Cache(cached_type)) => return Ok(cached_type.clone()),
            Some(CacheEntry::Error(reason)) => return Err(reason.clone()),
            _ => {}
        }

        // Track total flow work for budget enforcement.
        cache.flow_nodes_visited += 1;
        cache.prof_flow_nodes_walked += 1;
        if cache.flow_budget_exhausted() {
            // Budget exceeded mid-walk — return base type for this variable.
            return get_var_ref_type(db, cache, var_ref_id);
        }

        let flow_node = tree
            .get_flow_node(antecedent_flow_id)
            .ok_or(InferFailReason::None)?;

        match &flow_node.kind {
            FlowNodeKind::Start | FlowNodeKind::Unreachable => {
                return get_var_ref_type(db, cache, var_ref_id);
            }
            FlowNodeKind::LoopLabel | FlowNodeKind::Break | FlowNodeKind::Return => {
                if let Some(merged_type) =
                    try_get_multi_antecedent_type(db, tree, cache, root, var_ref_id, flow_node)?
                {
                    return Ok(merged_type);
                }
                antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                visited.push(antecedent_flow_id);
            }
            FlowNodeKind::BranchLabel | FlowNodeKind::NamedLabel(_) => {
                // Try the merge-skip optimisation for if/else BranchLabels.
                // If the variable was not modified in any branch AND all
                // antecedents are alive (no Return / Break / Unreachable),
                // the merged type is identical to the type at the common
                // predecessor — skip the merge and continue the linear walk.
                if matches!(flow_node.kind, FlowNodeKind::BranchLabel) {
                    if let Some(info) = tree.get_branch_label_info(antecedent_flow_id) {
                        let can_skip = match var_ref_id {
                            VarRefId::VarRef(_)
                            | VarRefId::SelfRef(_)
                            | VarRefId::GlobalName(_, _) => {
                                !info.has_name_assigns
                                    && !info.has_casts_or_implfunc
                                    && !info.has_inner_conditions
                            }
                            VarRefId::IndexRef(_, _) => {
                                !info.has_index_assigns
                                    && !info.has_casts_or_implfunc
                                    && !info.has_inner_conditions
                            }
                        };

                        if can_skip && all_branch_antecedents_alive(tree, flow_node) {
                            antecedent_flow_id = info.common_predecessor;
                            visited.push(antecedent_flow_id);
                            continue;
                        }
                    }
                }

                return merge_antecedent_types(db, tree, cache, root, var_ref_id, flow_node);
            }
            FlowNodeKind::DeclPosition(position) => {
                if *position <= var_ref_id.get_position() {
                    if let Some(decl_id) = var_ref_id.get_decl_id_ref()
                        && should_defer_uninitialized_local_decl_type(db, decl_id)
                    {
                        return Err(InferFailReason::UnResolveDeclType(decl_id));
                    }

                    match get_decl_position_var_ref_type(db, cache, var_ref_id) {
                        Ok(var_type) => {
                            return Ok(var_type);
                        }
                        Err(err) => {
                            if let Some(init_type) =
                                try_infer_decl_initializer_type(db, cache, root, var_ref_id)?
                            {
                                return Ok(init_type);
                            }

                            return Err(err);
                        }
                    }
                } else {
                    if let Some(merged_type) =
                        try_get_multi_antecedent_type(db, tree, cache, root, var_ref_id, flow_node)?
                    {
                        return Ok(merged_type);
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    visited.push(antecedent_flow_id);
                }
            }
            FlowNodeKind::Assignment(assign_ptr, assign_hint) => {
                let can_match_assignment = matches!(
                    (assign_hint, var_ref_id),
                    (AssignVarHint::Mixed, _)
                        | (AssignVarHint::NameOnly, VarRefId::VarRef(_))
                        | (AssignVarHint::NameOnly, VarRefId::GlobalName(_, _))
                        | (AssignVarHint::NameOnly, VarRefId::SelfRef(_))
                        | (AssignVarHint::IndexOnly, VarRefId::IndexRef(_, _))
                );

                if !can_match_assignment {
                    if let Some(merged_type) =
                        try_get_multi_antecedent_type(db, tree, cache, root, var_ref_id, flow_node)?
                    {
                        return Ok(merged_type);
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    visited.push(antecedent_flow_id);
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
                )?;

                if let ResultTypeOrContinue::Result(assign_type) = result_or_continue {
                    return Ok(assign_type);
                } else {
                    if let Some(merged_type) =
                        try_get_multi_antecedent_type(db, tree, cache, root, var_ref_id, flow_node)?
                    {
                        return Ok(merged_type);
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    visited.push(antecedent_flow_id);
                }
            }
            FlowNodeKind::ImplFunc(func_ptr) => {
                let func_stat = func_ptr.to_node(root).ok_or(InferFailReason::None)?;
                let Some(func_name) = func_stat.get_func_name() else {
                    if let Some(merged_type) =
                        try_get_multi_antecedent_type(db, tree, cache, root, var_ref_id, flow_node)?
                    {
                        return Ok(merged_type);
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    visited.push(antecedent_flow_id);
                    continue;
                };

                let Some(ref_id) = get_var_expr_var_ref_id(db, cache, func_name.to_expr()) else {
                    if let Some(merged_type) =
                        try_get_multi_antecedent_type(db, tree, cache, root, var_ref_id, flow_node)?
                    {
                        return Ok(merged_type);
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    visited.push(antecedent_flow_id);
                    continue;
                };

                if ref_id == *var_ref_id {
                    // Only use the func-stat's signature when the member isn't
                    // already declared (origin type is Nil). For Def types with
                    // @field annotations, let the flow continue so the declared
                    // type is preserved instead of being overridden by the
                    // implementation signature.
                    let is_undeclared = cache
                        .index_ref_origin_type_cache
                        .get(var_ref_id)
                        .is_some_and(|entry| matches!(entry, CacheEntry::Cache(t) if t.is_nil()));

                    if is_undeclared {
                        let Some(closure) = func_stat.get_closure() else {
                            return Err(InferFailReason::None);
                        };

                        return Ok(LuaType::Signature(LuaSignatureId::from_closure(
                            cache.get_file_id(),
                            &closure,
                        )));
                    }

                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    visited.push(antecedent_flow_id);
                } else {
                    if let Some(merged_type) =
                        try_get_multi_antecedent_type(db, tree, cache, root, var_ref_id, flow_node)?
                    {
                        return Ok(merged_type);
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    visited.push(antecedent_flow_id);
                }
            }
            FlowNodeKind::TrueCondition(condition_ptr) => {
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
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        cache.prof_condition_errors_caught += 1;
                        match &e {
                            InferFailReason::None => cache.prof_condition_errors_none += 1,
                            InferFailReason::RecursiveInfer => {
                                cache.prof_condition_errors_recursive += 1
                            }
                            InferFailReason::UnResolveDeclType(_) => {
                                cache.prof_condition_errors_unresolved += 1
                            }
                            _ => {}
                        }
                        ResultTypeOrContinue::Continue
                    }
                };

                if let ResultTypeOrContinue::Result(condition_type) = result_or_continue {
                    return Ok(condition_type);
                } else {
                    if let Some(merged_type) =
                        try_get_multi_antecedent_type(db, tree, cache, root, var_ref_id, flow_node)?
                    {
                        cache.prof_multi_ante_from_condition += 1;
                        return Ok(merged_type);
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    visited.push(antecedent_flow_id);
                }
            }
            FlowNodeKind::FalseCondition(condition_ptr) => {
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
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        cache.prof_condition_errors_caught += 1;
                        match &e {
                            InferFailReason::None => cache.prof_condition_errors_none += 1,
                            InferFailReason::RecursiveInfer => {
                                cache.prof_condition_errors_recursive += 1
                            }
                            InferFailReason::UnResolveDeclType(_) => {
                                cache.prof_condition_errors_unresolved += 1
                            }
                            _ => {}
                        }
                        ResultTypeOrContinue::Continue
                    }
                };

                if let ResultTypeOrContinue::Result(condition_type) = result_or_continue {
                    return Ok(condition_type);
                } else {
                    if let Some(merged_type) =
                        try_get_multi_antecedent_type(db, tree, cache, root, var_ref_id, flow_node)?
                    {
                        return Ok(merged_type);
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    visited.push(antecedent_flow_id);
                }
            }
            FlowNodeKind::ForIStat(_) => {
                // todo check for `for i = 1, 10 do end`
                if let Some(merged_type) =
                    try_get_multi_antecedent_type(db, tree, cache, root, var_ref_id, flow_node)?
                {
                    return Ok(merged_type);
                }
                antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                visited.push(antecedent_flow_id);
            }
            FlowNodeKind::TagCast(cast_ast_ptr) => {
                let tag_cast = cast_ast_ptr.to_node(root).ok_or(InferFailReason::None)?;
                let cast_or_continue =
                    get_type_at_cast_flow(db, tree, cache, root, var_ref_id, flow_node, tag_cast)?;

                if let ResultTypeOrContinue::Result(cast_type) = cast_or_continue {
                    return Ok(cast_type);
                } else {
                    if let Some(merged_type) =
                        try_get_multi_antecedent_type(db, tree, cache, root, var_ref_id, flow_node)?
                    {
                        return Ok(merged_type);
                    }
                    antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
                    visited.push(antecedent_flow_id);
                }
            }
        }
    }
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
            && let Ok(param_type) = infer_param(db, decl)
        {
            return Ok(param_type);
        }
    }

    get_var_ref_type(db, cache, var_ref_id)
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
) -> Result<Option<LuaType>, InferFailReason> {
    match flow_node.antecedent {
        Some(crate::FlowAntecedent::Multiple(_)) => Ok(Some(merge_antecedent_types(
            db, tree, cache, root, var_ref_id, flow_node,
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
) -> InferResult {
    if let Some(merged_type) =
        try_get_multi_antecedent_type(db, tree, cache, root, var_ref_id, flow_node)?
    {
        return Ok(merged_type);
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)
}

fn merge_antecedent_types(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
) -> InferResult {
    let antecedents = get_multi_antecedents(tree, flow_node)?;
    cache.prof_merge_calls += 1;
    cache.prof_merge_total_antecedents += antecedents.len() as u32;
    let target_realm = cache.flow_query_realm.unwrap_or_else(|| {
        db.get_gmod_infer_index()
            .get_realm_at_offset(&cache.get_file_id(), var_ref_id.get_position())
    });

    let mut result_type = LuaType::Unknown;
    let mut accepted_any = false;
    for &flow_id in &antecedents {
        let Some(antecedent_node) = tree.get_flow_node(flow_id) else {
            continue;
        };
        if matches!(
            antecedent_node.kind,
            FlowNodeKind::Unreachable | FlowNodeKind::Return | FlowNodeKind::Break
        ) {
            continue;
        }

        let antecedent_realm = get_flow_node_realm(db, cache.get_file_id(), root, antecedent_node);
        if !realms_can_reach(target_realm, antecedent_realm) {
            continue;
        }

        accepted_any = true;
        let branch_type = with_flow_query_realm(cache, target_realm, |cache| {
            get_merged_flow_type_or_nil(db, tree, cache, root, var_ref_id, flow_id)
        })?;
        result_type = TypeOps::Union.apply(db, &result_type, &branch_type);
    }

    if accepted_any {
        return Ok(result_type);
    }

    for &flow_id in &antecedents {
        let Some(antecedent_node) = tree.get_flow_node(flow_id) else {
            continue;
        };
        if matches!(
            antecedent_node.kind,
            FlowNodeKind::Unreachable | FlowNodeKind::Return | FlowNodeKind::Break
        ) {
            continue;
        }

        let branch_type = with_flow_query_realm(cache, target_realm, |cache| {
            get_merged_flow_type_or_nil(db, tree, cache, root, var_ref_id, flow_id)
        })?;
        result_type = TypeOps::Union.apply(db, &result_type, &branch_type);
    }

    Ok(result_type)
}

fn get_merged_flow_type_or_nil(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_id: FlowId,
) -> InferResult {
    match get_type_at_flow(db, tree, cache, root, var_ref_id, flow_id) {
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
        FlowNodeKind::DeclPosition(position) => Some(*position),
        FlowNodeKind::Assignment(assign_ptr, _) => {
            assign_ptr.to_node(root).map(|node| node.get_position())
        }
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
    }
}

/// Returns `true` when every direct antecedent of a `BranchLabel` / `NamedLabel`
/// is an alive flow node (not `Unreachable`, `Return`, or `Break`).
///
/// When all antecedents are alive, condition narrowing is guaranteed to cancel
/// out at the merge point, so variables that are not otherwise modified in any
/// branch keep the same type as at the common predecessor.
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

fn get_type_at_assign_stat(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    assign_stat: LuaAssignStat,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let (vars, exprs) = assign_stat.get_var_and_expr_list();
    for (i, var) in vars.iter().cloned().enumerate() {
        if let Some(prefix_collection_type) = maybe_get_collection_append_assignment_type(
            db, tree, cache, root, var_ref_id, flow_node, &var, &exprs, i,
        )? {
            return Ok(ResultTypeOrContinue::Result(prefix_collection_type));
        }

        let Some(maybe_ref_id) = get_var_expr_var_ref_id(db, cache, var.to_expr()) else {
            continue;
        };

        if maybe_ref_id != *var_ref_id {
            // let typ = get_var_ref_type(db, cache, var_ref_id)?;
            continue;
        }

        // Check if there's an explicit type annotation (not just inferred type)
        let var_id = match var {
            LuaVarExpr::NameExpr(name_expr) => {
                Some(LuaDeclId::new(cache.get_file_id(), name_expr.get_position()).into())
            }
            LuaVarExpr::IndexExpr(index_expr) => {
                Some(LuaMemberId::new(index_expr.get_syntax_id(), cache.get_file_id()).into())
            }
        };

        let explicit_var_type = var_id
            .and_then(|id| db.get_type_index().get_type_cache(&id))
            .filter(|tc| tc.is_doc())
            .map(|tc| tc.as_type().clone());

        let expr_type = infer_expr_list_value_type_at(db, cache, &exprs, i)?;
        let Some(expr_type) = expr_type else {
            return Ok(ResultTypeOrContinue::Continue);
        };

        let source_type = if let Some(explicit) = explicit_var_type.clone() {
            explicit
        } else {
            match get_antecedent_type_for_flow_node(db, tree, cache, root, var_ref_id, flow_node) {
                Ok(ty) => ty,
                Err(InferFailReason::UnResolveDeclType(decl_id))
                    if should_treat_unresolved_decl_as_nil(db, decl_id) =>
                {
                    LuaType::Nil
                }
                Err(err) => return Err(err),
            }
        };

        let narrowed = if source_type == LuaType::Nil {
            None
        } else {
            let declared =
                get_var_ref_type(db, cache, var_ref_id)
                    .ok()
                    .and_then(|decl| match decl {
                        LuaType::Def(_) | LuaType::Ref(_) => Some(decl),
                        _ => None,
                    });

            narrow_down_type(db, source_type.clone(), expr_type.clone(), declared)
        };

        let result_type = narrowed.unwrap_or(explicit_var_type.unwrap_or(expr_type));

        return Ok(ResultTypeOrContinue::Result(result_type));
    }

    Ok(ResultTypeOrContinue::Continue)
}

#[allow(clippy::too_many_arguments)]
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

    let source_type =
        match get_antecedent_type_for_flow_node(db, tree, cache, root, var_ref_id, flow_node) {
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
        if normalize_infer_collection_type(type_cache.as_type()).is_none() {
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

fn normalize_infer_collection_type(typ: &LuaType) -> Option<()> {
    match typ {
        LuaType::Array(_) => Some(()),
        LuaType::Tuple(tuple) if tuple.is_infer_resolve() => Some(()),
        _ => None,
    }
}

fn infer_collection_base_type(db: &DbIndex, typ: &LuaType) -> Option<LuaType> {
    match typ {
        LuaType::Array(array) => Some(array.get_base().clone()),
        LuaType::Tuple(tuple) if tuple.is_infer_resolve() => Some(tuple.cast_down_array_base(db)),
        _ => None,
    }
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
    let init_type = match infer_expr(db, cache, expr)? {
        LuaType::Variadic(variadic) => variadic.get_type(ret_idx).cloned().unwrap_or(LuaType::Nil),
        ty if ret_idx == 0 => ty,
        _ => LuaType::Nil,
    };

    Ok(Some(init_type))
}
