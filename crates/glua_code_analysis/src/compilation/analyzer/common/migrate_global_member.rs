use std::collections::HashSet;

use crate::{DbIndex, GlobalId, InFiled, LuaDeclId, LuaMemberId, LuaMemberOwner, LuaTypeOwner};

use super::get_owner_id;
use crate::compilation::analyzer::lua::is_guarded_table_assignment_member;

/// Re-derives the non-overwriting mark before a re-home elects a visible
/// member.
fn restore_non_overwriting_mark(db: &mut DbIndex, member_id: LuaMemberId) {
    if is_guarded_table_assignment_member(db, member_id) {
        db.get_member_index_mut()
            .mark_non_overwriting_assignment_member(member_id);
    }
}

/// The owner a global declaration resolves to, falling back to the table
/// literal it is written with when inference has not reached it yet.
fn declaration_owner(db: &DbIndex, decl_id: LuaDeclId) -> Option<LuaMemberOwner> {
    get_owner_id(db, &decl_id.into()).or_else(|| {
        let range = db.get_decl_index().get_global_initializer_table(&decl_id)?;
        Some(LuaMemberOwner::Element(InFiled::new(
            decl_id.file_id,
            range,
        )))
    })
}

pub fn migrate_global_members_when_type_resolve(
    db: &mut DbIndex,
    type_owner: LuaTypeOwner,
) -> Option<()> {
    match type_owner {
        LuaTypeOwner::Decl(decl_id) => {
            migrate_global_member_to_decl(db, decl_id);
        }
        LuaTypeOwner::Member(member_id) => {
            migrate_global_member_to_member(db, member_id);
        }
        _ => {}
    }
    Some(())
}

pub fn migrate_global_path_members_when_owner_resolved(
    db: &mut DbIndex,
    global_id: &GlobalId,
) -> Option<()> {
    let decl_ids = db
        .get_global_index()
        .get_global_decl_ids(global_id.get_name())?
        .clone();

    for decl_id in decl_ids {
        alias_global_members_to_decl_owner(db, decl_id);
    }

    Some(())
}

fn alias_global_members_to_decl_owner(db: &mut DbIndex, decl_id: LuaDeclId) -> Option<()> {
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    if !decl.is_global() {
        return None;
    }

    let owner_id = get_owner_id(db, &decl_id.into())?;

    let name = decl.get_name();
    let global_id = GlobalId::new(name);
    let members = db
        .get_member_index()
        .get_members(&LuaMemberOwner::GlobalPath(global_id))?
        .iter()
        .filter(|member| member.get_feature().is_meta_decl())
        .map(|member| member.get_id())
        .collect::<Vec<_>>();

    let member_index = db.get_member_index_mut();
    for member_id in members {
        member_index.add_member_alias_to_owner(owner_id.clone(), member_id);
    }

    Some(())
}

