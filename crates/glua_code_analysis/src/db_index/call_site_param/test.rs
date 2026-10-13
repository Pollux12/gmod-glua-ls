#[cfg(test)]
mod tests {
    use crate::db_index::call_site_param::*;
    use rowan::{TextRange, TextSize};

    fn signature_id(file_id: FileId, position: u32) -> LuaSignatureId {
        serde_json::from_str(&format!("\"{}|{}\"", file_id.id, position)).unwrap()
    }

    fn facts(
        contributions: Vec<(LuaSignatureId, usize, LuaType)>,
    ) -> Vec<(LuaSignatureId, usize, Option<LuaTypeFact>)> {
        contributions
            .into_iter()
            .map(|(signature_id, param_idx, typ)| {
                (signature_id, param_idx, Some(LuaTypeFact::certain(typ)))
            })
            .collect()
    }

    fn set(
        index: &mut CallSiteParamIndex,
        file_id: FileId,
        contributions: Vec<(LuaSignatureId, usize, LuaType)>,
    ) {
        index.set_callback_contributions(file_id, facts(contributions));
    }

    fn call(start: u32, end: u32) -> TextRange {
        TextRange::new(TextSize::new(start), TextSize::new(end))
    }

    fn set_call(
        index: &mut CallSiteParamIndex,
        file_id: FileId,
        range: TextRange,
        contributions: Vec<(LuaSignatureId, usize, LuaType)>,
    ) {
        index.set_call_contributions(file_id, range, facts(contributions));
    }

    fn inferred_union_members(
        index: &CallSiteParamIndex,
        signature_id: &LuaSignatureId,
    ) -> Vec<LuaType> {
        match index.get_inferred_param(signature_id, 0) {
            Some(LuaType::Union(union)) => union.as_ref().into_vec(),
            Some(other) => vec![other.clone()],
            None => panic!("expected inferred param for signature {signature_id:?}"),
        }
    }

    #[test]
    fn inferred_param_union_order_is_stable_across_file_insertion_order() {
        let lower_file_id = FileId::new(1);
        let higher_file_id = FileId::new(2);
        let signature_id = signature_id(FileId::new(10), 0);

        let mut forward_index = CallSiteParamIndex::new();
        set(
            &mut forward_index,
            higher_file_id,
            vec![(signature_id, 0, LuaType::Boolean)],
        );
        set(
            &mut forward_index,
            lower_file_id,
            vec![(signature_id, 0, LuaType::String)],
        );

        let mut reverse_index = CallSiteParamIndex::new();
        set(
            &mut reverse_index,
            lower_file_id,
            vec![(signature_id, 0, LuaType::String)],
        );
        set(
            &mut reverse_index,
            higher_file_id,
            vec![(signature_id, 0, LuaType::Boolean)],
        );

        let expected = vec![LuaType::Boolean, LuaType::String];
        assert_eq!(
            inferred_union_members(&forward_index, &signature_id),
            expected
        );
        assert_eq!(
            inferred_union_members(&reverse_index, &signature_id),
            expected
        );
        assert_eq!(forward_index.inferred_params, reverse_index.inferred_params);
    }

    /// Replacing or removing one file's evidence leaves the index a fresh
    /// build of the surviving evidence would have.
    #[test]
    fn replacing_and_removing_evidence_matches_a_fresh_build() {
        let caller_a = FileId::new(1);
        let caller_b = FileId::new(2);
        let caller_c = FileId::new(3);
        let first = signature_id(FileId::new(10), 100);
        let second = signature_id(FileId::new(10), 200);

        let mut index = CallSiteParamIndex::new();
        set(
            &mut index,
            caller_a,
            vec![(first, 0, LuaType::String), (second, 1, LuaType::Integer)],
        );
        set(&mut index, caller_b, vec![(first, 0, LuaType::Boolean)]);
        set(&mut index, caller_c, vec![(second, 1, LuaType::Nil)]);
        set(&mut index, caller_a, vec![(first, 0, LuaType::Number)]);
        index.remove_files(&[caller_c]);

        let mut fresh = CallSiteParamIndex::new();
        set(&mut fresh, caller_a, vec![(first, 0, LuaType::Number)]);
        set(&mut fresh, caller_b, vec![(first, 0, LuaType::Boolean)]);

        assert_eq!(index.inferred_params, fresh.inferred_params);
        assert_eq!(index.file_contributions, fresh.file_contributions);
        assert_eq!(index.contributors, fresh.contributors);
        assert_eq!(index.get_inferred_param(&second, 1), None);
    }

