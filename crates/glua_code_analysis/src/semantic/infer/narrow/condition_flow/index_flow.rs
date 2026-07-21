use std::collections::HashSet;

use glua_parser::{LuaAstNode, LuaChunk, LuaExpr, LuaIndexExpr, LuaIndexMemberExpr};

use crate::{
    DbIndex, FlowNode, FlowTree, GmodRealm, InferFailReason, InferGuard, LuaInferCache,
    LuaMemberKey, LuaMemberOwner, LuaType, LuaTypeDeclId,
    semantic::{
        infer::{
            VarRefId,
            infer_index::infer_member_by_member_key,
            narrow::{
                ResultTypeOrContinue,
                condition_flow::{InferConditionFlow, get_condition_antecedent_type},
                get_type_at_flow::FlowWalkPolicy,
                narrow_false_or_nil, remove_false_or_nil,
                var_ref_id::{
                    get_var_expr_var_ref_id, is_untyped_param_rooted_index,
                    unknown_prefix_should_widen_to_any,
                },
            },
        },
        type_check::is_sub_type_of,
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
    policy: FlowWalkPolicy,
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
            policy,
        );
    }

    let antecedent_type =
        get_condition_antecedent_type(db, tree, cache, root, var_ref_id, flow_node, policy)?;

    if matches!(condition_flow, InferConditionFlow::TrueCondition)
        && antecedent_type.is_nil()
        && is_untyped_param_rooted_index(db, var_ref_id)
    {
        return Ok(ResultTypeOrContinue::Result(LuaType::Any));
    }

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
    policy: FlowWalkPolicy,
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
            policy,
        );
    }

    let left_type =
        get_condition_antecedent_type(db, tree, cache, root, var_ref_id, flow_node, policy)?;

    let index = LuaIndexMemberExpr::IndexExpr(index_expr);

    // Dynamic index keys (`t[expr]` where `expr` is not a compile-time
    // constant) produce `ExprType` member keys, which alias every other
    // dynamic access with the same inferred key type. Field-existence
    // narrowing would then collapse to whichever subtype happens to contain
    // an unrelated dynamic write — and a dynamic key's truthiness can't
    // identify a subtype anyway — so skip candidate narrowing entirely.
    if let Some(index_key) = index.get_index_key()
        && LuaMemberKey::index_key_is_dynamic(db, cache, &index_key)
    {
        return Ok(ResultTypeOrContinue::Continue);
    }

    let Some(index_key) = index.get_index_key() else {
        return Ok(ResultTypeOrContinue::Continue);
    };
    let Ok(member_key) = LuaMemberKey::from_index_key(db, cache, &index_key) else {
        return Ok(ResultTypeOrContinue::Continue);
    };

    // Base type already owns field directly: skip subtype search (avoids Entity -> EFFECT)
    // only when that ownership is usable at the call-site realm. A server-only base method
    // must not freeze the receiver as the base on client — fall through so realm-compatible
    // subtypes (e.g. NetworkVar getters) can win instead.
    if let LuaType::Ref(type_id) | LuaType::Def(type_id) = &left_type
        && type_directly_owns_member(db, type_id, &member_key)
        && type_member_usable_at_caller_realm(
            db,
            cache,
            type_id,
            &member_key,
            index.get_range().start(),
        )
    {
        return Ok(ResultTypeOrContinue::Result(left_type));
    }

    let Some(candidates) =
        collect_field_exist_narrow_candidates(db, cache, &left_type, &member_key, &index)
    else {
        // Indexing an authoritative Unknown base implies it is non-nil/non-false.
        // Inferred unknown aliases may still carry concrete dynamic member types,
        // so the shared guard decides whether widening to Any is safe.
        if matches!(left_type, LuaType::Unknown)
            && unknown_prefix_should_widen_to_any(db, var_ref_id)
        {
            return Ok(ResultTypeOrContinue::Result(LuaType::Any));
        }
        return Ok(ResultTypeOrContinue::Continue);
    };

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
        // `Never` means the member cannot exist on this candidate (e.g. the
        // `nil` arm of a union) — indexing it can never be truthy, so it must
        // not survive as field-exists evidence.
        if !member_type.is_always_falsy() && !member_type.is_never() {
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
            let narrowed = filter_candidates_by_caller_realm(db, cache, narrowed, &index);
            let narrowed = expand_surviving_subtypes_for_falsy_overrides(
                db,
                cache,
                &index,
                &member_key,
                narrowed,
            );
            let narrowed = collapse_to_most_generic_types(db, narrowed);
            if !narrowed.is_empty() {
                return Ok(ResultTypeOrContinue::Result(LuaType::from_vec(narrowed)));
            }
        }
        InferConditionFlow::FalseCondition => {
            // Exclude only the (already reverse-bounded) owners that have the field.
            // Do not expand open bases into "every subtype without the field".
            if !result.is_empty() {
                let antecedent_arms = collect_antecedent_nominal_arms(&left_type);
                let remaining = if antecedent_arms.is_empty() {
                    candidates
                        .into_iter()
                        .filter(|candidate| !result.contains(candidate))
                        .collect::<Vec<_>>()
                } else {
                    antecedent_arms
                        .into_iter()
                        .filter(|arm| {
                            !result.iter().any(|owner| {
                                arm == owner || is_strict_sub_type_of(db, arm, owner)
                            })
                        })
                        .collect::<Vec<_>>()
                };
                if !remaining.is_empty() {
                    return Ok(ResultTypeOrContinue::Result(LuaType::from_vec(remaining)));
                }
            }
        }
    }

    Ok(ResultTypeOrContinue::Continue)
}