/// Reconciles every global path whose members are still parked on it.
pub fn reconcile_parked_global_path_members(db: &mut DbIndex) {
    // Sorted by name, so a parent path is always reconciled before the nested
    // paths whose election reads the owners it just settled.
    for global_id in db.get_member_index().sorted_global_path_owners() {
        let global_path_owner = LuaMemberOwner::GlobalPath(global_id.clone());
        let Some(candidates) = elected_global_owners(db, &global_id) else {
            continue;
        };
        let Some((_, canonical_owner)) = candidates.first() else {
            continue;
        };
        if *canonical_owner == global_path_owner {
            continue;
        }

        // Every member of the global path, and whether it still needs
        // re-homing.
        let (members, hidden) = {
            let member_index = db.get_member_index();
            let visible = member_index
                .get_members(&global_path_owner)
                .map(|members| {
                    members
                        .iter()
                        .map(|member| member.get_id())
                        .collect::<HashSet<_>>()
                })
                .unwrap_or_default();
            let mut hidden = HashSet::new();
            let members = member_index
                .get_member_history(&global_path_owner)
                .iter()
                .map(|member| member.get_id())
                // The same exclusion `migrate_global_path_members` applies,
                // and for the same reason: a member scripted-class
                // synthesis claimed belongs to that class. Reconciliation
                // ran without it, so a derma file's `PANEL` methods were
                // aliased onto every panel class the file's `PANEL`
                // declaration had been rewritten to.
                .filter(|member_id| {
                    !member_index.has_synthesized_owner(member_id)
                        && !file_hands_global_to_scripted_class(db, member_id.file_id, &global_id)
                })
                .map(|member_id| {
                    if !visible.contains(&member_id) {
                        hidden.insert(member_id);
                        return (member_id, false);
                    }
                    let needs_rehome = member_index
                        .get_member_owner(&member_id)
                        .is_none_or(|owner| *owner == global_path_owner);
                    (member_id, needs_rehome)
                })
                .collect::<Vec<_>>();
            (members, hidden)
        };
        if members.is_empty() {
            continue;
        }

        // After the early-out: the guarded repair is part of reconciling a
        // global that still has parked members, not a pass of its own.
        alias_guarded_assignment_members_across_candidates(db, &candidates);
        rehome_members_onto_their_own_files_table(db, &candidates);

        let declaring_files = declaring_files(db, &global_id);

        for (member_id, needs_rehome) in members {
            // A file that declares the global itself owns the members it
            // contributes: `marauth = marauth or {}` in two files describes one
            // runtime table, but each file's fields belong to the table literal
            // that file wrote. Falling back to the elected owner covers files
            // that only extend a global they never declare.
            let target_owner = match candidates
                .iter()
                .find(|(file_id, _)| *file_id == member_id.file_id)
            {
                Some((_, owner)) => Some(owner.clone()),
                // See `migrate_global_path_members`: a file that declares
                // the global but has not resolved its table keeps its
                // members parked rather than sharing a sibling file's
                // overwrite slot.
                None if declaring_files.contains(&member_id.file_id) => None,
                None => Some(canonical_owner.clone()),
            };

            let rehome_target = target_owner.clone().filter(|target_owner| {
                needs_rehome
                    && db
                        .get_member_index()
                        .get_member_owner(&member_id)
                        .is_none_or(|owner| owner != target_owner)
            });
            if let Some(target_owner) = rehome_target {
                restore_non_overwriting_mark(db, member_id);
                let member_index = db.get_member_index_mut();
                member_index.set_member_owner(target_owner.clone(), member_id.file_id, member_id);
                member_index.add_member_to_owner(target_owner, member_id);
            }
            let member_index = db.get_member_index_mut();
            // Aliasing the remaining candidates is what makes a global
            // declared once per realm behave like the single table it is at
            // runtime, and it has to run for members that already reached a
            // concrete owner too. Re-indexing a file rebuilds its members
            // from scratch, so the aliases the original migration created
            // are gone; gating the repair behind `needs_rehome` meant they
            // were only ever rebuilt for members still parked on the global
            // path.
            for (alias_file_id, alias_owner) in &candidates {
                if hidden.contains(&member_id) && *alias_file_id != member_id.file_id {
                    continue;
                }
                if Some(alias_owner) != target_owner.as_ref() {
                    member_index.add_member_alias_to_owner(alias_owner.clone(), member_id);
                }
            }
        }
    }
}

/// Moves a member that landed on a *sibling* file's table literal onto the
/// one its own file declares.
fn rehome_members_onto_their_own_files_table(
    db: &mut DbIndex,
    candidates: &[(crate::FileId, LuaMemberOwner)],
) {
    if candidates.len() < 2 {
        return;
    }

    let member_index = db.get_member_index();
    let mut seen = HashSet::new();
    let moves = candidates
        .iter()
        .flat_map(|(_, owner)| member_index.get_member_history(owner))
        .map(|member| member.get_id())
        .filter(|member_id| !member_index.has_synthesized_owner(member_id))
        .filter_map(|member_id| {
            // "The table its own file declares" only names one table when the
            // file declares the path once. `bullet = {} … bullet.Src = …`
            // written twice in one weapon file is two unrelated tables, and
            // collapsing the second block's fields onto the first erases them.
            let mut own = candidates
                .iter()
                .filter(|(file_id, _)| *file_id == member_id.file_id);
            let (_, target_owner) = own.next()?;
            if own.next().is_some() {
                return None;
            }
            let current_owner = member_index.get_member_owner(&member_id)?;
            (current_owner != target_owner
                && candidates
                    .iter()
                    .any(|(_, candidate)| candidate == current_owner)
                && seen.insert(member_id))
            .then(|| (member_id, current_owner.clone(), target_owner.clone()))
        })
        .collect::<Vec<_>>();

    for (member_id, current_owner, target_owner) in moves {
        // A member deferred resolution created never belonged to the table
        // it was provisionally placed on, so the move has to take its
        // enumerability with it: `set_member_owner` rewrites the current
        // owner but leaves the item, and that leftover is a fact the other
        // analysis order never produced. Every other member reached its
        // owner from a settled fact and is reachable through several tables
        // on purpose — detaching those cost real facts.
        if db
            .get_member_index()
            .is_deferred_index_expr_member(&member_id)
        {
            db.get_member_index_mut()
                .detach_member_from_owner(&current_owner, member_id);
        }
        restore_non_overwriting_mark(db, member_id);
        let member_index = db.get_member_index_mut();
        member_index.set_member_owner(target_owner.clone(), member_id.file_id, member_id);
        member_index.add_member_to_owner(target_owner, member_id);
    }
}