    /// A file whose recollection comes back empty keeps no entry.
    #[test]
    fn an_emptied_recollection_retracts_the_file() {
        let caller = FileId::new(1);
        let signature_id = signature_id(FileId::new(2), 10);
        let mut index = CallSiteParamIndex::new();
        set(&mut index, caller, vec![(signature_id, 0, LuaType::String)]);

        set(&mut index, caller, Vec::new());

        assert!(index.file_contributions.is_empty());
        assert!(index.contributors.is_empty());
        assert!(index.inferred_params.is_empty());
    }

    /// Evidence for a function in a file an edit shifted moves with the
    /// function's signature.
    #[test]
    fn a_shift_carries_evidence_to_the_moved_signature() {
        let caller = FileId::new(1);
        let callee_file = FileId::new(10);
        let before = signature_id(callee_file, 2);
        let mut index = CallSiteParamIndex::new();
        set(&mut index, caller, vec![(before, 0, LuaType::Integer)]);

        let dirty = index.remap_file_identities(&FileRemap::unvalidated(
            callee_file,
            crate::PositionMap::new("abcdef", "\nabcdef"),
        ));

        assert!(dirty.is_empty());
        assert_eq!(
            index.get_inferred_param(&signature_id(callee_file, 3), 0),
            Some(&LuaType::Integer)
        );
        assert_eq!(index.get_inferred_param(&before, 0), None);
    }

    /// Evidence for a function the edit destroyed stays as it was, and its
    /// file is reported so it is analysed again.
    #[test]
    fn evidence_for_a_signature_an_edit_destroyed_leaves_its_file_dirty() {
        let caller = FileId::new(1);
        let callee_file = FileId::new(10);
        let destroyed = signature_id(callee_file, 3);
        let mut index = CallSiteParamIndex::new();
        set(&mut index, caller, vec![(destroyed, 0, LuaType::Integer)]);

        let dirty = index.remap_file_identities(&FileRemap::unvalidated(
            callee_file,
            crate::PositionMap::new("abcdef", "abXYef"),
        ));

        assert_eq!(
            dirty,
            [crate::LostFact::new(
                caller,
                crate::db_index::derivation::FactKey::CallSiteCallbacks(caller)
            )]
        );
        assert_eq!(
            index.get_inferred_param(&destroyed, 0),
            Some(&LuaType::Integer)
        );
    }

    /// A file's calls fold in the order of their ranges, whichever order their
    /// derivations stored them in.
    #[test]
    fn calls_fold_in_range_order_whatever_order_they_were_stored_in() {
        let file_id = FileId::new(1);
        let signature_id = signature_id(FileId::new(10), 0);
        let (outer, nested, later) = (call(0, 20), call(0, 10), call(30, 40));
        let strings = vec![(signature_id, 0, LuaType::String)];
        let integers = vec![(signature_id, 0, LuaType::Integer)];
        let booleans = vec![(signature_id, 0, LuaType::Boolean)];
        let mut forward = CallSiteParamIndex::new();
        set_call(&mut forward, file_id, outer, strings.clone());
        set_call(&mut forward, file_id, nested, integers.clone());
        set_call(&mut forward, file_id, later, booleans.clone());

        let mut reverse = CallSiteParamIndex::new();
        set_call(&mut reverse, file_id, later, booleans);
        set_call(&mut reverse, file_id, nested, integers);
        set_call(&mut reverse, file_id, outer, strings);

        assert_eq!(forward.inferred_params, reverse.inferred_params);
        assert_eq!(forward.file_contributions, reverse.file_contributions);
        assert_eq!(forward.contributors, reverse.contributors);
        for key in [
            FactKey::CallSiteFile(file_id),
            FactKey::CallSiteParam(InFiled::new(file_id, outer)),
            FactKey::Contrib(signature_id, 0),
        ] {
            assert_eq!(
                forward.fact_fingerprint(&key),
                reverse.fact_fingerprint(&key)
            );
        }
        assert_eq!(inferred_union_members(&forward, &signature_id).len(), 3);
    }