fn type_directly_owns_member(
    db: &DbIndex,
    type_id: &crate::LuaTypeDeclId,
    member_key: &LuaMemberKey,
) -> bool {
    let member_index = db.get_member_index();
    let owner = LuaMemberOwner::Type(type_id.clone());
    if member_index.get_member_item(&owner, member_key).is_some() {
        return true;
    }

    let global_owner = LuaMemberOwner::GlobalPath(crate::GlobalId::new(type_id.get_name()));
    member_index
        .get_member_item(&global_owner, member_key)
        .is_some()
}

fn collect_field_exist_narrow_candidates(
    db: &DbIndex,
    cache: &LuaInferCache,
    left_type: &LuaType,
    member_key: &LuaMemberKey,
    index: &LuaIndexMemberExpr,
) -> Option<Vec<LuaType>> {
    let antecedent_arms = collect_antecedent_nominal_arms(left_type);
    if antecedent_arms.is_empty() {
        return None;
    }

    // Prefer reverse member-owner lookup over get_all_sub_types(Entity). Owners of
    // the key that are subtypes of an antecedent arm are the only useful candidates;
    // open bases must not fan out into every scripted subclass.
    let reverse_owners = collect_reverse_member_owner_types(db, member_key);
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    for owner in reverse_owners {
        if !owner_fits_antecedent(db, &owner, &antecedent_arms) {
            continue;
        }
        if let LuaType::Ref(type_id) | LuaType::Def(type_id) = &owner
            && !type_member_usable_at_caller_realm(
                db,
                cache,
                type_id,
                member_key,
                index.get_range().start(),
            )
        {
            continue;
        }
        if seen.insert(owner.clone()) {
            candidates.push(owner);
        }
    }

    // Antecedent arms themselves may own/inherit the field without appearing as a
    // reverse direct owner (inherited members live on a parent type).
    for arm in &antecedent_arms {
        if seen.contains(arm) {
            continue;
        }
        if let LuaType::Ref(type_id) | LuaType::Def(type_id) = arm
            && !type_member_usable_at_caller_realm(
                db,
                cache,
                type_id,
                member_key,
                index.get_range().start(),
            )
        {
            continue;
        }
        if seen.insert(arm.clone()) {
            candidates.push(arm.clone());
        }
    }

    // Narrow scripted antecedents only: expand subtypes when reverse lookup found
    // nothing beyond the arm itself. Never expand open engine bases (Entity, …).
    if candidates
        .iter()
        .all(|c| antecedent_arms.iter().any(|arm| arm == c))
    {
        for arm in &antecedent_arms {
            let (LuaType::Ref(type_id) | LuaType::Def(type_id)) = arm else {
                continue;
            };
            if is_open_base_type(type_id) {
                continue;
            }
            for sub in db.get_type_index().get_all_sub_types(type_id) {
                let sub_ty = LuaType::Ref(sub.get_id());
                if seen.insert(sub_ty.clone()) {
                    candidates.push(sub_ty);
                }
            }
        }
    }

    (!candidates.is_empty()).then_some(candidates)
}