/// Makes every guarded `X.k = X.k or …` write reachable through every table
/// literal `X` is declared with.
fn alias_guarded_assignment_members_across_candidates(
    db: &mut DbIndex,
    candidates: &[(crate::FileId, LuaMemberOwner)],
) {
    if candidates.len() < 2 {
        return;
    }

    let member_index = db.get_member_index();
    let guarded = candidates
        .iter()
        .flat_map(|(_, owner)| member_index.get_member_history(owner))
        .map(|member| member.get_id())
        .filter(|member_id| member_index.is_non_overwriting_assignment_member(*member_id))
        .collect::<Vec<_>>();
    if guarded.is_empty() {
        return;
    }

    let member_index = db.get_member_index_mut();
    for member_id in guarded {
        for (_, owner) in candidates {
            member_index.add_member_alias_to_owner(owner.clone(), member_id);
        }
    }
}

/// The elected owners of `global_id`, computed purely from current index
/// state.
fn elected_global_owners(
    db: &DbIndex,
    global_id: &GlobalId,
) -> Option<Vec<(crate::FileId, LuaMemberOwner)>> {
    match global_id.get_prev_id() {
        Some(parent_id) => {
            let declaring_member_ids = declaring_member_ids(db, global_id, parent_id);

            elect_owners(declaring_member_ids.into_iter().filter_map(|member_id| {
                let owner = get_owner_id(db, &member_id.into())?;
                Some((
                    global_member_sort_key(db, member_id),
                    member_id.file_id,
                    owner,
                ))
            }))
        }
        None => {
            let decl_ids = db
                .get_global_index()
                .get_global_decl_ids(global_id.get_name())?;

            elect_owners(
                decl_ids
                    .iter()
                    .copied()
                    .filter(|decl_id| {
                        db.get_decl_index()
                            .get_decl(decl_id)
                            .is_some_and(|decl| decl.is_global())
                    })
                    .filter_map(|decl_id| {
                        let owner = declaration_owner(db, decl_id)?;
                        Some((global_decl_sort_key(db, decl_id), decl_id.file_id, owner))
                    }),
            )
        }
    }
}

fn elect_owners<K: Ord>(
    candidates: impl Iterator<Item = (K, crate::FileId, LuaMemberOwner)>,
) -> Option<Vec<(crate::FileId, LuaMemberOwner)>> {
    let mut candidates = candidates.collect::<Vec<_>>();
    if candidates.is_empty() {
        return None;
    }

    candidates.sort_by(|(left, _, _), (right, _, _)| left.cmp(right));

    let mut elected: Vec<(crate::FileId, LuaMemberOwner)> = Vec::with_capacity(candidates.len());
    for (_, file_id, owner) in candidates {
        if !elected.iter().any(|(_, existing)| *existing == owner) {
            elected.push((file_id, owner));
        }
    }
    Some(elected)
}

fn migrate_global_member_to_decl(db: &mut DbIndex, decl_id: LuaDeclId) -> Option<()> {
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    if !decl.is_global() {
        return None;
    }

    let global_id = GlobalId::new(decl.get_name());
    let owners = resolved_global_decl_owners(db, &global_id, decl_id)?;
    migrate_global_path_members(db, &global_id, &owners)
}

