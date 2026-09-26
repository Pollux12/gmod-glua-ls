#[cfg(test)]
mod tests {
    use crate::db_index::call_site_param::*;

    fn signature_id(file_id: FileId, position: u32) -> LuaSignatureId {
        serde_json::from_str(&format!("\"{}|{}\"", file_id.id, position)).unwrap()
    }

    fn set(
        index: &mut CallSiteParamIndex,
        file_id: FileId,
        contributions: Vec<(LuaSignatureId, usize, LuaType)>,
    ) {
        index.set_file_contributions(
            file_id,
            contributions
                .into_iter()
                .map(|(signature_id, param_idx, typ)| {
                    (signature_id, param_idx, LuaTypeFact::certain(typ))
                })
                .collect(),
        );
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

        assert_eq!(dirty.into_iter().collect::<Vec<_>>(), vec![caller]);
        assert_eq!(
            index.get_inferred_param(&destroyed, 0),
            Some(&LuaType::Integer)
        );
    }
}
