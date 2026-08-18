use std::collections::HashSet;

use crate::{
    DbIndex, FileId, LuaTypeCache, LuaTypeOwner,
    db_index::{LuaMemberId, LuaType, MemberAssignmentContributionKey},
};

use super::member_write_policy::{
    MemberAssignmentWideningDecision, MemberAssignmentWideningState,
    decide_member_assignment_widening, is_member_realm_compatible,
    union_member_assignment_widening,
};
use super::stats::is_assignment_file_define_member;

/// Re-derives member assignment widenings from the complete writer set.
pub(in crate::compilation::analyzer) fn rederive_contributed_member_assignments(
    db: &mut DbIndex,
    analyzed_files: &HashSet<FileId>,
) {
    let store_keys = db
        .get_member_index()
        .member_assignment_contributions()
        .keys_for_files(analyzed_files);
    if store_keys.is_empty() {
        return;
    }

    let mut updates = Vec::new();
    for store_key in &store_keys {
        collect_group_updates(db, store_key, &mut updates);
    }

    for (member_id, widened_type) in updates {
        db.get_type_index_mut().force_bind_type(
            LuaTypeOwner::Member(member_id),
            LuaTypeCache::InferType(widened_type),
        );
    }
}

/// The canonical merge for one owner/key group.
///
/// Every answer is computed from the recorded contributions alone and applied
/// afterwards, so no member's result can depend on another's being written
/// first.
fn collect_group_updates(
    db: &DbIndex,
    store_key: &MemberAssignmentContributionKey,
    updates: &mut Vec<(LuaMemberId, LuaType)>,
) -> Option<()> {
    let member_index = db.get_member_index();
    let group = member_index
        .member_assignment_contributions()
        .contributions(store_key)?;
    if group.len() < 2 {
        return Some(());
    }

    let (owner, key) = store_key;
    // The merge gives up when the group holds anything other than plain
    // assignment writers. Ask the unpruned history rather than the visible
    // members, which `retain_only_member_for_owner_key` has already shrunk.
    if member_index
        .get_current_owner_members_for_key(owner, key)
        .iter()
        .any(|member| !is_assignment_file_define_member(db, member.get_id()))
    {
        return Some(());
    }

    let mut contributions = group
        .iter()
        .filter(|(member_id, _)| member_index.get_member_owner(member_id) == Some(owner))
        .map(|(member_id, contribution)| (*member_id, contribution))
        .collect::<Vec<_>>();
    if contributions.len() < 2 {
        return Some(());
    }
    contributions.sort_by_key(|(member_id, _)| sort_key(*member_id));

    for (member_id, contribution) in &contributions {
        // A guarded bootstrap keeps its siblings visible and runs its own
        // literal-preserving merge, so it is not what the pruning destroyed.
        if contribution.guarded_bootstrap || contribution.preserve_table_literals {
            continue;
        }
        // "We could not tell" is a report about the batch, not about the write,
        // so it is not evidence this pass can merge or overwrite with.
        if is_uninformative(&contribution.source_type) {
            continue;
        }
        // Only an inferred assignment cache is this pass' to rewrite: a doc type
        // outranks inference, and anything else in the slot was put there by an
        // authority this pass has no evidence to overrule.
        let current_type = match db.get_type_index().get_type_cache(&(*member_id).into()) {
            Some(cache) if cache.is_infer() => cache.as_type().clone(),
            _ => continue,
        };

        // Only earlier writers are evidence for this one. The lua pass merges a
        // write with the siblings that already carry a type, which is the batch's
        // stand-in for "written before"; taking it from the writer set instead
        // keeps the first writer as narrow as it is today.
        let previous_states = contributions
            .iter()
            .take_while(|(other_id, _)| other_id != member_id)
            .filter(|(other_id, _)| is_member_realm_compatible(db, *member_id, *other_id))
            .filter_map(|(other_id, other)| {
                // The sibling's cache is the settled answer where it exists; the
                // recorded contribution stands in for the writers the merge
                // could not see, not for the ones it could.
                let state = match db.get_type_index().get_type_cache(&(*other_id).into()) {
                    Some(cache) if is_uninformative(cache.as_type()) => return None,
                    Some(cache) => MemberAssignmentWideningState::from_type_cache(cache),
                    None if is_uninformative(&other.bound_type) => return None,
                    None => MemberAssignmentWideningState::from_assigned_type(
                        &other.bound_type,
                        other.doc_type.clone(),
                    ),
                };
                Some(state)
            })
            .collect::<Vec<_>>();
        if previous_states.is_empty() {
            continue;
        }

        let widened_type = match decide_member_assignment_widening(
            db,
            &contribution.source_type,
            true,
            previous_states.iter(),
        ) {
            MemberAssignmentWideningDecision::Widened(widened_type) => widened_type,
            MemberAssignmentWideningDecision::ClassBootstrapRejected => {
                union_member_assignment_widening(
                    db,
                    &contribution.source_type,
                    true,
                    previous_states.iter(),
                )
            }
            MemberAssignmentWideningDecision::NoPreviousAssignments => continue,
        };

        if widened_type != current_type {
            updates.push((*member_id, widened_type));
        }
    }

    Some(())
}

/// A type that says the batch had no answer rather than what the answer is.
fn is_uninformative(typ: &LuaType) -> bool {
    typ.is_unknown() || typ.is_any()
}

fn sort_key(member_id: LuaMemberId) -> (u32, u32, u32) {
    let range = member_id.get_syntax_id().get_range();
    (
        member_id.file_id.id,
        range.start().into(),
        range.end().into(),
    )
}
