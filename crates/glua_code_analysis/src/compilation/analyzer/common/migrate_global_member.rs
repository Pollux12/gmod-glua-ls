use crate::{DbIndex, GlobalId, InFiled, LuaDeclId, LuaMemberId, LuaMemberOwner, LuaTypeOwner};

use super::get_owner_id;

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
        let members = {
            let member_index = db.get_member_index();
            match member_index.get_members(&global_path_owner) {
                Some(members) => members
                    .iter()
                    .map(|member| {
                        let member_id = member.get_id();
                        let needs_rehome = member_index
                            .get_member_owner(&member_id)
                            .is_none_or(|owner| *owner == global_path_owner);
                        (member_id, needs_rehome)
                    })
                    .collect::<Vec<_>>(),
                None => continue,
            }
        };
        if members.is_empty() {
            continue;
        }

        for (member_id, needs_rehome) in members {
            // A file that declares the global itself owns the members it
            // contributes: `marauth = marauth or {}` in two files describes one
            // runtime table, but each file's fields belong to the table literal
            // that file wrote. Falling back to the elected owner covers files
            // that only extend a global they never declare.
            let target_owner = candidates
                .iter()
                .find(|(file_id, _)| *file_id == member_id.file_id)
                .map(|(_, owner)| owner)
                .unwrap_or(canonical_owner)
                .clone();

            let member_index = db.get_member_index_mut();
            if needs_rehome
                && member_index
                    .get_member_owner(&member_id)
                    .is_none_or(|owner| *owner != target_owner)
            {
                member_index.set_member_owner(target_owner.clone(), member_id.file_id, member_id);
                member_index.add_member_to_owner(target_owner.clone(), member_id);
            }
            // Aliasing the remaining candidates is what makes a global
            // declared once per realm behave like the single table it is at
            // runtime, and it has to run for members that already reached a
            // concrete owner too. Re-indexing a file rebuilds its members
            // from scratch, so the aliases the original migration created
            // are gone; gating the repair behind `needs_rehome` meant they
            // were only ever rebuilt for members still parked on the global
            // path.
            for (_, alias_owner) in &candidates {
                if *alias_owner != target_owner {
                    member_index.add_member_alias_to_owner(alias_owner.clone(), member_id);
                }
            }
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
            let declaring_member_ids = db
                .get_member_index()
                .get_members(&LuaMemberOwner::GlobalPath(parent_id))
                .map(|members| {
                    members
                        .iter()
                        .filter(|member| member.get_global_id() == Some(global_id))
                        .map(|member| member.get_id())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

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
    let triggering_owner = get_owner_id(db, &triggering_decl_id.into())?;

    // No shortcut for the single-declaration case. It used to return the
    // *triggering* declaration's owner without ranking, which is only the
    // same answer when the global index already knows every sibling — and
    // during a partial re-index it does not, so the shortcut could hand a
    // global to whichever declaration happened to fire while the index was
    // still being rebuilt.
    let _ = triggering_owner;

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
        .filter(|member_id| !member_index.has_synthesized_owner(member_id))
        .collect::<Vec<_>>();

    for member_id in members {
        // Same rule the end-of-batch reconciliation uses: a file that
        // declares the global owns the fields it writes, and only files
        // that merely extend a global they never declare fall back to the
        // elected owner.
        let target_owner = owners
            .iter()
            .find(|(file_id, _)| *file_id == member_id.file_id)
            .map(|(_, owner)| owner)
            .unwrap_or(canonical_owner)
            .clone();

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

    let declaring_member_ids = db
        .get_member_index()
        .get_members(&LuaMemberOwner::GlobalPath(parent_id))
        .map(|members| {
            members
                .iter()
                .filter(|member| member.get_global_id() == Some(global_id))
                .map(|member| member.get_id())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // See `resolved_global_decl_owners`: the resolution of `member_id` is the
    // event being handled, so it must have produced an owner for this call to
    // carry information.
    let triggering_owner = get_owner_id(db, &member_id.into())?;
    if declaring_member_ids.len() <= 1 {
        return Some(vec![(member_id.file_id, triggering_owner)]);
    }

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
}