fn collect_antecedent_nominal_arms(left_type: &LuaType) -> Vec<LuaType> {
    let mut arms = Vec::new();
    let mut seen = HashSet::new();
    collect_antecedent_nominal_arms_into(left_type, &mut arms, &mut seen);
    arms
}

fn collect_antecedent_nominal_arms_into(
    left_type: &LuaType,
    arms: &mut Vec<LuaType>,
    seen: &mut HashSet<LuaType>,
) {
    match left_type {
        LuaType::Union(union_type) => {
            for arm in union_type.types() {
                collect_antecedent_nominal_arms_into(arm, arms, seen);
            }
        }
        LuaType::Instance(instance_type) => {
            collect_antecedent_nominal_arms_into(instance_type.get_base(), arms, seen);
        }
        LuaType::Ref(_) | LuaType::Def(_) => {
            if seen.insert(left_type.clone()) {
                arms.push(left_type.clone());
            }
        }
        _ => {}
    }
}

fn collect_reverse_member_owner_types(db: &DbIndex, member_key: &LuaMemberKey) -> Vec<LuaType> {
    let mut owners = Vec::new();
    let mut seen = HashSet::new();
    for member in db.get_member_index().get_current_members_for_key(member_key) {
        let Some(owner) = db.get_member_index().get_current_owner(&member.get_id()) else {
            continue;
        };
        let type_id = match owner {
            LuaMemberOwner::Type(id) => id.clone(),
            LuaMemberOwner::GlobalPath(path) => LuaTypeDeclId::global(path.get_name()),
            _ => continue,
        };
        let ty = LuaType::Ref(type_id);
        if seen.insert(ty.clone()) {
            owners.push(ty);
        }
    }
    owners
}

fn owner_fits_antecedent(db: &DbIndex, owner: &LuaType, antecedent_arms: &[LuaType]) -> bool {
    let (LuaType::Ref(owner_id) | LuaType::Def(owner_id)) = owner else {
        return false;
    };
    antecedent_arms.iter().any(|arm| {
        let (LuaType::Ref(arm_id) | LuaType::Def(arm_id)) = arm else {
            return false;
        };
        owner_id == arm_id || is_sub_type_of(db, owner_id, arm_id)
    })
}

/// Engine / annotation roots with huge subclass fan-out. Expanding these via
/// `get_all_sub_types` is both slow and produces unusable mega-unions.
fn is_open_base_type(type_id: &LuaTypeDeclId) -> bool {
    matches!(
        type_id.get_name(),
        "Entity" | "Player" | "Vehicle" | "Weapon" | "NPC" | "NextBot" | "Panel" | "Vector" | "Angle"
    )
}

/// Prefer the most generic type that still satisfies field-exist: drop every
/// candidate that is a strict subtype of another surviving candidate.
fn collapse_to_most_generic_types(db: &DbIndex, candidates: Vec<LuaType>) -> Vec<LuaType> {
    if candidates.len() <= 1 {
        return candidates;
    }
    candidates
        .iter()
        .filter(|candidate| {
            !candidates.iter().any(|other| {
                other != *candidate && is_strict_sub_type_of(db, candidate, other)
            })
        })
        .cloned()
        .collect()
}

