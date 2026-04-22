use glua_parser::{LuaChunk, LuaExpr, LuaIndexExpr, LuaIndexMemberExpr};

use crate::{
    DbIndex, FlowNode, FlowTree, InferFailReason, InferGuard, LuaInferCache, LuaMemberKey,
    LuaMemberOwner, LuaType,
    semantic::infer::{
        VarRefId,
        infer_index::infer_member_by_member_key,
        narrow::{
            ResultTypeOrContinue, condition_flow::InferConditionFlow, get_single_antecedent,
            get_type_at_flow::get_type_at_flow, narrow_false_or_nil, remove_false_or_nil,
            var_ref_id::get_var_expr_var_ref_id,
        },
    },
};

#[allow(clippy::too_many_arguments)]
pub fn get_type_at_index_expr(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    index_expr: LuaIndexExpr,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    // The IndexExpr may not resolve to its own VarRefId — e.g. when the prefix
    // is an undefined global (`tmysql.Version`), `get_index_expr_var_ref_id`
    // bails out because it only handles `SelfRef`/`VarRef` prefixes. In that
    // case we still need to try prefix-based narrowing so that
    // `if tmysql.Version then` narrows the prefix `tmysql` itself.
    let name_var_ref_id =
        get_var_expr_var_ref_id(db, cache, LuaExpr::IndexExpr(index_expr.clone()));

    if name_var_ref_id.as_ref() != Some(var_ref_id) {
        return maybe_field_exist_narrow(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            index_expr,
            condition_flow,
        );
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let antecedent_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;

    let result_type = match condition_flow {
        InferConditionFlow::FalseCondition => narrow_false_or_nil(db, antecedent_type),
        InferConditionFlow::TrueCondition => remove_false_or_nil(antecedent_type),
    };

    Ok(ResultTypeOrContinue::Result(result_type))
}

#[allow(clippy::too_many_arguments)]
fn maybe_field_exist_narrow(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    index_expr: LuaIndexExpr,
    condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    let maybe_var_ref_id = get_var_expr_var_ref_id(db, cache, prefix_expr.clone());

    if maybe_var_ref_id.as_ref() != Some(var_ref_id) {
        // Direct prefix doesn't match the queried var. For an Unknown base in
        // the truthy branch we still want to narrow the *transitive* leftmost
        // name (e.g. `if a.b.c then` → `a` is non-nil), so fall through to the
        // transitive prefix scan below.
        return maybe_transitive_unknown_prefix_narrow(
            db,
            tree,
            cache,
            root,
            var_ref_id,
            flow_node,
            &prefix_expr,
            condition_flow,
        );
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let left_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;

    // Bug fix: when the input is a single concrete `Ref`/`Def` that already
    // directly defines the queried field, the field-existence check is
    // useless since every value of this type already has the field. Falling
    // through to class expansion + "direct definer" filtering
    // can sometimes narrow to a an incorrect override (e.g. `Entity` →
    // `EFFECT` because `EFFECT` overrides `EndTouch`), which then causes
    // false-positive realm/type diagnostics.
    if matches!(condition_flow, InferConditionFlow::TrueCondition)
        && let LuaType::Ref(type_id) | LuaType::Def(type_id) = &left_type
    {
        let index_member = LuaIndexMemberExpr::IndexExpr(index_expr.clone());
        if let Some(index_key) = index_member.get_index_key()
            && let Ok(member_key) = LuaMemberKey::from_index_key(db, cache, &index_key)
        {
            let member_index = db.get_member_index();
            let owner = LuaMemberOwner::Type(type_id.clone());
            let global_owner =
                LuaMemberOwner::GlobalPath(crate::GlobalId::new(type_id.get_name()));
            if member_index.get_member_item(&owner, &member_key).is_some()
                || member_index
                    .get_member_item(&global_owner, &member_key)
                    .is_some()
            {
                return Ok(ResultTypeOrContinue::Result(left_type));
            }
        }
    }

    let Some(candidates) = collect_field_exist_narrow_candidates(db, &left_type) else {
        // Indexing an Unknown base (e.g. an undefined global like `tmysql.Version`)
        // implies the base is non-nil/non-false at this point — both branches of
        // an `if tmysql.X then ... else ... end` only execute if `tmysql.X` was
        // successfully evaluated, which requires `tmysql` to be non-nil. Narrow
        // Unknown → Any so subsequent reads aren't reported as `unknown`.
        if matches!(left_type, LuaType::Unknown) {
            return Ok(ResultTypeOrContinue::Result(LuaType::Any));
        }
        return Ok(ResultTypeOrContinue::Continue);
    };

    let index = LuaIndexMemberExpr::IndexExpr(index_expr);
    let mut result = vec![];
    for sub_type in &candidates {
        let member_type = match infer_member_by_member_key(
            db,
            cache,
            sub_type,
            index.clone(),
            &InferGuard::new(),
        ) {
            Ok(member_type) => member_type,
            Err(_) => continue, // If we cannot infer the member type, skip this type
        };
        // donot use always true
        if !member_type.is_always_falsy() {
            result.push(sub_type.clone());
        }
    }

    match condition_flow {
        InferConditionFlow::TrueCondition => {
            let direct_definers =
                find_safe_direct_field_definers(db, cache, &candidates, &result, &index);
            let narrowed = if !direct_definers.is_empty() {
                direct_definers
            } else {
                result
            };
            if !narrowed.is_empty() {
                return Ok(ResultTypeOrContinue::Result(LuaType::from_vec(narrowed)));
            }
        }
        InferConditionFlow::FalseCondition => {
            // Use the original (non-collapsed) result to determine which types to exclude,
            // so subtypes that have the field through inheritance are correctly excluded.
            if !result.is_empty() {
                let remaining = candidates
                    .into_iter()
                    .filter(|candidate| !result.contains(candidate))
                    .collect::<Vec<_>>();
                if !remaining.is_empty() {
                    return Ok(ResultTypeOrContinue::Result(LuaType::from_vec(remaining)));
                }
            }
        }
    }

    Ok(ResultTypeOrContinue::Continue)
}

fn collect_field_exist_narrow_candidates(
    db: &DbIndex,
    left_type: &LuaType,
) -> Option<Vec<LuaType>> {
    const MAX_CANDIDATES: usize = 128;

    match left_type {
        LuaType::Union(union_type) => Some(union_type.into_vec().to_vec()),
        LuaType::Ref(type_decl_id) | LuaType::Def(type_decl_id) => {
            let mut candidates = vec![LuaType::Ref(type_decl_id.clone())];
            let all_sub_types = db.get_type_index().get_all_sub_types(type_decl_id);
            if all_sub_types.len() > MAX_CANDIDATES {
                return None;
            }

            for sub_type in all_sub_types {
                candidates.push(LuaType::Ref(sub_type.get_id()));
            }

            Some(candidates)
        }
        LuaType::Instance(instance_type) => {
            collect_field_exist_narrow_candidates(db, instance_type.get_base())
        }
        _ => None,
    }
}

/// From a set of candidate types that have a given field (potentially through inheritance),
/// find only those types that DIRECTLY define the field on themselves.
/// For example, if `base_glide` defines `IsGlideVehicle` and `base_glide_car` inherits it,
/// this returns only `[base_glide]`.
/// Falls back to the full candidate set if no direct definers can be identified.
fn find_safe_direct_field_definers(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    _all_candidates: &[LuaType],
    candidates: &[LuaType],
    index: &LuaIndexMemberExpr,
) -> Vec<LuaType> {
    let index_key = match index.get_index_key() {
        Some(key) => key,
        None => return candidates.to_vec(),
    };
    let key = match LuaMemberKey::from_index_key(db, cache, &index_key) {
        Ok(key) => key,
        Err(_) => return candidates.to_vec(),
    };

    let member_index = db.get_member_index();
    let direct: Vec<LuaType> = candidates
        .iter()
        .filter(|t| {
            let type_id = match t {
                LuaType::Ref(id) | LuaType::Def(id) => id,
                _ => return true, // Keep non-Ref/Def types
            };
            let owner = LuaMemberOwner::Type(type_id.clone());
            // Check if this type directly owns the member (no inheritance walk)
            if member_index.get_member_item(&owner, &key).is_some() {
                return true;
            }
            // Also check GlobalPath ownership (for patterns like ENTITY.foo)
            let global_owner = LuaMemberOwner::GlobalPath(crate::GlobalId::new(type_id.get_name()));
            member_index.get_member_item(&global_owner, &key).is_some()
        })
        .cloned()
        .collect();

    if direct.is_empty() {
        // Fallback: no direct definers found (shouldn't happen normally)
        candidates.to_vec()
    } else {
        let direct_snapshot = direct.clone();
        if direct_snapshot.iter().any(|direct_type| {
            _all_candidates.iter().any(|candidate| {
                !candidates.contains(candidate) && is_strict_sub_type_of(db, candidate, direct_type)
            })
        }) {
            return candidates.to_vec();
        }

        direct
            .into_iter()
            .filter(|direct_type| {
                !direct_snapshot.iter().any(|other_direct| {
                    other_direct != direct_type
                        && is_strict_sub_type_of(db, other_direct, direct_type)
                })
            })
            .collect()
    }
}

fn is_strict_sub_type_of(db: &DbIndex, candidate: &LuaType, possible_base: &LuaType) -> bool {
    let (LuaType::Ref(candidate_id) | LuaType::Def(candidate_id)) = candidate else {
        return false;
    };
    let (LuaType::Ref(base_id) | LuaType::Def(base_id)) = possible_base else {
        return false;
    };

    candidate_id != base_id
        && crate::semantic::type_check::is_sub_type_of(db, &candidate_id.clone(), base_id)
}

/// Walk up an index chain looking for the leftmost prefix that resolves to the
/// queried ar_ref_id. If found, and we are in the truthy branch with the
/// base type still `Unknown`, narrow it to `Any` — successfully indexing
/// any link in the chain (e.g. `a.b.c.d`) implies every prefix is non-nil.
///
/// We intentionally only widen `Unknown` here. For known base types, walking
/// up beyond the immediate prefix would require recomputing field-existence
/// candidates against intermediate IndexExpr types, which the regular path
/// (maybe_field_exist_narrow) already handles when it actually matches.
#[allow(clippy::too_many_arguments)]
fn maybe_transitive_unknown_prefix_narrow(
    db: &DbIndex,
    tree: &FlowTree,
    cache: &mut LuaInferCache,
    root: &LuaChunk,
    var_ref_id: &VarRefId,
    flow_node: &FlowNode,
    prefix_expr: &LuaExpr,
    _condition_flow: InferConditionFlow,
) -> Result<ResultTypeOrContinue, InferFailReason> {
    let mut current = prefix_expr.clone();
    loop {
        match current {
            LuaExpr::IndexExpr(idx) => {
                let Some(next_prefix) = idx.get_prefix_expr() else {
                    return Ok(ResultTypeOrContinue::Continue);
                };
                current = next_prefix;
            }
            LuaExpr::NameExpr(_) => break,
            _ => return Ok(ResultTypeOrContinue::Continue),
        }
    }

    let Some(leftmost_var_ref_id) = get_var_expr_var_ref_id(db, cache, current) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    if leftmost_var_ref_id != *var_ref_id {
        return Ok(ResultTypeOrContinue::Continue);
    }

    let antecedent_flow_id = get_single_antecedent(tree, flow_node)?;
    let left_type = get_type_at_flow(db, tree, cache, root, var_ref_id, antecedent_flow_id)?;
    if matches!(left_type, LuaType::Unknown) {
        return Ok(ResultTypeOrContinue::Result(LuaType::Any));
    }
    Ok(ResultTypeOrContinue::Continue)
}