/// Every declaration of `global_id` that already carries a resolved type,
/// ordered deterministically with the canonical owner first.
fn resolved_global_decl_owners(
    db: &DbIndex,
    global_id: &GlobalId,
    triggering_decl_id: LuaDeclId,
) -> Option<Vec<(crate::FileId, LuaMemberOwner)>> {
    let sibling_decl_ids = db
        .get_global_index()
        .get_global_decl_ids(global_id.get_name())
        .map(Vec::as_slice)
        .unwrap_or_default();

    // Almost every global is declared exactly once. Keep that path
    // allocation free and identical in cost to electing by arrival.
    get_owner_id(db, &triggering_decl_id.into())?;

    // Rank every declaration up front so the canonical owner is decided by
    // source position rather than by which declaration this event arrived
    // after. Declarations that have not resolved an owner yet drop out here and
    // re-enter through their own resolution event.
    let mut ranked = sibling_decl_ids
        .iter()
        .copied()
        .chain(std::iter::once(triggering_decl_id))
        .filter(|decl_id| {
            db.get_decl_index()
                .get_decl(decl_id)
                .is_some_and(|decl| decl.is_global())
        })
        .map(|decl_id| (global_decl_sort_key(db, decl_id), decl_id))
        .collect::<Vec<_>>();
    ranked.sort_by(|(left, _), (right, _)| left.cmp(right));
    ranked.dedup_by(|(_, left), (_, right)| left == right);

    elect_owners(ranked.into_iter().filter_map(|(sort_key, decl_id)| {
        let owner = declaration_owner(db, decl_id)?;
        Some((sort_key, decl_id.file_id, owner))
    }))
}

/// Stable ordering key for a global declaration.
///
/// Keyed on the normalized source path first so the election survives `FileId`
/// renumbering when files are added or removed during a session.
fn global_decl_sort_key(db: &DbIndex, decl_id: LuaDeclId) -> (String, u32, u32) {
    (
        normalized_file_path(db, decl_id.file_id),
        decl_id.file_id.id,
        u32::from(decl_id.position),
    )
}

fn normalized_file_path(db: &DbIndex, file_id: crate::FileId) -> String {
    db.get_vfs()
        .get_file_path(&file_id)
        .map(|path| crate::vfs::normalize_path_for_ordering(&path.to_string_lossy()))
        .unwrap_or_default()
}

/// Moves the members parked under `GlobalPath(global_id)` onto the
/// canonical owner and aliases them onto every other declaration of the
/// same global.
fn migrate_global_path_members(
    db: &mut DbIndex,
    global_id: &GlobalId,
    owners: &[(crate::FileId, LuaMemberOwner)],
) -> Option<()> {
    let (_, canonical_owner) = owners.first()?;
    let member_index = db.get_member_index();
    let members = member_index
        .get_members(&LuaMemberOwner::GlobalPath(global_id.clone()))?
        .iter()
        .map(|member| member.get_id())
        // A member that scripted-class synthesis already claimed belongs to
        // that class, not to this global. `PANEL` is the case that matters:
        // it is a per-file scratch table consumed by `vgui.Register`, but
        // its methods stay enumerable under `GlobalPath("PANEL")`, so
        // without this every derma file's methods would be re-homed onto
        // whichever `PANEL` declaration won the election.
        .filter(|member_id| {
            !member_index.has_synthesized_owner(member_id)
                && !file_hands_global_to_scripted_class(db, member_id.file_id, global_id)
        })
        .collect::<Vec<_>>();

    if members.is_empty() {
        return Some(());
    }
    // Only needed to place members, so it is computed after the early-outs
    // above: on a cold build the overwhelming majority of these events find
    // nothing parked, and this walks every declaration of the global.
    let declaring_files = declaring_files(db, global_id);

    for member_id in members {
        // Same rule the end-of-batch reconciliation uses: a file that
        // declares the global owns the fields it writes, and only files
        // that merely extend a global they never declare fall back to the
        // elected owner.
        let target_owner = match owners
            .iter()
            .find(|(file_id, _)| *file_id == member_id.file_id)
        {
            Some((_, owner)) => owner.clone(),
            // The member's own file declares the global but has not resolved a
            // table for it yet. Leave it parked rather than re-homing it onto a
            // sibling's table; it re-enters through its own declaration's
            // resolution event, or through the end-of-batch reconciliation.
            None if declaring_files.contains(&member_id.file_id) => continue,
            None => canonical_owner.clone(),
        };

        restore_non_overwriting_mark(db, member_id);
        let member_index = db.get_member_index_mut();
        member_index.set_member_owner(target_owner.clone(), member_id.file_id, member_id);
        member_index.add_member_to_owner(target_owner.clone(), member_id);
        for (_, alias_owner) in owners {
            if *alias_owner != target_owner {
                member_index.add_member_alias_to_owner(alias_owner.clone(), member_id);
            }
        }
    }

    Some(())
}

