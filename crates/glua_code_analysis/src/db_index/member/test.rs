#[cfg(test)]
mod tests {
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    use crate::db_index::member::*;
    use crate::{FileId, LuaTypeDeclId};

    fn make_member(member_id: LuaMemberId, key: &str) -> LuaMember {
        make_member_with_feature(member_id, key, LuaMemberFeature::FileFieldDecl)
    }

    fn make_member_with_feature(
        member_id: LuaMemberId,
        key: &str,
        feature: LuaMemberFeature,
    ) -> LuaMember {
        LuaMember::new(member_id, LuaMemberKey::Name(key.into()), feature, None)
    }

    fn make_member_id(file_id: FileId, start: u32) -> LuaMemberId {
        let range = TextRange::new(TextSize::new(start), TextSize::new(start + 1));
        LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::NameExpr.into(), range),
            file_id,
        )
    }

    fn make_index_member_id(file_id: FileId, start: u32) -> LuaMemberId {
        let range = TextRange::new(TextSize::new(start), TextSize::new(start + 1));
        LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range),
            file_id,
        )
    }

    fn make_member_id_with_kind_and_end(
        file_id: FileId,
        kind: LuaSyntaxKind,
        start: u32,
        end: u32,
    ) -> LuaMemberId {
        LuaMemberId::new(
            LuaSyntaxId::new(
                kind.into(),
                TextRange::new(TextSize::new(start), TextSize::new(end)),
            ),
            file_id,
        )
    }

    fn owner_member_ids(index: &LuaMemberIndex, owner: &LuaMemberOwner) -> Vec<LuaMemberId> {
        index
            .get_members(owner)
            .expect("owner should exist")
            .into_iter()
            .map(|member| member.get_id())
            .collect()
    }

    fn site(file: u32, start: u32, end: u32) -> crate::InFiled<TextRange> {
        crate::InFiled::new(
            FileId::new(file),
            TextRange::new(TextSize::new(start), TextSize::new(end)),
        )
    }

    fn path(name: &str) -> LuaMemberOwner {
        LuaMemberOwner::GlobalPath(GlobalId::new(name))
    }

    #[test]
    fn an_inline_literal_field_is_owned_by_the_path_not_the_literal() {
        let mut index = LuaMemberIndex::new();
        let site = site(1, 10, 12);
        index.set_definition_site(GlobalId::new("cityrp.util"), site.clone());
        let member_id = make_index_member_id(FileId::new(1), 11);
        index.add_member(
            LuaMemberOwner::Element(site.clone()),
            make_member(member_id, "Bind"),
        );

        assert_eq!(
            index.get_member_owner(&member_id),
            Some(&path("cityrp.util"))
        );
        assert_eq!(
            owner_member_ids(&index, &LuaMemberOwner::Element(site)),
            owner_member_ids(&index, &path("cityrp.util"))
        );
    }

    #[test]
    fn a_literal_that_is_not_a_definition_site_keeps_its_element_owner() {
        let mut index = LuaMemberIndex::new();
        let anonymous = site(1, 40, 42);
        let member_id = make_index_member_id(FileId::new(1), 41);
        index.add_member(
            LuaMemberOwner::Element(anonymous.clone()),
            make_member(member_id, "Bind"),
        );

        assert_eq!(
            index.get_member_owner(&member_id),
            Some(&LuaMemberOwner::Element(anonymous))
        );
    }

    #[test]
    fn three_literals_in_three_files_share_one_owner() {
        let mut index = LuaMemberIndex::new();
        for file in 1..=3u32 {
            index.set_definition_site(GlobalId::new("cityrp.util"), site(file, 10, 12));
            index.add_member(
                LuaMemberOwner::Element(site(file, 10, 12)),
                make_member(make_index_member_id(FileId::new(file), 20 + file), "Bind"),
            );
        }

        assert_eq!(owner_member_ids(&index, &path("cityrp.util")).len(), 3);
        assert_eq!(
            index.definition_sites(&GlobalId::new("cityrp.util")).len(),
            3
        );
        assert_eq!(
            index
                .definition_sites(&GlobalId::new("cityrp.util"))
                .iter()
                .map(|s| s.file_id.id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn removing_one_definition_site_file_keeps_the_other_files_members() {
        let mut index = LuaMemberIndex::new();
        for file in 1..=3u32 {
            index.set_definition_site(GlobalId::new("cityrp.util"), site(file, 10, 12));
            index.add_member(
                LuaMemberOwner::Element(site(file, 10, 12)),
                make_member(make_index_member_id(FileId::new(file), 20 + file), "Bind"),
            );
        }
        index.remove_files(&[FileId::new(2)]);

        assert_eq!(
            index.definition_sites(&GlobalId::new("cityrp.util")).len(),
            2
        );
        for file in [1u32, 3] {
            let member_id = make_index_member_id(FileId::new(file), 20 + file);
            assert_eq!(
                index.get_member_owner(&member_id),
                Some(&path("cityrp.util"))
            );
        }
    }

    #[test]
    fn insertion_order_does_not_change_owners() {
        let owners_for = |order: [u32; 3]| {
            let mut index = LuaMemberIndex::new();
            for file in order {
                index.set_definition_site(GlobalId::new("cityrp.util"), site(file, 10, 12));
                index.add_member(
                    LuaMemberOwner::Element(site(file, 10, 12)),
                    make_member(make_index_member_id(FileId::new(file), 20 + file), "Bind"),
                );
            }
            let mut owners = (1..=3u32)
                .map(|file| {
                    let member_id = make_index_member_id(FileId::new(file), 20 + file);
                    format!("{:?}", index.get_member_owner(&member_id))
                })
                .collect::<Vec<_>>();
            owners.sort();
            let sites = index
                .definition_sites(&GlobalId::new("cityrp.util"))
                .iter()
                .map(|s| s.file_id.id)
                .collect::<Vec<_>>();
            (owners, sites)
        };

        let baseline = owners_for([1, 2, 3]);
        for order in [[3, 2, 1], [2, 1, 3], [2, 3, 1], [3, 1, 2], [1, 3, 2]] {
            assert_eq!(
                owners_for(order),
                baseline,
                "order {order:?} changed ownership"
            );
        }
    }

    #[test]
    fn get_members_multi_key_owner_matches_member_id_sort_order() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let member_specs = [
            (
                make_member_id_with_kind_and_end(FileId::new(3), LuaSyntaxKind::NameExpr, 20, 24),
                "gamma",
            ),
            (
                make_member_id_with_kind_and_end(FileId::new(1), LuaSyntaxKind::IndexExpr, 40, 45),
                "alpha",
            ),
            (
                make_member_id_with_kind_and_end(FileId::new(1), LuaSyntaxKind::NameExpr, 10, 12),
                "beta",
            ),
            (
                make_member_id_with_kind_and_end(FileId::new(2), LuaSyntaxKind::NameExpr, 5, 6),
                "delta",
            ),
            (
                make_member_id_with_kind_and_end(FileId::new(1), LuaSyntaxKind::IndexExpr, 10, 11),
                "epsilon",
            ),
        ];

        let mut index = LuaMemberIndex::new();
        for (member_id, key) in member_specs {
            index.add_member(owner.clone(), make_member(member_id, key));
        }

        let mut expected_ids = member_specs.map(|(member_id, _)| member_id).to_vec();
        expected_ids.sort_by_key(|member_id| member_id_sort_key(*member_id));

        assert_eq!(owner_member_ids(&index, &owner), expected_ids);
    }

    #[test]
    fn batch_removal_matches_sequential_removal_for_surviving_members() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("BatchRemoveType"));
        let first = FileId::new(1);
        let second = FileId::new(2);
        let survivor = FileId::new(3);

        let populate = || {
            let mut index = LuaMemberIndex::new();
            index.add_member(
                owner.clone(),
                make_member(make_member_id(first, 10), "alpha"),
            );
            index.add_member(
                owner.clone(),
                make_member(make_member_id(second, 20), "alpha"),
            );
            index.add_member(
                owner.clone(),
                make_member(make_member_id(second, 30), "beta"),
            );
            index.add_member(
                owner.clone(),
                make_member(make_member_id(survivor, 40), "alpha"),
            );
            index.add_member(
                owner.clone(),
                make_member(make_member_id(survivor, 50), "gamma"),
            );
            index
        };

        let mut sequential = populate();
        sequential.remove(first);
        sequential.remove(second);

        let mut batched = populate();
        batched.remove_files(&[second, first, second]);

        assert_eq!(
            owner_member_ids(&sequential, &owner),
            owner_member_ids(&batched, &owner)
        );
        assert_eq!(
            owner_member_ids(&batched, &owner),
            vec![make_member_id(survivor, 40), make_member_id(survivor, 50)]
        );
        for index in [&sequential, &batched] {
            assert!(index.get_file_members(first).is_empty());
            assert!(index.get_file_members(second).is_empty());
            assert!(index.get_member(&make_member_id(first, 10)).is_none());
            assert!(index.get_member(&make_member_id(second, 20)).is_none());
            assert!(index.get_member(&make_member_id(survivor, 40)).is_some());
        }
        assert_eq!(
            sequential.member_owner_key_index,
            batched.member_owner_key_index
        );
        assert_eq!(
            sequential.member_owner_key_history_index,
            batched.member_owner_key_history_index
        );
    }

    #[test]
    fn get_members_cache_invalidates_when_adding_earlier_member_after_warm() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let first_member_id = make_member_id(FileId::new(4), 20);
        let second_member_id = make_member_id(FileId::new(5), 30);
        let earlier_member_id = make_member_id(FileId::new(1), 5);
        let mut index = LuaMemberIndex::new();

        index.add_member(owner.clone(), make_member(first_member_id, "first"));
        index.add_member(owner.clone(), make_member(second_member_id, "second"));

        assert_eq!(
            owner_member_ids(&index, &owner),
            vec![first_member_id, second_member_id]
        );

        index.add_member(owner.clone(), make_member(earlier_member_id, "third"));

        assert_eq!(
            owner_member_ids(&index, &owner),
            vec![earlier_member_id, first_member_id, second_member_id]
        );
    }

    #[test]
    fn set_member_owner_moves_member_between_owner_indexes() {
        let file_id = FileId::new(1);
        let old_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OldOwner"));
        let new_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("NewOwner"));
        let key = LuaMemberKey::Name("field".into());
        let member_id = make_member_id(file_id, 1);

        let mut index = LuaMemberIndex::new();
        index.add_member(old_owner.clone(), make_member(member_id, "field"));
        assert!(index.get_member_item(&old_owner, &key).is_some());

        index
            .set_member_owner(new_owner.clone(), file_id, member_id)
            .expect("owner reassignment should succeed");

        assert!(index.get_member_item(&old_owner, &key).is_some());
        assert!(index.get_member_item(&new_owner, &key).is_none());
        assert!(index.get_members_for_owner_key(&old_owner, &key).is_empty());
        assert_eq!(index.get_members_for_owner_key(&new_owner, &key).len(), 1);
        assert!(
            index
                .get_current_owner_member_history(&old_owner)
                .is_empty()
        );
        assert_eq!(
            index
                .get_current_owner_member_history(&new_owner)
                .iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![member_id]
        );
    }

    #[test]
    fn set_member_owner_keeps_other_old_owner_members() {
        let file_id = FileId::new(2);
        let old_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OriginalOwner"));
        let new_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("ReassignedOwner"));
        let key = LuaMemberKey::Name("field".into());
        let first_member_id = make_member_id(file_id, 1);
        let second_member_id = make_member_id(file_id, 3);

        let mut index = LuaMemberIndex::new();
        index.add_member(old_owner.clone(), make_member(first_member_id, "field"));
        index.add_member(old_owner.clone(), make_member(second_member_id, "field"));

        index
            .set_member_owner(new_owner.clone(), file_id, first_member_id)
            .expect("owner reassignment should succeed");

        assert_eq!(index.get_members_for_owner_key(&old_owner, &key).len(), 1);
        assert_eq!(index.get_members_for_owner_key(&new_owner, &key).len(), 1);

        let old_owner_member_ids = index
            .get_members_for_owner_key(&old_owner, &key)
            .iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(old_owner_member_ids, vec![second_member_id]);

        let new_owner_member_ids = index
            .get_members_for_owner_key(&new_owner, &key)
            .iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(new_owner_member_ids, vec![first_member_id]);
        assert_eq!(
            index
                .get_current_owner_member_history(&old_owner)
                .iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![second_member_id]
        );
        assert_eq!(
            index
                .get_current_owner_member_history(&new_owner)
                .iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![first_member_id]
        );

        let new_owner_history_member_ids = index
            .get_current_owner_members_for_key(&new_owner, &key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(new_owner_history_member_ids, vec![first_member_id]);

        let key_history_member_ids = index
            .get_current_members_for_key(&key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(
            key_history_member_ids,
            vec![first_member_id, second_member_id]
        );
    }

    #[test]
    fn get_members_cache_invalidates_when_removing_file_after_warm() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let removed_member_id = make_member_id(FileId::new(4), 10);
        let retained_member_id = make_member_id(FileId::new(5), 5);
        let mut index = LuaMemberIndex::new();

        index.add_member(owner.clone(), make_member(removed_member_id, "removed"));
        index.add_member(owner.clone(), make_member(retained_member_id, "retained"));

        assert_eq!(
            owner_member_ids(&index, &owner),
            vec![removed_member_id, retained_member_id]
        );

        index.remove(FileId::new(4));

        assert_eq!(owner_member_ids(&index, &owner), vec![retained_member_id]);
    }

    #[test]
    fn warming_get_members_does_not_break_owner_move_visibility() {
        let file_id = FileId::new(6);
        let old_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OldOwner"));
        let new_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("NewOwner"));
        let key = LuaMemberKey::Name("field".into());
        let member_id = make_member_id(file_id, 1);
        let mut index = LuaMemberIndex::new();

        index.add_member(old_owner.clone(), make_member(member_id, "field"));
        assert_eq!(owner_member_ids(&index, &old_owner), vec![member_id]);
        assert!(index.get_members(&new_owner).is_none());

        index
            .set_member_owner(new_owner.clone(), file_id, member_id)
            .expect("owner reassignment should succeed");

        assert!(index.get_members_for_owner_key(&old_owner, &key).is_empty());
        assert_eq!(
            index
                .get_members_for_owner_key(&new_owner, &key)
                .into_iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![member_id]
        );
    }

    #[test]
    fn get_members_cache_invalidates_when_one_promotes_to_many_after_warm() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let first_member_id = make_member_id(FileId::new(4), 20);
        let second_member_id = make_member_id(FileId::new(4), 10);
        let mut index = LuaMemberIndex::new();

        index.add_member(
            owner.clone(),
            LuaMember::new(
                first_member_id,
                key.clone(),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        assert_eq!(owner_member_ids(&index, &owner), vec![first_member_id]);

        index.add_member(
            owner.clone(),
            LuaMember::new(second_member_id, key, LuaMemberFeature::FileFieldDecl, None),
        );

        assert_eq!(
            owner_member_ids(&index, &owner),
            vec![second_member_id, first_member_id]
        );
    }

    #[test]
    fn clear_resets_member_owner_tracking() {
        let file_id = FileId::new(3);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let member_id = make_member_id(file_id, 7);

        let mut index = LuaMemberIndex::new();
        index.add_member(owner, make_member(member_id, "field"));
        assert!(index.get_member_owner(&member_id).is_some());
        assert!(
            !index
                .get_current_members_for_key(&LuaMemberKey::Name("field".into()))
                .is_empty()
        );

        index.clear();

        assert!(index.get_member_owner(&member_id).is_none());
        assert!(
            index
                .get_current_members_for_key(&LuaMemberKey::Name("field".into()))
                .is_empty()
        );
    }

    #[test]
    fn key_lookup_cache_invalidates_after_member_mutation() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let first_member_id = make_member_id(FileId::new(9), 1);
        let second_member_id = make_member_id(FileId::new(10), 3);
        let mut index = LuaMemberIndex::new();

        index.add_member(owner.clone(), make_member(first_member_id, "field"));
        assert_eq!(
            index
                .get_current_members_for_key(&key)
                .into_iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![first_member_id]
        );

        index.add_member(owner.clone(), make_member(second_member_id, "field"));
        assert_eq!(
            index
                .get_current_members_for_key(&key)
                .into_iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![first_member_id, second_member_id]
        );

        index.remove(FileId::new(9));
        assert_eq!(
            index
                .get_current_members_for_key(&key)
                .into_iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![second_member_id]
        );
    }

    #[test]
    fn a_declaration_and_a_write_coexist_whichever_reaches_the_slot_first() {
        let owner = path("holdem");
        let key = LuaMemberKey::Name("action".into());
        // `function holdem.action(...)` in the server file.
        let declared = make_index_member_id(FileId::new(6), 30);
        // `holdem.action = { ... }` in the client file.
        let written = make_index_member_id(FileId::new(5), 20);

        let item_for = |first, second| {
            let mut index = LuaMemberIndex::new();
            for (member_id, feature) in [first, second] {
                index.add_member(
                    owner.clone(),
                    LuaMember::new(member_id, key.clone(), feature, None),
                );
            }
            index.get_member_item(&owner, &key).cloned()
        };

        let both = Some(LuaMemberIndexItem::Many(vec![written, declared]));
        assert_eq!(
            item_for(
                (written, LuaMemberFeature::FileDefine),
                (declared, LuaMemberFeature::FileMethodDecl),
            ),
            both
        );
        assert_eq!(
            item_for(
                (declared, LuaMemberFeature::FileMethodDecl),
                (written, LuaMemberFeature::FileDefine),
            ),
            both
        );
    }

    #[test]
    fn visible_owner_key_other_member_check_uses_current_visible_members() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let first_member_id = make_index_member_id(FileId::new(4), 1);
        let second_member_id = make_index_member_id(FileId::new(4), 3);

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            LuaMember::new(
                first_member_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        assert!(!index.has_visible_member_for_owner_key_other_than(&owner, &key, first_member_id));

        index.add_member(
            owner.clone(),
            LuaMember::new(
                second_member_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        assert!(index.has_visible_member_for_owner_key_other_than(&owner, &key, second_member_id));
    }

    #[test]
    fn meta_only_member_item_preserves_meta_when_assignment_is_added() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let meta_member_id = make_member_id(FileId::new(4), 1);
        let assignment_member_id = make_index_member_id(FileId::new(4), 3);

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            make_member_with_feature(meta_member_id, "field", LuaMemberFeature::MetaFieldDecl),
        );
        index.add_member(
            owner.clone(),
            make_member_with_feature(assignment_member_id, "field", LuaMemberFeature::FileDefine),
        );

        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::Many(vec![
                meta_member_id,
                assignment_member_id,
            ]))
        );
    }

    #[test]
    fn preserved_assignment_duplicate_insertions_keep_existing_item_order() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let first_member_id = make_index_member_id(FileId::new(4), 1);
        let second_member_id = make_index_member_id(FileId::new(4), 3);

        let mut index = LuaMemberIndex::new();
        for member_id in [first_member_id, second_member_id] {
            index.mark_non_overwriting_assignment_member(member_id);
            index.add_member(
                owner.clone(),
                make_member_with_feature(member_id, "field", LuaMemberFeature::FileDefine),
            );
        }

        index.merge_member_into_owner_item(owner.clone(), key.clone(), second_member_id);
        index.merge_member_into_owner_item(owner.clone(), key.clone(), first_member_id);

        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::Many(vec![
                first_member_id,
                second_member_id,
            ]))
        );
    }

    #[test]
    fn alias_adds_to_an_existing_file_define_without_displacing_it() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("SharedTable"));
        let other_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OtherTable"));
        let key = LuaMemberKey::Name("field".into());
        let own_member_id = make_index_member_id(FileId::new(1), 10);
        let aliased_member_id = make_index_member_id(FileId::new(2), 20);

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            make_member_with_feature(own_member_id, "field", LuaMemberFeature::FileDefine),
        );
        index.add_member(
            other_owner,
            make_member_with_feature(aliased_member_id, "field", LuaMemberFeature::FileDefine),
        );

        index.add_member_alias_to_owner(owner.clone(), aliased_member_id);

        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::Many(vec![
                own_member_id,
                aliased_member_id,
            ])),
            "an alias only ever adds; it must never replace the owner's own writer"
        );
    }

    #[test]
    fn a_table_field_write_arriving_after_an_alias_still_takes_the_slot() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("SharedTable"));
        let other_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OtherTable"));
        let key = LuaMemberKey::Name("field".into());
        // A table-literal field is a `FileDefine` that is not an index-expr
        // assignment, so it falls through to the latest-defined rule.
        let own_member_id = make_member_id(FileId::new(1), 10);
        let aliased_member_id = make_index_member_id(FileId::new(2), 20);

        let mut index = LuaMemberIndex::new();
        index.add_member(
            other_owner,
            make_member_with_feature(aliased_member_id, "field", LuaMemberFeature::FileDefine),
        );
        index.add_member_alias_to_owner(owner.clone(), aliased_member_id);
        index.add_member(
            owner.clone(),
            make_member_with_feature(own_member_id, "field", LuaMemberFeature::FileDefine),
        );

        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::Many(vec![
                own_member_id,
                aliased_member_id,
            ])),
            "which of the two arrived first must not decide the slot"
        );
    }

    #[test]
    fn marked_non_overwriting_file_defines_share_lookup_item() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let first_member_id = LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::IndexExpr.into(),
                TextRange::new(TextSize::new(1), TextSize::new(2)),
            ),
            FileId::new(4),
        );
        let second_member_id = LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::IndexExpr.into(),
                TextRange::new(TextSize::new(3), TextSize::new(4)),
            ),
            FileId::new(4),
        );

        let mut index = LuaMemberIndex::new();
        index.mark_non_overwriting_assignment_member(first_member_id);
        index.add_member(
            owner.clone(),
            LuaMember::new(
                first_member_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );
        index.mark_non_overwriting_assignment_member(second_member_id);
        index.add_member(
            owner.clone(),
            LuaMember::new(
                second_member_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::Many(vec![
                first_member_id,
                second_member_id
            ]))
        );
    }

    #[test]
    fn preserved_assignment_insertion_invalidates_get_members_cache_after_warm() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let later_member_id = make_index_member_id(FileId::new(4), 30);
        let earlier_member_id = make_index_member_id(FileId::new(4), 10);
        let mut index = LuaMemberIndex::new();

        index.mark_non_overwriting_assignment_member(later_member_id);
        index.add_member(
            owner.clone(),
            LuaMember::new(
                later_member_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );
        assert_eq!(owner_member_ids(&index, &owner), vec![later_member_id]);

        index.mark_non_overwriting_assignment_member(earlier_member_id);
        index.add_member(
            owner.clone(),
            LuaMember::new(earlier_member_id, key, LuaMemberFeature::FileDefine, None),
        );

        assert_eq!(
            owner_member_ids(&index, &owner),
            vec![earlier_member_id, later_member_id]
        );
    }

    #[test]
    fn many_marked_non_overwriting_file_defines_share_lookup_item() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let mut index = LuaMemberIndex::new();

        for i in 0..128 {
            let member_id = LuaMemberId::new(
                LuaSyntaxId::new(
                    LuaSyntaxKind::IndexExpr.into(),
                    TextRange::new(TextSize::new(i), TextSize::new(i + 1)),
                ),
                FileId::new(4),
            );
            index.mark_non_overwriting_assignment_member(member_id);
            index.add_member(
                owner.clone(),
                LuaMember::new(member_id, key.clone(), LuaMemberFeature::FileDefine, None),
            );
        }

        let Some(LuaMemberIndexItem::Many(member_ids)) = index.get_member_item(&owner, &key) else {
            panic!("marked assignments should be preserved as a shared lookup item");
        };

        assert_eq!(member_ids.len(), 128);
        assert_eq!(
            member_ids.first().copied(),
            Some(make_index_member_id(FileId::new(4), 0))
        );
        assert_eq!(
            member_ids.last().copied(),
            Some(make_index_member_id(FileId::new(4), 127))
        );
    }

    #[test]
    fn marked_non_overwriting_file_defines_keep_stable_order_when_added_out_of_order() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let mut index = LuaMemberIndex::new();

        for start in [30, 10, 20] {
            let member_id = make_index_member_id(FileId::new(4), start);
            index.mark_non_overwriting_assignment_member(member_id);
            index.add_member(
                owner.clone(),
                LuaMember::new(member_id, key.clone(), LuaMemberFeature::FileDefine, None),
            );
        }

        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::Many(vec![
                make_index_member_id(FileId::new(4), 10),
                make_index_member_id(FileId::new(4), 20),
                make_index_member_id(FileId::new(4), 30),
            ]))
        );
    }

    /// Transparency only applies when a real writer exists. With nothing but
    /// bootstraps across files there is no placeholder to see through, so the
    /// existing merge still has to keep every writer visible.
    #[test]
    fn cross_file_all_bootstrap_writers_still_merge() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("stock".into());
        let first_id = make_index_member_id(FileId::new(1), 10);
        let second_id = make_index_member_id(FileId::new(2), 20);

        let mut index = LuaMemberIndex::new();
        for member_id in [first_id, second_id] {
            index.mark_non_overwriting_assignment_member(member_id);
            index.add_member(
                owner.clone(),
                LuaMember::new(member_id, key.clone(), LuaMemberFeature::FileDefine, None),
            );
        }

        assert_eq!(owner_member_ids(&index, &owner), vec![first_id, second_id]);
    }

    #[test]
    fn file_removal_clears_previous_owner_history_entries() {
        let file_id = FileId::new(6);
        let old_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OldOwner"));
        let new_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("NewOwner"));
        let old_key = LuaMemberKey::Name("old_field".into());
        let new_key = LuaMemberKey::Name("new_field".into());
        let member_id = make_member_id(file_id, 10);
        let mut index = LuaMemberIndex::new();

        index.add_member(old_owner.clone(), make_member(member_id, "old_field"));
        index
            .set_member_owner(new_owner, file_id, member_id)
            .expect("owner reassignment should succeed");
        index.remove(file_id);
        index.add_member(old_owner.clone(), make_member(member_id, "new_field"));

        assert!(
            index
                .get_current_owner_members_for_key(&old_owner, &old_key)
                .is_empty()
        );
        assert!(index.get_current_members_for_key(&old_key).is_empty());
        assert_eq!(
            index
                .get_current_owner_members_for_key(&old_owner, &new_key)
                .into_iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![member_id]
        );
        assert_eq!(
            index
                .get_current_members_for_key(&new_key)
                .into_iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![member_id]
        );
    }

    #[test]
    fn function_scope_lookup_returns_innermost_range() {
        let file_id = FileId::new(7);
        let outer = TextRange::new(TextSize::new(10), TextSize::new(100));
        let inner = TextRange::new(TextSize::new(30), TextSize::new(60));
        let mut index = LuaMemberIndex::new();

        index.add_function_scope_range(file_id, outer);
        index.add_function_scope_range(file_id, inner);

        assert_eq!(
            index.enclosing_function_scope_range(file_id, TextSize::new(40)),
            Some(inner)
        );
        assert_eq!(
            index.enclosing_function_scope_range(file_id, TextSize::new(80)),
            Some(outer)
        );
        assert_eq!(
            index.enclosing_function_scope_range(file_id, TextSize::new(5)),
            None
        );
    }

    #[test]
    fn file_removal_clears_function_scope_metadata() {
        let file_id = FileId::new(8);
        let range = TextRange::new(TextSize::new(10), TextSize::new(100));
        let member_id = make_member_id(file_id, 20);
        let mut index = LuaMemberIndex::new();

        index.add_function_scope_range(file_id, range);
        index.set_member_function_scope_range(member_id, Some(range));
        assert_eq!(
            index.enclosing_function_scope_range(file_id, TextSize::new(20)),
            Some(range)
        );
        assert_eq!(index.member_function_scope_range(member_id), Some(range));

        index.remove(file_id);

        assert_eq!(
            index.enclosing_function_scope_range(file_id, TextSize::new(20)),
            None
        );
        assert_eq!(index.member_function_scope_range(member_id), None);

        index.add_function_scope_range(file_id, range);
        index.set_member_function_scope_range(member_id, Some(range));
        index.clear();

        assert_eq!(
            index.enclosing_function_scope_range(file_id, TextSize::new(20)),
            None
        );
        assert_eq!(index.member_function_scope_range(member_id), None);
    }

    /// A reader of a member object records the object alone, read out of the
    /// member: the member's function scope moving holds it, a rekey moves it.
    #[test]
    fn a_member_scope_move_holds_a_reader_of_the_member_object() {
        use crate::db_index::derivation::{self, FactKey};
        let file_id = FileId::new(9);
        let member_id = make_member_id(file_id, 20);
        let mut index = LuaMemberIndex::new();
        index.add_member(path("Holder"), make_member(member_id, "x"));
        let _recording = derivation::begin(derivation::DerivationId::workspace(
            derivation::DerivationKind::GmodLoad,
        ));
        assert!(index.get_member(&member_id).is_some());
        let reads = derivation::end().into_recorded().2;
        let [read] = reads.as_slice() else {
            panic!("expected one read, got {reads:?}");
        };
        let member = FactKey::Member(member_id);
        assert!(member.is_base());
        assert_eq!(read.base(), Some(member.clone()));
        let fingerprint = |index: &LuaMemberIndex, key: &FactKey| {
            derivation::in_file(Some(file_id), || index.fact_fingerprint(key))
        };
        let (member_seen, read_seen) = (fingerprint(&index, &member), fingerprint(&index, read));

        let scope = TextRange::new(TextSize::new(10), TextSize::new(100));
        index.set_member_function_scope_range(member_id, Some(scope));
        assert_ne!(fingerprint(&index, &member), member_seen);
        assert_eq!(fingerprint(&index, read), read_seen);

        index.rekey_member(member_id, LuaMemberKey::Name("y".into()));
        assert_ne!(fingerprint(&index, read), read_seen);
    }

    /// A reader of a member's owner records the member's home, a fact of its
    /// own that a transfer moves too: the member's function scope moving holds
    /// it, a move to another owner moves it.
    #[test]
    fn a_member_scope_move_holds_a_reader_of_the_member_owner() {
        use crate::db_index::derivation::{self, FactKey};
        let file_id = FileId::new(9);
        let member_id = make_member_id(file_id, 20);
        let mut index = LuaMemberIndex::new();
        index.add_member(path("Holder"), make_member(member_id, "x"));
        let _recording = derivation::begin(derivation::DerivationId::workspace(
            derivation::DerivationKind::GmodLoad,
        ));
        assert_eq!(index.get_current_owner(&member_id), Some(&path("Holder")));
        let reads = derivation::end().into_recorded().2;
        let [read] = reads.as_slice() else {
            panic!("expected one read, got {reads:?}");
        };
        assert_eq!(read.key(), &FactKey::MemberHome(member_id));
        let fingerprint = |index: &LuaMemberIndex, key: &FactKey| {
            derivation::in_file(Some(file_id), || index.fact_fingerprint(key))
        };
        let seen = fingerprint(&index, read);

        let scope = TextRange::new(TextSize::new(10), TextSize::new(100));
        index.set_member_function_scope_range(member_id, Some(scope));
        assert_eq!(fingerprint(&index, read), seen);

        index.set_member_owner(path("Other"), file_id, member_id);
        assert_ne!(fingerprint(&index, read), seen);
    }

    /// `MYTBL["net.handler"] = f` keys one member `net.handler` under `MYTBL`.
    /// Its global path is `MYTBL.net.handler`, whose last dotted segment is
    /// `handler` — a key nothing is stored under.
    #[test]
    fn history_for_a_global_path_finds_a_member_whose_key_contains_dots() {
        let owner = LuaMemberOwner::GlobalPath(GlobalId::new("MYTBL"));
        let global_id = GlobalId::new("MYTBL.net.handler");
        let member_id = make_index_member_id(FileId::new(1), 10);

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            LuaMember::new(
                member_id,
                LuaMemberKey::Name("net.handler".into()),
                LuaMemberFeature::FileFieldDecl,
                Some(global_id.clone()),
            ),
        );

        assert_eq!(
            index.get_member_history_for_global_path(&owner, &global_id),
            vec![member_id]
        );
    }

    #[test]
    fn history_for_a_global_path_still_finds_a_plain_nested_member() {
        let owner = LuaMemberOwner::GlobalPath(GlobalId::new("MYTBL.net"));
        let global_id = GlobalId::new("MYTBL.net.handler");
        let member_id = make_index_member_id(FileId::new(1), 10);

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            LuaMember::new(
                member_id,
                LuaMemberKey::Name("handler".into()),
                LuaMemberFeature::FileFieldDecl,
                Some(global_id.clone()),
            ),
        );

        assert_eq!(
            index.get_member_history_for_global_path(&owner, &global_id),
            vec![member_id]
        );
    }

    #[test]
    fn an_owner_whose_only_member_is_gone_has_no_live_member() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let file_id = FileId::new(1);
        let member_id = make_member_id(file_id, 10);

        let mut index = LuaMemberIndex::new();
        index.add_member(owner.clone(), make_member(member_id, "field"));
        assert!(index.has_live_member(&owner));

        index.remove(file_id);
        assert!(!index.has_live_member(&owner));
    }

    fn make_expr_member(member_id: LuaMemberId, key: LuaMemberKey) -> LuaMember {
        LuaMember::new(member_id, key, LuaMemberFeature::FileFieldDecl, None)
    }

    #[test]
    fn rekey_member_moves_the_member_between_slots() {
        let file = FileId::new(1);
        let owner = LuaMemberOwner::Element(site(1, 10, 12));
        let old_key = LuaMemberKey::ExprType(crate::LuaType::Integer);
        let new_key = LuaMemberKey::ExprType(crate::LuaType::String);
        let mover = make_index_member_id(file, 20);
        let stayer = make_index_member_id(file, 30);

        let mut index = LuaMemberIndex::new();
        index.add_member(owner.clone(), make_expr_member(mover, old_key.clone()));
        index.add_member(owner.clone(), make_expr_member(stayer, old_key.clone()));

        index.rekey_member(mover, new_key.clone());

        assert_eq!(index.get_member(&mover).unwrap().get_key(), &new_key);
        assert_eq!(
            index
                .get_members_with_key(&owner, &new_key)
                .unwrap()
                .iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![mover]
        );
        assert_eq!(
            index
                .get_members_with_key(&owner, &old_key)
                .unwrap()
                .iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![stayer]
        );
        assert!(
            index
                .get_current_members_for_key(&old_key)
                .iter()
                .all(|member| member.get_id() == stayer)
        );
        assert_eq!(index.get_current_members_for_key(&new_key).len(), 1);
        // The whole-owner view still yields both, once each.
        let mut owner_ids = index
            .get_members(&owner)
            .unwrap()
            .iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        owner_ids.sort_by_key(|id| member_id_sort_key(*id));
        assert_eq!(owner_ids, vec![mover, stayer]);
    }

    #[test]
    fn removing_a_rekeyed_members_file_leaves_no_residue() {
        let file = FileId::new(1);
        let owner = LuaMemberOwner::Element(site(1, 10, 12));
        let old_key = LuaMemberKey::ExprType(crate::LuaType::Integer);
        let new_key = LuaMemberKey::ExprType(crate::LuaType::String);
        let member_id = make_index_member_id(file, 20);

        let mut index = LuaMemberIndex::new();
        index.add_member(owner.clone(), make_expr_member(member_id, old_key.clone()));
        index.rekey_member(member_id, new_key.clone());

        index.remove_files(&[file]);

        assert!(index.get_member(&member_id).is_none());
        assert!(index.get_file_members(file).is_empty());
        assert!(index.get_members(&owner).is_none());
        assert!(!index.owner_members.contains_key(&owner));
        for key in [&old_key, &new_key] {
            assert!(index.get_current_members_for_key(key).is_empty());
            assert!(!index.current_members_by_key.contains_key(key));
        }
        assert!(!index.member_owner_key_index.contains_key(&owner));
        assert!(!index.member_owner_key_history_index.contains_key(&owner));
        assert!(!index.member_current_owner.contains_key(&member_id));
    }

    #[test]
    fn add_file_entries_leaves_index_contents_unchanged() {
        let mut index = LuaMemberIndex::new();
        let file_id = FileId::new(1);
        index.add_file_entries(file_id);
        assert!(index.get_file_members(file_id).is_empty());

        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("ReservedType"));
        let member_id = make_index_member_id(file_id, 10);
        index.add_member(owner.clone(), make_member(member_id, "field"));

        assert_eq!(owner_member_ids(&index, &owner), vec![member_id]);
        assert_eq!(index.get_file_members(file_id).len(), 1);
    }

    #[test]
    fn owner_only_transition_hides_and_readmission_restores_same_owner_member() {
        let file_id = FileId::new(1);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("Holder"));
        let member_id = make_member_id(file_id, 1);

        let mut index = LuaMemberIndex::new();
        index.add_member(owner.clone(), make_member(member_id, "field"));
        assert_eq!(owner_member_ids(&index, &owner), vec![member_id]);

        index
            .set_member_owner_only(owner.clone(), file_id, member_id)
            .expect("owner-only filing should succeed");
        assert_eq!(index.get_current_owner(&member_id), Some(&owner));
        assert!(index.owner_only_members.contains(&member_id));
        assert!(
            index.get_members(&owner).is_none_or(|members| {
                !members.iter().any(|member| member.get_id() == member_id)
            })
        );

        index
            .set_member_owner(owner.clone(), file_id, member_id)
            .expect("owner reassignment should succeed");
        index.add_member_to_owner(owner.clone(), member_id);
        assert!(!index.owner_only_members.contains(&member_id));
        assert_eq!(owner_member_ids(&index, &owner), vec![member_id]);
    }

    #[test]
    fn refiling_under_a_former_owner_does_not_duplicate_history() {
        let file_id = FileId::new(1);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("ReturnOwner"));
        let away = LuaMemberOwner::Type(LuaTypeDeclId::global("AwayOwner"));
        let key = LuaMemberKey::Name("field".into());
        let member_id = make_member_id(file_id, 1);

        let mut index = LuaMemberIndex::new();
        index.add_member(owner.clone(), make_member(member_id, "field"));
        // The canonical move path: settle re-homes the member, leaving a
        // left-behind history entry under the owner it departed.
        index
            .set_member_owner(away.clone(), file_id, member_id)
            .expect("owner reassignment should succeed");
        index.add_member_to_owner(away.clone(), member_id);
        // A later pass re-derives the original owner and re-files the member
        // under the (owner, key) slot its history already lists.
        index.add_member(owner.clone(), make_member(member_id, "field"));

        assert_eq!(
            index.get_current_owner_member_ids_for_key_unsorted(&owner, &key),
            vec![member_id],
            "a returning member must occupy its owner/key slot once"
        );
        assert_eq!(index.owner_key_history_len(&owner, &key), 1);
        assert_eq!(
            index
                .get_current_owner_members_for_key(&owner, &key)
                .into_iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![member_id]
        );
        assert_eq!(owner_member_ids(&index, &owner), vec![member_id]);
    }

    #[test]
    fn refiling_under_a_former_key_does_not_duplicate_history() {
        let file_id = FileId::new(1);
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("KeyOwner"));
        let first_key = LuaMemberKey::ExprType(crate::LuaType::Integer);
        let second_key = LuaMemberKey::ExprType(crate::LuaType::String);
        let member_id = make_index_member_id(file_id, 1);

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            make_expr_member(member_id, first_key.clone()),
        );
        index.rekey_member(member_id, second_key.clone());
        // Re-filing under the key the member already occupied must not append
        // a second history entry to that slot.
        index.add_member(
            owner.clone(),
            make_expr_member(member_id, first_key.clone()),
        );

        assert_eq!(
            index.get_current_owner_member_ids_for_key_unsorted(&owner, &first_key),
            vec![member_id],
            "a member returning to a former key must occupy the slot once"
        );
        assert_eq!(index.owner_key_history_len(&owner, &first_key), 1);
        assert_eq!(owner_member_ids(&index, &owner), vec![member_id]);
    }
    /// The installed state a batch install has to reproduce, in a stable order.
    fn installed_state(index: &LuaMemberIndex) -> Vec<String> {
        let mut state = Vec::new();
        state.extend(
            index
                .owner_history_fingerprints
                .iter()
                .map(|entry| format!("history {entry:?}")),
        );
        state.extend(
            index
                .key_fingerprints
                .iter()
                .map(|entry| format!("key {entry:?}")),
        );
        state.extend(
            index
                .file_fingerprints
                .iter()
                .map(|entry| format!("file {entry:?}")),
        );
        state.extend(
            index
                .current_members_by_key
                .iter()
                .map(|entry| format!("current {entry:?}")),
        );
        state.extend(index.owner_members.iter().map(|(owner, members)| {
            format!(
                "owner {owner:?} {:?} {}",
                members.sorted_member_ids(),
                members.fingerprint()
            )
        }));
        state.extend(
            index
                .member_owner_key_history_index
                .iter()
                .flat_map(|(owner, slots)| {
                    slots
                        .iter()
                        .map(move |(key, ids)| format!("slot {owner:?} {key:?} {ids:?}"))
                }),
        );
        state.sort();
        state
    }

    #[test]
    fn forks_rekeying_different_members_alike_install_the_history_a_recount_gives() {
        use crate::db_index::cow_shards::TaskFork;
        let owner = LuaMemberOwner::GlobalPath(crate::GlobalId::new("ix.gui"));
        let unknown = LuaMemberKey::ExprType(crate::LuaType::Unknown);
        let string = LuaMemberKey::ExprType(crate::LuaType::String);
        let movers = [
            make_index_member_id(FileId::new(330), 4446),
            make_index_member_id(FileId::new(387), 1174),
        ];
        let mut base = LuaMemberIndex::new();
        for mover in movers {
            base.add_member(owner.clone(), make_expr_member(mover, unknown.clone()));
        }
        let frozen = base.task_fork();
        let forks = movers
            .iter()
            .map(|mover| {
                let mut fork = frozen.task_fork();
                fork.rekey_member(*mover, string.clone());
                fork
            })
            .collect::<Vec<_>>();
        let mut conflicts = vec![Vec::new(); forks.len()];
        base.install_tasks_reporting(
            &frozen,
            forks.into_iter().enumerate().collect(),
            &mut conflicts,
        );

        assert_eq!(
            base.owner_history_fingerprints.get(&owner).copied(),
            Some(base.recomputed_owner_history_fingerprint(&owner))
        );
    }

    #[test]
    fn installing_forks_field_by_field_matches_installing_them_one_by_one() {
        use crate::db_index::cow_shards::TaskFork;
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("Shared"));
        let mut base = LuaMemberIndex::new();
        base.add_member(
            owner.clone(),
            make_member(make_member_id(FileId::new(1), 1), "a"),
        );
        let frozen = base.task_fork();
        let forks = (2..6)
            .map(|file| {
                let mut fork = frozen.task_fork();
                let key = if file % 2 == 0 { "a" } else { "b" };
                fork.add_member(
                    owner.clone(),
                    make_member(make_member_id(FileId::new(file), file * 10), key),
                );
                fork
            })
            .collect::<Vec<_>>();
        let mut one_by_one = base.clone();
        let mut one_by_one_conflicts = vec![Vec::new(); forks.len()];
        for (at, fork) in forks.iter().cloned().enumerate() {
            one_by_one.install_task_reporting(&frozen, fork, &mut one_by_one_conflicts[at]);
        }
        let mut by_field = base.clone();
        let mut by_field_conflicts = vec![Vec::new(); forks.len()];
        by_field.install_tasks_reporting(
            &frozen,
            forks.into_iter().enumerate().collect(),
            &mut by_field_conflicts,
        );

        assert_eq!(
            (installed_state(&by_field), by_field_conflicts),
            (installed_state(&one_by_one), one_by_one_conflicts)
        );
    }

    #[derive(Clone, Copy, Debug)]
    enum HomeStep {
        Filed(&'static str),
        Transfer,
        ResetTransfer,
    }

    fn class(name: &str) -> LuaMemberOwner {
        LuaMemberOwner::Type(LuaTypeDeclId::global(name))
    }

    fn apply_home_step(index: &mut LuaMemberIndex, id: LuaMemberId, step: HomeStep) {
        match step {
            HomeStep::Filed(owner) => {
                index.set_member_owner(class(owner), id.file_id, id);
                index.add_member_to_owner(class(owner), id);
            }
            HomeStep::Transfer => index.transfer_member(class("Panel"), id, false),
            HomeStep::ResetTransfer => index.reset_transfer(id),
        }
    }

    #[test]
    fn a_member_is_homed_by_its_transfer_over_its_filing_in_any_order() {
        let steps = [
            HomeStep::Filed("First"),
            HomeStep::Transfer,
            HomeStep::ResetTransfer,
            HomeStep::Filed("Second"),
        ];
        let id = make_index_member_id(FileId::new(1), 10);
        let mut orders = vec![Vec::<usize>::new()];
        for _ in 0..steps.len() {
            orders = orders
                .iter()
                .flat_map(|order| {
                    (0..steps.len())
                        .filter(|step| !order.contains(step))
                        .map(|step| [order.as_slice(), &[step]].concat())
                })
                .collect();
        }
        assert_eq!(orders.len(), 24);
        for order in orders {
            let order = order.iter().map(|step| steps[*step]).collect::<Vec<_>>();
            let mut index = LuaMemberIndex::new();
            let mut filed_only = LuaMemberIndex::new();
            for index in [&mut index, &mut filed_only] {
                index.add_member(class("Origin"), make_member(id, "Gender"));
            }
            let mut filed = "Origin";
            let mut transferred = false;
            for step in &order {
                apply_home_step(&mut index, id, *step);
                match step {
                    HomeStep::Filed(owner) => {
                        apply_home_step(&mut filed_only, id, *step);
                        filed = owner;
                    }
                    HomeStep::Transfer => transferred = true,
                    HomeStep::ResetTransfer => transferred = false,
                }
            }
            let home = class(if transferred { "Panel" } else { filed });

            assert_eq!(index.get_current_owner(&id), Some(&home), "{order:?}");
            assert!(owner_member_ids(&index, &home).contains(&id), "{order:?}");
            assert_eq!(
                index.fact_fingerprint(&FactKey::Member(id)),
                filed_only.fact_fingerprint(&FactKey::Member(id)),
                "{order:?}"
            );
        }
    }

    #[test]
    fn removing_a_co_writer_leaves_the_slot_as_a_build_of_the_rest_files_it() {
        let kept = make_member_id(FileId::new(1), 10);
        let key = LuaMemberKey::Name("x".into());
        let mut cold = LuaMemberIndex::new();
        cold.add_member(path("Holder"), make_member(kept, "x"));
        for removed_start in [5, 20] {
            let mut index = LuaMemberIndex::new();
            index.add_member(path("Holder"), make_member(kept, "x"));
            let removed = make_member_id(FileId::new(2), removed_start);
            index.add_member(path("Holder"), make_member(removed, "x"));

            index.remove(FileId::new(2));

            assert_eq!(
                index.get_member_item(&path("Holder"), &key),
                cold.get_member_item(&path("Holder"), &key)
            );
        }
    }

    #[test]
    fn refiling_a_member_under_another_owner_and_key_leaves_nothing_current_behind() {
        let id = make_member_id(FileId::new(1), 10);
        let old_key = LuaMemberKey::Name("old".into());
        let mut index = LuaMemberIndex::new();
        index.add_member(path("A"), make_member(id, "old"));

        index.add_member(path("B"), make_member(id, "new"));

        let current_history = index
            .get_current_owner_member_history(&path("A"))
            .iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        let current_for_old_key = index.get_current_members_for_key(&old_key).len();
        let visible_under_old_slot = index.visible_member_count_for_owner_key(&path("A"), &old_key);
        assert_eq!(
            (current_history, current_for_old_key, visible_under_old_slot),
            (Vec::new(), 0, 0)
        );
    }

    #[test]
    fn a_member_forgotten_with_its_literal_is_unlisted_from_its_aliases() {
        let literal = site(1, 10, 30);
        let alias = LuaMemberOwner::Type(LuaTypeDeclId::global("Alias"));
        let key = LuaMemberKey::Name("field".into());
        let own = make_index_member_id(FileId::new(3), 5);
        let homed = make_index_member_id(FileId::new(2), 20);
        let mut index = LuaMemberIndex::new();
        index.add_member(alias.clone(), make_member(own, "field"));
        index.add_member(
            LuaMemberOwner::Element(literal.clone()),
            make_member(homed, "field"),
        );
        index.add_member_alias_to_owner(alias.clone(), homed);

        let forgotten = index.remove_deleted_element_owners(&[literal]);

        assert_eq!(
            (forgotten, index.get_member_item(&alias, &key)),
            (vec![homed], Some(&LuaMemberIndexItem::One(own)))
        );
    }

    #[test]
    fn removing_a_file_unlists_its_members_from_every_owner_they_were_listed_under() {
        let away = LuaMemberOwner::Type(LuaTypeDeclId::global("Away"));
        let member_id = make_member_id(FileId::new(2), 20);
        let mut index = LuaMemberIndex::new();
        index.add_member(path("Home"), make_member(member_id, "field"));
        index.add_member_to_owner(away.clone(), member_id);

        index.remove(FileId::new(2));

        assert_eq!(
            index.get_member_item(&away, &LuaMemberKey::Name("field".into())),
            None
        );
    }
}
