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
    fn file_define_assignment_history_stays_visible_for_owner_key_queries() {
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
            FileId::new(5),
        );

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
            Some(&LuaMemberIndexItem::One(second_member_id))
        );
        let member_ids = index
            .get_members_for_owner_key(&owner, &key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(member_ids, vec![first_member_id, second_member_id]);
    }

    #[test]
    fn retained_file_define_keeps_owner_key_history() {
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
        index.add_member(
            owner.clone(),
            LuaMember::new(
                first_member_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );
        index.add_member(
            owner.clone(),
            LuaMember::new(
                second_member_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        index
            .retain_only_member_for_owner_key(second_member_id)
            .expect("retain should succeed");

        let visible_member_ids = index
            .get_members_for_owner_key(&owner, &key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(visible_member_ids, vec![second_member_id]);

        let history_member_ids = index
            .get_current_owner_members_for_key(&owner, &key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(history_member_ids, vec![first_member_id, second_member_id]);
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
                assignment_member_id,
                meta_member_id,
            ]))
        );
    }

    #[test]
    fn retain_only_member_for_owner_key_keeps_mixed_visible_members() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let declaration_member_id = make_member_id(FileId::new(4), 1);
        let assignment_member_id = make_index_member_id(FileId::new(4), 3);

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            make_member_with_feature(
                declaration_member_id,
                "field",
                LuaMemberFeature::FileFieldDecl,
            ),
        );
        index.add_member(
            owner.clone(),
            make_member_with_feature(assignment_member_id, "field", LuaMemberFeature::FileDefine),
        );

        index
            .retain_only_member_for_owner_key(assignment_member_id)
            .expect("retain should no-op for mixed visible members");

        let visible_member_ids = index
            .get_members_for_owner_key(&owner, &key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(
            visible_member_ids,
            vec![declaration_member_id, assignment_member_id]
        );
    }

    #[test]
    fn preserve_members_for_owner_key_filters_dedups_and_updates_visible_item() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let other_owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OtherOwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let first_member_id = make_index_member_id(FileId::new(4), 1);
        let second_member_id = make_index_member_id(FileId::new(4), 3);
        let other_owner_member_id = make_index_member_id(FileId::new(5), 1);
        let other_key_member_id = make_index_member_id(FileId::new(4), 5);

        let mut index = LuaMemberIndex::new();
        for member_id in [first_member_id, second_member_id] {
            index.add_member(
                owner.clone(),
                make_member_with_feature(member_id, "field", LuaMemberFeature::FileDefine),
            );
        }
        index.add_member(
            other_owner,
            make_member_with_feature(other_owner_member_id, "field", LuaMemberFeature::FileDefine),
        );
        index.add_member(
            owner.clone(),
            make_member_with_feature(other_key_member_id, "other", LuaMemberFeature::FileDefine),
        );

        index
            .preserve_members_for_owner_key(
                first_member_id,
                vec![
                    second_member_id,
                    other_owner_member_id,
                    first_member_id,
                    second_member_id,
                    other_key_member_id,
                ],
            )
            .expect("preserve should succeed");

        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::Many(vec![
                second_member_id,
                first_member_id,
            ]))
        );
        let visible_member_ids = index
            .get_members_for_owner_key(&owner, &key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        assert_eq!(visible_member_ids, vec![second_member_id, first_member_id]);
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
    fn alias_merge_does_not_duplicate_an_id_already_in_an_unsorted_item() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let earlier_member_id = make_member_id(FileId::new(1), 10);
        let later_member_id = make_member_id(FileId::new(2), 20);

        // Decl inserts append in arrival order, so adding the later id first
        // leaves the stored item unsorted.
        let mut index = LuaMemberIndex::new();
        index.add_member(owner.clone(), make_member(later_member_id, "field"));
        index.add_member(owner.clone(), make_member(earlier_member_id, "field"));
        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::Many(vec![
                later_member_id,
                earlier_member_id,
            ]))
        );

        index.add_member_alias_to_owner(owner.clone(), later_member_id);

        let Some(LuaMemberIndexItem::Many(member_ids)) = index.get_member_item(&owner, &key) else {
            panic!("the item should still hold both members");
        };
        assert_eq!(
            member_ids.len(),
            2,
            "aliasing an id already in the item must not add it again"
        );
        assert_eq!(owner_member_ids(&index, &owner).len(), 2);
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
    fn global_path_key_keeps_every_writer_in_history_while_one_wins_the_visible_slot() {
        let owner = LuaMemberOwner::GlobalPath(crate::GlobalId::new("cityrp"));
        let first_member_id = make_member_id(FileId::new(1), 10);
        let second_member_id = make_member_id(FileId::new(2), 20);

        let mut index = LuaMemberIndex::new();
        for member_id in [first_member_id, second_member_id] {
            index.add_member(
                owner.clone(),
                make_member_with_feature(member_id, "menu", LuaMemberFeature::FileDefine),
            );
        }

        assert_eq!(
            index
                .get_member_history(&owner)
                .iter()
                .map(|member| member.get_id())
                .collect::<Vec<_>>(),
            vec![first_member_id, second_member_id],
            "history must enumerate every file that wrote the key"
        );
        assert_eq!(
            owner_member_ids(&index, &owner),
            vec![second_member_id],
            "the visible slot is still last-writer-wins"
        );
    }

    /// `cityrp.progresshud = {}` is written by both `cl_progress_hud.lua` and
    /// `sv_progress_hud.lua`. Only one can hold the visible slot, and the
    /// canonical owner of the path is what the survivor is filed under — so if
    /// arrival decided the survivor, a re-index moved the member's owner on
    /// unchanged source.
    #[test]
    fn cross_file_assignment_writers_elect_the_visible_slot_by_source_order() {
        let owner = LuaMemberOwner::GlobalPath(crate::GlobalId::new("cityrp"));
        let earlier_member_id = make_index_member_id(FileId::new(1), 10);
        let later_member_id = make_index_member_id(FileId::new(2), 20);

        for arrival in [
            [earlier_member_id, later_member_id],
            [later_member_id, earlier_member_id],
        ] {
            let mut index = LuaMemberIndex::new();
            for member_id in arrival {
                index.add_member(
                    owner.clone(),
                    make_member_with_feature(
                        member_id,
                        "progresshud",
                        LuaMemberFeature::FileDefine,
                    ),
                );
            }

            assert_eq!(
                owner_member_ids(&index, &owner),
                vec![later_member_id],
                "the visible writer must not depend on which file was analysed first"
            );
        }
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

    #[test]
    fn marked_non_overwriting_file_define_does_not_preserve_unmarked_assignment() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("field".into());
        let class_assignment_id = LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::IndexExpr.into(),
                TextRange::new(TextSize::new(1), TextSize::new(2)),
            ),
            FileId::new(4),
        );
        let guarded_assignment_id = LuaMemberId::new(
            LuaSyntaxId::new(
                LuaSyntaxKind::IndexExpr.into(),
                TextRange::new(TextSize::new(3), TextSize::new(4)),
            ),
            FileId::new(4),
        );

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            LuaMember::new(
                class_assignment_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );
        index.mark_non_overwriting_assignment_member(guarded_assignment_id);
        index.add_member(
            owner.clone(),
            LuaMember::new(
                guarded_assignment_id,
                key.clone(),
                LuaMemberFeature::FileDefine,
                None,
            ),
        );

        assert_eq!(
            index.get_member_item(&owner, &key),
            Some(&LuaMemberIndexItem::One(guarded_assignment_id))
        );
    }

    /// A guarded bootstrap in one file and a plain assignment in another is
    /// the order-dependent case: whichever was processed last used to take the
    /// visible slot, so the surviving writer followed load order. The
    /// bootstrap contributes no type of its own, so the plain writer must win
    /// either way.
    #[test]
    fn cross_file_bootstrap_never_displaces_a_plain_writer() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OwnedType"));
        let key = LuaMemberKey::Name("stock".into());
        let plain_id = make_index_member_id(FileId::new(1), 10);
        let bootstrap_id = make_index_member_id(FileId::new(2), 20);

        let visible_after = |bootstrap_first: bool| {
            let mut index = LuaMemberIndex::new();
            let order = if bootstrap_first {
                [bootstrap_id, plain_id]
            } else {
                [plain_id, bootstrap_id]
            };
            for member_id in order {
                if member_id == bootstrap_id {
                    index.mark_non_overwriting_assignment_member(member_id);
                }
                index.add_member(
                    owner.clone(),
                    LuaMember::new(member_id, key.clone(), LuaMemberFeature::FileDefine, None),
                );
            }
            owner_member_ids(&index, &owner)
        };

        assert_eq!(
            visible_after(false),
            vec![plain_id],
            "plain writer analysed first"
        );
        assert_eq!(
            visible_after(true),
            vec![plain_id],
            "bootstrap analysed first"
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

    #[test]
    fn pruned_cross_file_member_is_unreachable_until_reattached() {
        let owner_file = FileId::new(1);
        let sibling_file = FileId::new(2);
        let owner = LuaMemberOwner::Element(crate::InFiled::new(
            owner_file,
            TextRange::new(TextSize::new(0), TextSize::new(2)),
        ));
        let key = LuaMemberKey::Name("field".into());
        let own_write = make_index_member_id(owner_file, 10);
        let sibling_write = make_index_member_id(sibling_file, 10);

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            make_member_with_feature(sibling_write, "field", LuaMemberFeature::FileDefine),
        );
        index.add_member(
            owner.clone(),
            make_member_with_feature(own_write, "field", LuaMemberFeature::FileDefine),
        );
        assert!(index.member_reachable_for_owner_key(&owner, &key, sibling_write));

        // The owning file's own write collapsing the slot to itself is how a
        // re-walk that cannot see the settled sibling prunes it: ownership
        // survives, reachability does not.
        index.retain_only_member_for_owner_key(own_write);
        assert!(!index.member_reachable_for_owner_key(&owner, &key, sibling_write));
        assert_eq!(index.get_member_owner(&sibling_write), Some(&owner));

        // The reconcile repair path: re-homing onto the owner it already has
        // restores the visible entry.
        index.set_member_owner(owner.clone(), sibling_file, sibling_write);
        index.add_member_to_owner(owner.clone(), sibling_write);
        assert!(index.member_reachable_for_owner_key(&owner, &key, sibling_write));
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
        index.record_member_assignment_contribution(
            member_id,
            crate::db_index::MemberAssignmentContribution {
                bound_type: crate::LuaType::Integer,
                source_type: crate::LuaType::Integer,
                doc_type: None,
                guarded_bootstrap: false,
                preserve_table_literals: false,
            },
        );
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
        assert!(
            index
                .assignment_contributions
                .contribution_of(&member_id)
                .is_none()
        );
        assert!(
            index
                .assignment_contributions
                .contribution_group_of(&member_id)
                .is_none()
        );
    }

    /// `pMeta[fn] = function(...)` in each arm of `if optional then ... else
    /// ... end` is a conditional-branch slot, first resolved while the key is
    /// still `ExprType(unknown)`. The settled tail then rekeys the members onto
    /// the key the index expression actually infers to, and the plain insert
    /// classifier that runs on the way in keeps only the first arm.
    #[test]
    fn rekeying_a_conditional_branch_member_re_resolves_its_new_slot() {
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("Player"));
        let old_key = LuaMemberKey::ExprType(crate::LuaType::Unknown);
        let new_key = LuaMemberKey::ExprType(crate::LuaType::String);
        let first_arm = make_index_member_id(FileId::new(1), 10);
        let second_arm = make_index_member_id(FileId::new(1), 20);
        let third_arm = make_index_member_id(FileId::new(1), 40);
        let fourth_arm = make_index_member_id(FileId::new(1), 50);

        let mut index = LuaMemberIndex::new();
        for (member_id, branch, if_range) in [
            (first_arm, (5, 15), (0, 30)),
            (second_arm, (15, 25), (0, 30)),
            (third_arm, (35, 45), (30, 60)),
            (fourth_arm, (45, 55), (30, 60)),
        ] {
            index.add_conditional_branch_range(
                FileId::new(1),
                TextRange::new(TextSize::new(branch.0), TextSize::new(branch.1)),
                TextRange::new(TextSize::new(if_range.0), TextSize::new(if_range.1)),
            );
            index.add_member(
                owner.clone(),
                LuaMember::new(
                    member_id,
                    old_key.clone(),
                    LuaMemberFeature::FileDefine,
                    None,
                ),
            );
            index.mark_conditional_branch_assignment_member(member_id, true);
        }
        let all_arms = vec![first_arm, second_arm, third_arm, fourth_arm];
        assert_eq!(owner_member_ids(&index, &owner), all_arms);

        for member_id in all_arms.iter().copied() {
            index.rekey_member(member_id, new_key.clone());
        }

        assert_eq!(
            owner_member_ids(&index, &owner),
            all_arms,
            "every arm must survive the key move, as they did under the old key"
        );
    }

    /// `holdem.games = holdem.games or {}` is written by both `cl_init.lua` and
    /// `sv_init.lua`, and both survive a cold build. Re-indexing one of them
    /// re-inserts its write before the walk marks it a guarded self-assignment,
    /// so the plain classifier collapses the slot to it; the other file is not
    /// in the batch, so nothing else can bring its writer back.
    #[test]
    fn marking_a_guarded_writer_readmits_the_co_writer_its_reinsert_evicted() {
        let owner = LuaMemberOwner::GlobalPath(crate::GlobalId::new("holdem"));
        let client = make_index_member_id(FileId::new(1), 10);
        let server = make_index_member_id(FileId::new(2), 20);

        let mut index = LuaMemberIndex::new();
        index.add_member(
            owner.clone(),
            make_member_with_feature(client, "games", LuaMemberFeature::FileDefine),
        );
        index.mark_non_overwriting_assignment_member(client);
        index.add_member_to_owner(owner.clone(), client);
        assert_eq!(owner_member_ids(&index, &owner), vec![client]);

        // The re-index of the server file: its write arrives before the walk
        // marks it, so the classifier collapses the slot to it.
        index.add_member(
            owner.clone(),
            make_member_with_feature(server, "games", LuaMemberFeature::FileDefine),
        );
        assert_eq!(
            owner_member_ids(&index, &owner),
            vec![server],
            "the unmarked re-insert evicts the co-writer, which is what the mark repairs"
        );

        index.mark_non_overwriting_assignment_member(server);

        assert_eq!(
            owner_member_ids(&index, &owner),
            vec![client, server],
            "the co-writer in a file this batch never touched must come back"
        );
    }
}