fn migrate_global_member_to_member(db: &mut DbIndex, member_id: LuaMemberId) -> Option<()> {
    let member = db.get_member_index().get_member(&member_id)?;
    let global_id = member.get_global_id()?.clone();
    let owners = resolved_global_member_owners(db, &global_id, member_id)?;
    migrate_global_path_members(db, &global_id, &owners)
}

/// The nested-path counterpart of [`resolved_global_decl_owners`].
fn resolved_global_member_owners(
    db: &DbIndex,
    global_id: &GlobalId,
    member_id: LuaMemberId,
) -> Option<Vec<(crate::FileId, LuaMemberOwner)>> {
    let Some(parent_id) = global_id.get_prev_id() else {
        return get_owner_id(db, &member_id.into()).map(|owner| vec![(member_id.file_id, owner)]);
    };

    let declaring_member_ids = declaring_member_ids(db, global_id, parent_id);

    // See `resolved_global_decl_owners`: the resolution of `member_id` is
    // the event being handled, so it must have produced an owner for this
    // call to carry information. The owner itself is not used as an answer
    // — there is no shortcut for the single-declaration case, because
    // returning the triggering member's owner unranked hands the whole path
    // to whichever file happened to fire, and every other file's members
    // then fall back to it as the canonical owner.
    get_owner_id(db, &member_id.into())?;

    // See `resolved_global_decl_owners`: rank every declaring member up front so
    // the canonical owner is decided by source position, not by arrival.
    let mut ranked = declaring_member_ids
        .into_iter()
        .chain(std::iter::once(member_id))
        .map(|declaring_id| (global_member_sort_key(db, declaring_id), declaring_id))
        .collect::<Vec<_>>();
    ranked.sort_by(|(left, _), (right, _)| left.cmp(right));
    ranked.dedup_by(|(_, left), (_, right)| left == right);

    elect_owners(ranked.into_iter().filter_map(|(sort_key, declaring_id)| {
        let owner = get_owner_id(db, &declaring_id.into())?;
        Some((sort_key, declaring_id.file_id, owner))
    }))
}

/// The members that declare the nested path `global_id` under `parent_id`.
fn declaring_member_ids(
    db: &DbIndex,
    global_id: &GlobalId,
    parent_id: GlobalId,
) -> Vec<LuaMemberId> {
    db.get_member_index()
        .get_member_history_for_global_path(&LuaMemberOwner::GlobalPath(parent_id), global_id)
}

/// Whether `file_id` hands the global `global_id` to a scripted-class
/// registration, i.e. uses it as a scratch table the way derma files use
/// `PANEL = {} … vgui.Register("X", PANEL, …)`.
pub(crate) fn file_hands_global_to_scripted_class(
    db: &DbIndex,
    file_id: crate::FileId,
    global_id: &GlobalId,
) -> bool {
    if global_id.get_prev_id().is_some() {
        return false;
    }
    let Some(metadata) = db
        .get_gmod_class_metadata_index()
        .get_file_metadata(&file_id)
    else {
        return false;
    };

    metadata
        .vgui_register_calls
        .iter()
        .chain(metadata.vgui_register_table_calls.iter())
        .chain(metadata.derma_define_control_calls.iter())
        .chain(metadata.scripted_ent_register_calls.iter())
        .any(|call| {
            call.args
                .iter()
                .filter_map(|arg| arg.value.as_ref())
                .any(|value| {
                    matches!(value, crate::GmodClassCallLiteral::NameRef(name) if name == global_id.get_name())
                })
        })
}

/// The files that declare `global_id` themselves, whether or not their
/// declaration has resolved an owner yet.
fn declaring_files(db: &DbIndex, global_id: &GlobalId) -> HashSet<crate::FileId> {
    match global_id.get_prev_id() {
        Some(parent_id) => declaring_member_ids(db, global_id, parent_id)
            .into_iter()
            .map(|member_id| member_id.file_id)
            .collect(),
        None => db
            .get_global_index()
            .get_global_decl_ids(global_id.get_name())
            .map(|decl_ids| decl_ids.iter().map(|decl_id| decl_id.file_id).collect())
            .unwrap_or_default(),
    }
}