    /// Replacing or retracting one call's evidence leaves the others', and
    /// the index a fresh build of the surviving evidence would have.
    #[test]
    fn a_call_replaces_and_retracts_only_its_own_evidence() {
        let file_id = FileId::new(1);
        let signature_id = signature_id(FileId::new(10), 0);
        let (first, second) = (call(0, 10), call(20, 30));
        let mut index = CallSiteParamIndex::new();
        set_call(
            &mut index,
            file_id,
            first,
            vec![(signature_id, 0, LuaType::String)],
        );
        set_call(
            &mut index,
            file_id,
            second,
            vec![(signature_id, 0, LuaType::Boolean)],
        );
        let second_key = FactKey::CallSiteParam(InFiled::new(file_id, second));
        let second_fingerprint = index.fact_fingerprint(&second_key);

        set_call(
            &mut index,
            file_id,
            first,
            vec![(signature_id, 0, LuaType::Number)],
        );
        assert_eq!(index.fact_fingerprint(&second_key), second_fingerprint);
        let members = inferred_union_members(&index, &signature_id);
        assert!(members.contains(&LuaType::Boolean) && members.contains(&LuaType::Number));
        assert_eq!(members.len(), 2);

        index.retract_call(file_id, first);
        let mut fresh = CallSiteParamIndex::new();
        set_call(
            &mut fresh,
            file_id,
            second,
            vec![(signature_id, 0, LuaType::Boolean)],
        );
        assert_eq!(index.inferred_params, fresh.inferred_params);
        assert_eq!(index.file_contributions, fresh.file_contributions);
        assert_eq!(index.contributors, fresh.contributors);
        assert_eq!(index.referencing, fresh.referencing);

        index.retract_call(file_id, second);
        assert!(index.file_contributions.is_empty());
        assert!(index.contributors.is_empty());
        assert!(index.inferred_params.is_empty());
        assert!(index.referencing.is_empty());
    }

    /// A shift carries each of a file's calls' evidence to the moved
    /// signature it names.
    #[test]
    fn a_shift_carries_each_calls_evidence() {
        let caller = FileId::new(1);
        let callee_file = FileId::new(10);
        let mut index = CallSiteParamIndex::new();
        set_call(
            &mut index,
            caller,
            call(0, 10),
            vec![(signature_id(callee_file, 2), 0, LuaType::Integer)],
        );
        set_call(
            &mut index,
            caller,
            call(20, 30),
            vec![(signature_id(callee_file, 4), 0, LuaType::String)],
        );

        let dirty = index.remap_file_identities(&FileRemap::unvalidated(
            callee_file,
            crate::PositionMap::new("abcdef", "\nabcdef"),
        ));

        assert!(dirty.is_empty());
        assert_eq!(
            index.get_inferred_param(&signature_id(callee_file, 3), 0),
            Some(&LuaType::Integer)
        );
        assert_eq!(
            index.get_inferred_param(&signature_id(callee_file, 5), 0),
            Some(&LuaType::String)
        );
        assert_eq!(
            index.get_inferred_param(&signature_id(callee_file, 2), 0),
            None
        );
    }
    #[test]
    fn unsupported_contribution_blocks_publication_until_retracted() {
        let mut index = CallSiteParamIndex::new();
        let callee = FileId { id: 1 };
        let supported = FileId { id: 2 };
        let unsupported = FileId { id: 3 };
        let signature = signature_id(callee, 0);
        set_call(
            &mut index,
            supported,
            call(0, 10),
            vec![(signature, 0, LuaType::Nil)],
        );
        let before = index.fact_fingerprint(&FactKey::Contrib(signature, 0));
        index.set_call_contributions(unsupported, call(0, 10), vec![(signature, 0, None)]);
        assert!(index.get_inferred_param(&signature, 0).is_none());
        assert_ne!(
            before,
            index.fact_fingerprint(&FactKey::Contrib(signature, 0))
        );
        index.remove(unsupported);
        assert_eq!(index.get_inferred_param(&signature, 0), Some(&LuaType::Nil));
    }

    #[test]
    fn unsupported_and_supported_task_forks_merge_without_publishing_partial_type() {
        use crate::db_index::cow_shards::TaskFork;
        let frozen = CallSiteParamIndex::new();
        let mut supported = frozen.task_fork();
        let mut unsupported = frozen.task_fork();
        let signature = signature_id(FileId { id: 1 }, 0);
        set_call(
            &mut supported,
            FileId { id: 2 },
            call(0, 10),
            vec![(signature, 0, LuaType::Nil)],
        );
        unsupported.set_call_contributions(
            FileId { id: 3 },
            call(0, 10),
            vec![(signature, 0, None)],
        );
        for reverse in [false, true] {
            let mut index = frozen.clone();
            let tasks = if reverse {
                vec![(0, unsupported.clone()), (1, supported.clone())]
            } else {
                vec![(0, supported.clone()), (1, unsupported.clone())]
            };
            assert!(index.install_tasks(&frozen, tasks).is_empty());
            assert!(index.get_inferred_param(&signature, 0).is_none());
        }
    }
}