/// If a parent owns a truthy field but a subclass directly overrides the same
/// key with a falsy value, the parent alone is not a safe collapse target
/// (BrokenGlide.IsGlideVehicle = false under BaseGlide). Drop that parent and
/// keep truthy subtypes so surviving overrides remain visible.
fn expand_surviving_subtypes_for_falsy_overrides(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index: &LuaIndexMemberExpr,
    member_key: &LuaMemberKey,
    candidates: Vec<LuaType>,
) -> Vec<LuaType> {
    let reverse_owners = collect_reverse_member_owner_types(db, member_key);
    if reverse_owners.is_empty() || candidates.is_empty() {
        return candidates;
    }

    let mut falsy_override_parents: Vec<LuaType> = Vec::new();
    for candidate in &candidates {
        let (LuaType::Ref(_) | LuaType::Def(_)) = candidate else {
            continue;
        };
        let has_falsy_override = reverse_owners.iter().any(|owner| {
            if !is_strict_sub_type_of(db, owner, candidate) {
                return false;
            }
            match infer_member_by_member_key(db, cache, owner, index.clone(), &InferGuard::new()) {
                Ok(member_type) => member_type.is_always_falsy() || member_type.is_never(),
                Err(_) => false,
            }
        });
        if has_falsy_override {
            falsy_override_parents.push(candidate.clone());
        }
    }

    if falsy_override_parents.is_empty() {
        return candidates;
    }

    let mut expanded = Vec::new();
    let mut seen = HashSet::new();
    for candidate in &candidates {
        if falsy_override_parents.iter().any(|p| p == candidate) {
            continue;
        }
        if seen.insert(candidate.clone()) {
            expanded.push(candidate.clone());
        }
    }

    for parent in &falsy_override_parents {
        let (LuaType::Ref(parent_id) | LuaType::Def(parent_id)) = parent else {
            continue;
        };
        if is_open_base_type(parent_id) {
            // Unsafe to enumerate every Entity subclass; keep parent as best effort.
            if seen.insert(parent.clone()) {
                expanded.push(parent.clone());
            }
            continue;
        }
        let mut added_subtype = false;
        for sub in db.get_type_index().get_all_sub_types(parent_id) {
            let sub_ty = LuaType::Ref(sub.get_id());
            if !seen.insert(sub_ty.clone()) {
                continue;
            }
            match infer_member_by_member_key(db, cache, &sub_ty, index.clone(), &InferGuard::new())
            {
                Ok(member_type)
                    if !member_type.is_always_falsy() && !member_type.is_never() =>
                {
                    expanded.push(sub_ty);
                    added_subtype = true;
                }
                _ => {}
            }
        }
        // If no truthy subtype survived, fall back to the parent rather than
        // emptying the narrow (still better than the open-base mega-union).
        if !added_subtype && seen.insert(parent.clone()) {
            expanded.push(parent.clone());
        }
    }
    expanded
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

        // Prefer the most generic direct definer (drop strict subtypes of another
        // direct definer). Subclass overrides of the same field must not win over
        // the root that already defines it.
        direct
            .into_iter()
            .filter(|direct_type| {
                !direct_snapshot.iter().any(|other_direct| {
                    other_direct != direct_type
                        && is_strict_sub_type_of(db, direct_type, other_direct)
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

/// Drop candidates whose direct member decls are all realm-incompatible with the caller.
/// Uses annotation realm when present, otherwise the member's file/branch realm (so
/// NetworkVar getters in shared files stay, while `---@realm server` base methods drop).
/// Empty result means "no realm-valid candidate" — callers should not narrow to an
/// invalid type rather than falling back to the incompatible set.
fn filter_candidates_by_caller_realm(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    candidates: Vec<LuaType>,
    index: &LuaIndexMemberExpr,
) -> Vec<LuaType> {
    if candidates.is_empty() || !db.get_emmyrc().gmod.enabled {
        return candidates;
    }
    let caller_file_id = cache.get_file_id();
    let caller_offset = index.get_range().start();
    let gmod_infer = db.get_gmod_infer_index();
    let caller_mask = gmod_infer.get_state_mask_at_offset(&caller_file_id, caller_offset);
    if caller_mask.is_empty() {
        return candidates;
    }
    let Some(index_key) = index.get_index_key() else {
        return candidates;
    };
    let Ok(key) = LuaMemberKey::from_index_key(db, cache, &index_key) else {
        return candidates;
    };

    let filtered: Vec<LuaType> = candidates
        .iter()
        .filter(|t| {
            let type_id = match t {
                LuaType::Ref(id) | LuaType::Def(id) => id,
                _ => return true,
            };
            type_member_usable_at_caller_mask(db, type_id, &key, caller_mask)
        })
        .cloned()
        .collect();

    // Prefer realm-valid candidates. If every candidate was stripped for realm
    // incompatibility, return empty so the guard does not narrow to a dead type.
    if filtered.is_empty() {
        let had_direct_incompatible = candidates.iter().any(|t| {
            let type_id = match t {
                LuaType::Ref(id) | LuaType::Def(id) => id,
                _ => return false,
            };
            let owner = LuaMemberOwner::Type(type_id.clone());
            let global_owner = LuaMemberOwner::GlobalPath(crate::GlobalId::new(type_id.get_name()));
            let mut decls = direct_members_for_owner_key(db, &owner, &key);
            decls.extend(direct_members_for_owner_key(db, &global_owner, &key));
            !decls.is_empty()
                && !type_member_usable_at_caller_mask(db, type_id, &key, caller_mask)
        });
        if had_direct_incompatible {
            Vec::new()
        } else {
            candidates
        }
    } else {
        filtered
    }
}

fn type_member_usable_at_caller_realm(
    db: &DbIndex,
    cache: &LuaInferCache,
    type_id: &crate::LuaTypeDeclId,
    member_key: &LuaMemberKey,
    caller_offset: rowan::TextSize,
) -> bool {
    if !db.get_emmyrc().gmod.enabled {
        return true;
    }
    let caller_file_id = cache.get_file_id();
    let caller_mask = db
        .get_gmod_infer_index()
        .get_state_mask_at_offset(&caller_file_id, caller_offset);
    if caller_mask.is_empty() {
        return true;
    }
    type_member_usable_at_caller_mask(db, type_id, member_key, caller_mask)
}

fn type_member_usable_at_caller_mask(
    db: &DbIndex,
    type_id: &crate::LuaTypeDeclId,
    member_key: &LuaMemberKey,
    caller_mask: crate::GmodStateMask,
) -> bool {
    let owner = LuaMemberOwner::Type(type_id.clone());
    let global_owner = LuaMemberOwner::GlobalPath(crate::GlobalId::new(type_id.get_name()));
    let mut decls = direct_members_for_owner_key(db, &owner, member_key);
    decls.extend(direct_members_for_owner_key(db, &global_owner, member_key));
    if decls.is_empty() {
        // Inherited / no direct decl — can't decide from this type alone.
        return true;
    }
    let gmod_infer = db.get_gmod_infer_index();
    decls.iter().any(|member| {
        let member_realm = gmod_infer
            .get_member_annotation_realm_at_offset(&member.get_file_id(), member.get_range().start())
            .unwrap_or_else(|| {
                gmod_infer.get_realm_at_offset(&member.get_file_id(), member.get_range().start())
            });
        match member_realm {
            GmodRealm::Unknown => true,
            realm => caller_mask.is_compatible_with(realm.state_mask()),
        }
    })
}

fn direct_members_for_owner_key<'db>(
    db: &'db DbIndex,
    owner: &LuaMemberOwner,
    key: &LuaMemberKey,
) -> Vec<&'db crate::LuaMember> {
    let Some(member_item) = db.get_member_index().get_member_item(owner, key) else {
        return Vec::new();
    };

    member_item
        .get_member_ids()
        .into_iter()
        .filter_map(|member_id| {
            let member = db.get_member_index().get_member(&member_id)?;
            (member.get_key() == key).then_some(member)
        })
        .collect()
}

/// Walk up an index chain looking for the leftmost prefix that resolves to the
/// queried var_ref_id. If found, and we are in the truthy branch with an
/// authoritative `Unknown` base, narrow it to `Any` — successfully indexing any
/// link in the chain (e.g. `a.b.c.d`) implies every prefix is non-nil.
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
    policy: FlowWalkPolicy,
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

    let left_type =
        get_condition_antecedent_type(db, tree, cache, root, var_ref_id, flow_node, policy)?;
    if matches!(left_type, LuaType::Unknown) && unknown_prefix_should_widen_to_any(db, var_ref_id) {
        return Ok(ResultTypeOrContinue::Result(LuaType::Any));
    }
    Ok(ResultTypeOrContinue::Continue)
}