/// Stable ordering key for a member that declares a nested global path.
fn global_member_sort_key(db: &DbIndex, member_id: LuaMemberId) -> (String, u32, u32) {
    (
        normalized_file_path(db, member_id.file_id),
        member_id.file_id.id,
        u32::from(member_id.get_syntax_id().get_range().start()),
    )
}

#[cfg(test)]
mod tests {
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    use crate::{
        FileId, GlobalId, LuaDecl, LuaDeclExtra, LuaDeclarationTree, LuaMember, LuaMemberFeature,
        LuaMemberKey, LuaMemberOwner, LuaType, LuaTypeCache, LuaTypeDeclId, LuaTypeOwner,
    };

    use super::*;

    fn syntax_id(kind: LuaSyntaxKind, start: u32) -> LuaSyntaxId {
        LuaSyntaxId::new(
            kind.into(),
            TextRange::new(TextSize::new(start), TextSize::new(start + 1)),
        )
    }

    #[test]
    fn alias_global_members_to_decl_owner_only_aliases_meta_members() {
        let mut db = DbIndex::new();
        let decl_file = FileId::new(1);
        let decl = LuaDecl::new(
            "math",
            decl_file,
            TextRange::new(TextSize::new(0), TextSize::new(4)),
            LuaDeclExtra::Global {
                kind: LuaSyntaxKind::NameExpr.into(),
            },
            None,
        );
        let decl_id = decl.get_id();
        let mut decl_tree = LuaDeclarationTree::new(decl_file);
        decl_tree.add_decl(decl);
        db.get_decl_index_mut().add_decl_tree(decl_tree);
        db.get_global_index_mut().add_global_decl("math", decl_id);

        let math_type_id = LuaTypeDeclId::global("mathlib");
        db.get_type_index_mut().bind_type(
            LuaTypeOwner::Decl(decl_id),
            LuaTypeCache::DocType(LuaType::Ref(math_type_id.clone())),
        );

        let global_owner = LuaMemberOwner::GlobalPath(GlobalId::new("math"));
        let meta_member_id =
            LuaMemberId::new(syntax_id(LuaSyntaxKind::IndexExpr, 10), FileId::new(2));
        let file_member_id =
            LuaMemberId::new(syntax_id(LuaSyntaxKind::IndexExpr, 20), FileId::new(3));
        db.get_member_index_mut().add_member(
            global_owner.clone(),
            LuaMember::new(
                meta_member_id,
                LuaMemberKey::Name("Clamp".into()),
                LuaMemberFeature::MetaMethodDecl,
                Some(GlobalId::new("math.Clamp")),
            ),
        );
        db.get_member_index_mut().add_member(
            global_owner,
            LuaMember::new(
                file_member_id,
                LuaMemberKey::Name("AddonOnly".into()),
                LuaMemberFeature::FileMethodDecl,
                Some(GlobalId::new("math.AddonOnly")),
            ),
        );

        alias_global_members_to_decl_owner(&mut db, decl_id);

        let resolved_owner = LuaMemberOwner::Type(math_type_id);
        let member_index = db.get_member_index();
        assert!(
            member_index
                .get_member_item(&resolved_owner, &LuaMemberKey::Name("Clamp".into()))
                .is_some(),
            "meta global-path members should be visible on the resolved global owner"
        );
        assert!(
            member_index
                .get_member_item(&resolved_owner, &LuaMemberKey::Name("AddonOnly".into()))
                .is_none(),
            "non-meta global-path members should not be aliased by the late meta bridge"
        );
    }

    fn add_global_decl(db: &mut DbIndex, name: &str, file_id: FileId, start: u32) -> LuaDeclId {
        let decl = LuaDecl::new(
            name,
            file_id,
            TextRange::new(TextSize::new(start), TextSize::new(start + 1)),
            LuaDeclExtra::Global {
                kind: LuaSyntaxKind::NameExpr.into(),
            },
            None,
        );
        let decl_id = decl.get_id();
        let mut decl_tree = LuaDeclarationTree::new(file_id);
        decl_tree.add_decl(decl);
        db.get_decl_index_mut().add_decl_tree(decl_tree);
        db.get_global_index_mut().add_global_decl(name, decl_id);
        decl_id
    }

    /// Two files write `cityrp.type = cityrp.type or …` and each landed on a
    /// different `cityrp = cityrp or {}` literal, because the merged table the
    /// Lua pass elected from was still partial in the file analysed first.
    /// Neither writer can preserve the other while they sit under different
    /// owners, so both have to be aliased onto every literal.
    #[test]
    fn reconcile_aliases_guarded_assignment_members_onto_every_declared_table() {
        let mut db = DbIndex::new();
        let util_file = FileId::new(1);
        let shared_file = FileId::new(2);

        for (file_id, start) in [(util_file, 0), (shared_file, 0)] {
            let decl_id = add_global_decl(&mut db, "cityrp", file_id, start);
            db.get_decl_index_mut().set_global_initializer_table(
                decl_id,
                TextRange::new(TextSize::new(10), TextSize::new(12)),
            );
        }

        let owner_of = |file_id| {
            LuaMemberOwner::Element(InFiled::new(
                file_id,
                TextRange::new(TextSize::new(10), TextSize::new(12)),
            ))
        };

        // Each file's `cityrp.type` write, already homed on the literal its own
        // Lua pass elected.
        let mut writers = Vec::new();
        for file_id in [util_file, shared_file] {
            let member_id = LuaMemberId::new(syntax_id(LuaSyntaxKind::IndexExpr, 20), file_id);
            db.get_member_index_mut().add_member(
                owner_of(file_id),
                LuaMember::new(
                    member_id,
                    LuaMemberKey::Name("type".into()),
                    LuaMemberFeature::FileDefine,
                    None,
                ),
            );
            db.get_member_index_mut()
                .mark_non_overwriting_assignment_member(member_id);
            writers.push(member_id);
        }

        // Reconciliation is driven by the global still having something parked.
        db.get_member_index_mut().add_member(
            LuaMemberOwner::GlobalPath(GlobalId::new("cityrp")),
            LuaMember::new(
                LuaMemberId::new(syntax_id(LuaSyntaxKind::IndexExpr, 30), shared_file),
                LuaMemberKey::Name("LoadedOnce".into()),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        reconcile_parked_global_path_members(&mut db);

        let member_index = db.get_member_index();
        for file_id in [util_file, shared_file] {
            let item = member_index
                .get_member_item(&owner_of(file_id), &LuaMemberKey::Name("type".into()))
                .expect("expected a `type` item on every declared `cityrp` table");
            let stored = match item {
                crate::LuaMemberIndexItem::One(id) => vec![*id],
                crate::LuaMemberIndexItem::Many(ids) => ids.clone(),
            };
            assert!(
                writers.iter().all(|writer| stored.contains(writer)),
                "both guarded `cityrp.type` writers should be visible through {file_id:?}, got {item:?}"
            );
        }
    }

    /// A member homed on a *sibling* file's table belongs to the table its own
    /// file declares. Which one it reached during analysis depends on how much
    /// of the path had resolved at that moment, so the final index has to decide
    /// it instead.
    #[test]
    fn reconcile_rehomes_a_member_onto_its_own_files_table() {
        let mut db = DbIndex::new();
        let own_file = FileId::new(1);
        let sibling_file = FileId::new(2);

        for file_id in [own_file, sibling_file] {
            let decl_id = add_global_decl(&mut db, "cityrp", file_id, 0);
            db.get_decl_index_mut().set_global_initializer_table(
                decl_id,
                TextRange::new(TextSize::new(10), TextSize::new(12)),
            );
        }

        let owner_of = |file_id| {
            LuaMemberOwner::Element(InFiled::new(
                file_id,
                TextRange::new(TextSize::new(10), TextSize::new(12)),
            ))
        };

        // `own_file`'s field, placed on `sibling_file`'s literal because that is
        // what the prefix resolved to when the deferred write was handled.
        let member_id = LuaMemberId::new(syntax_id(LuaSyntaxKind::IndexExpr, 20), own_file);
        db.get_member_index_mut().add_member(
            owner_of(sibling_file),
            LuaMember::new(
                member_id,
                LuaMemberKey::Name("menu".into()),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        // Reconciliation is driven by the global still having something parked.
        db.get_member_index_mut().add_member(
            LuaMemberOwner::GlobalPath(GlobalId::new("cityrp")),
            LuaMember::new(
                LuaMemberId::new(syntax_id(LuaSyntaxKind::IndexExpr, 30), sibling_file),
                LuaMemberKey::Name("LoadedOnce".into()),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        reconcile_parked_global_path_members(&mut db);

        assert_eq!(
            db.get_member_index().get_member_owner(&member_id),
            Some(&owner_of(own_file)),
            "a member of a declaring file belongs to that file's own table"
        );
        assert!(
            db.get_member_index()
                .get_members(&owner_of(sibling_file))
                .is_some_and(|members| members.iter().any(|member| member.get_id() == member_id)),
            "a member that reached the sibling table from a settled fact stays \
             enumerable through it"
        );
    }

    /// The move has to take enumerability with it when the first placement was
    /// provisional: leaving the member listed under the sibling table is a fact
    /// only the analysis order that mis-placed it ever produced.
    #[test]
    fn reconcile_detaches_a_deferred_member_from_the_table_it_left() {
        let mut db = DbIndex::new();
        let own_file = FileId::new(1);
        let sibling_file = FileId::new(2);

        for file_id in [own_file, sibling_file] {
            let decl_id = add_global_decl(&mut db, "cityrp", file_id, 0);
            db.get_decl_index_mut().set_global_initializer_table(
                decl_id,
                TextRange::new(TextSize::new(10), TextSize::new(12)),
            );
        }

        let owner_of = |file_id| {
            LuaMemberOwner::Element(InFiled::new(
                file_id,
                TextRange::new(TextSize::new(10), TextSize::new(12)),
            ))
        };

        let member_id = LuaMemberId::new(syntax_id(LuaSyntaxKind::IndexExpr, 20), own_file);
        db.get_member_index_mut().add_member(
            owner_of(sibling_file),
            LuaMember::new(
                member_id,
                LuaMemberKey::Name("menu".into()),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );
        db.get_member_index_mut()
            .mark_deferred_index_expr_member(member_id);

        db.get_member_index_mut().add_member(
            LuaMemberOwner::GlobalPath(GlobalId::new("cityrp")),
            LuaMember::new(
                LuaMemberId::new(syntax_id(LuaSyntaxKind::IndexExpr, 30), sibling_file),
                LuaMemberKey::Name("LoadedOnce".into()),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        reconcile_parked_global_path_members(&mut db);

        assert_eq!(
            db.get_member_index().get_member_owner(&member_id),
            Some(&owner_of(own_file)),
            "a member of a declaring file belongs to that file's own table"
        );
        assert!(
            db.get_member_index()
                .get_members(&owner_of(sibling_file))
                .is_none_or(|members| members.iter().all(|member| member.get_id() != member_id)),
            "a provisionally placed member is not enumerable through the table \
             it was moved off"
        );
    }

    /// A declaring file whose own table has not resolved keeps its members
    /// parked — but reconciliation is the batch's last pass, so parking may not
    /// also cost the member its reachability through the elected table.
    #[test]
    fn reconcile_aliases_parked_members_of_an_unresolved_declaring_file() {
        let mut db = DbIndex::new();
        let resolved_file = FileId::new(1);
        let unresolved_file = FileId::new(2);

        let resolved_decl_id = add_global_decl(&mut db, "cityrp", resolved_file, 0);
        add_global_decl(&mut db, "cityrp", unresolved_file, 0);

        let table_type_id = LuaTypeDeclId::global("cityrptable");
        db.get_type_index_mut().bind_type(
            LuaTypeOwner::Decl(resolved_decl_id),
            LuaTypeCache::DocType(LuaType::Ref(table_type_id.clone())),
        );

        let member_id = LuaMemberId::new(syntax_id(LuaSyntaxKind::IndexExpr, 10), unresolved_file);
        db.get_member_index_mut().add_member(
            LuaMemberOwner::GlobalPath(GlobalId::new("cityrp")),
            LuaMember::new(
                member_id,
                LuaMemberKey::Name("menu".into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );

        reconcile_parked_global_path_members(&mut db);

        let member_index = db.get_member_index();
        assert!(
            member_index
                .get_member_item(
                    &LuaMemberOwner::Type(table_type_id),
                    &LuaMemberKey::Name("menu".into())
                )
                .is_some(),
            "a parked member must stay reachable through the elected owner"
        );
        assert_eq!(
            member_index.get_member_owner(&member_id),
            Some(&LuaMemberOwner::GlobalPath(GlobalId::new("cityrp"))),
            "aliasing must not re-home the member onto a sibling file's table"
        );
    }
}
