#[cfg(test)]
mod tests {
    use crate::db_index::call_site_param::*;

    fn signature_id(file_id: FileId, position: u32) -> LuaSignatureId {
        serde_json::from_str(&format!("\"{}|{}\"", file_id.id, position)).unwrap()
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

        let lower_file_contribution = (signature_id, 0, LuaType::String);
        let higher_file_contribution = (signature_id, 0, LuaType::Boolean);

        let mut forward_index = CallSiteParamIndex::new();
        forward_index.set_files_contributions(vec![
            (higher_file_id, vec![higher_file_contribution.clone()]),
            (lower_file_id, vec![lower_file_contribution.clone()]),
        ]);

        let mut reverse_index = CallSiteParamIndex::new();
        reverse_index.set_files_contributions(vec![
            (lower_file_id, vec![lower_file_contribution]),
            (higher_file_id, vec![higher_file_contribution]),
        ]);

        // The union is canonicalised by type rather than by which file
        // contributed first, so the order no longer depends on the contributing
        // set being complete — an incremental reindex that replaces only some
        // files' contributions still produces this exact union.
        let expected = vec![LuaType::Boolean, LuaType::String];
        assert_eq!(
            inferred_union_members(&forward_index, &signature_id),
            expected
        );
        assert_eq!(
            inferred_union_members(&reverse_index, &signature_id),
            expected
        );
    }

    #[test]
    fn batch_removal_matches_rebuilding_with_surviving_file_inputs() {
        let removed_first = FileId::new(1);
        let surviving = FileId::new(2);
        let removed_last = FileId::new(3);
        let removed_first_signature = signature_id(removed_first, 10);
        let surviving_signature = signature_id(surviving, 20);
        let removed_last_signature = signature_id(removed_last, 30);

        let mut index = CallSiteParamIndex::new();
        index.set_files_source_signatures(vec![
            (
                removed_first,
                vec![(
                    "removed-first".to_string(),
                    removed_first_signature,
                    vec![0],
                )],
            ),
            (
                surviving,
                vec![("surviving".to_string(), surviving_signature, vec![1])],
            ),
            (
                removed_last,
                vec![("removed-last".to_string(), removed_last_signature, vec![2])],
            ),
        ]);
        index.set_files_contributions(vec![
            (
                removed_first,
                vec![(surviving_signature, 0, LuaType::String)],
            ),
            (surviving, vec![(surviving_signature, 0, LuaType::Boolean)]),
            (
                removed_last,
                vec![(surviving_signature, 0, LuaType::Integer)],
            ),
        ]);

        index.remove_files(&[removed_last, removed_first]);

        let mut expected = CallSiteParamIndex::new();
        expected.set_files_source_signatures(vec![(
            surviving,
            vec![("surviving".to_string(), surviving_signature, vec![1])],
        )]);
        expected.set_files_contributions(vec![(
            surviving,
            vec![(surviving_signature, 0, LuaType::Boolean)],
        )]);

        assert_eq!(
            index.source_signatures_by_path,
            expected.source_signatures_by_path
        );
        assert_eq!(index.mutated_params, expected.mutated_params);
        assert_eq!(index.inferred_params, expected.inferred_params);
        assert_eq!(
            index.inference_events_by_file,
            expected.inference_events_by_file
        );
    }

    #[test]
    fn removed_contribution_reports_its_signature_as_changed() {
        let source_file = FileId::new(1);
        let signature_id = signature_id(FileId::new(2), 10);
        let mut index = CallSiteParamIndex::new();
        index.set_files_contributions(vec![(
            source_file,
            vec![(signature_id, 0, LuaType::String)],
        )]);

        index.remove(source_file);
        let changed = index.set_files_fact_contributions(vec![(source_file, Vec::new())]);

        assert_eq!(changed, HashSet::from_iter([signature_id]));
    }

    #[test]
    fn flushing_a_deferred_contribution_reports_its_signature_as_changed() {
        let source_file = FileId::new(1);
        let signature_id = signature_id(FileId::new(2), 10);
        let mut index = CallSiteParamIndex::new();
        index.set_files_contributions(vec![(
            source_file,
            vec![(signature_id, 0, LuaType::String)],
        )]);

        index.queue_deferred_contribution(
            source_file,
            signature_id,
            0,
            LuaTypeFact::certain(LuaType::Boolean),
        );

        assert_eq!(
            index.flush_deferred_contributions(),
            HashSet::from_iter([signature_id])
        );
        assert_eq!(
            inferred_union_members(&index, &signature_id),
            vec![LuaType::Boolean, LuaType::String]
        );
    }

    #[test]
    fn flushing_an_empty_queue_reports_no_changed_signatures() {
        let mut index = CallSiteParamIndex::new();

        assert!(index.flush_deferred_contributions().is_empty());
    }

    #[test]
    fn unchanged_reindexed_contribution_does_not_report_a_change() {
        let source_file = FileId::new(1);
        let signature_id = signature_id(FileId::new(2), 10);
        let mut index = CallSiteParamIndex::new();
        index.set_files_contributions(vec![(
            source_file,
            vec![(signature_id, 0, LuaType::String)],
        )]);

        index.remove(source_file);
        let changed = index.set_files_fact_contributions(vec![(
            source_file,
            vec![(signature_id, 0, LuaTypeFact::certain(LuaType::String))],
        )]);

        assert!(changed.is_empty());
    }

    #[test]
    fn a_contribution_follows_the_signature_an_edit_moved() {
        let callee = FileId::new(1);
        let caller = FileId::new(2);
        let before = signature_id(callee, 100);
        let after = signature_id(callee, 101);

        let mut index = CallSiteParamIndex::new();
        index.set_files_source_signatures(vec![(
            callee,
            vec![("m.f".to_string(), before, Vec::new())],
        )]);
        index.set_files_contributions(vec![(caller, vec![(before, 0, LuaType::String)])]);

        // The callee is re-indexed on its own, as an edit outside the caller's
        // expansion re-indexes it: the caller keeps the contribution it made
        // against the pre-edit text.
        index.remove(callee);
        index.set_files_source_signatures(vec![(
            callee,
            vec![("m.f".to_string(), after, Vec::new())],
        )]);

        assert_eq!(index.get_inferred_param(&after, 0), Some(&LuaType::String));
        assert_eq!(index.get_inferred_param(&before, 0), None);
    }

    #[test]
    fn a_path_that_gained_a_definition_is_left_alone() {
        let callee = FileId::new(1);
        let caller = FileId::new(2);
        let before = signature_id(callee, 100);

        let mut index = CallSiteParamIndex::new();
        index.set_files_source_signatures(vec![(
            callee,
            vec![("m.f".to_string(), before, Vec::new())],
        )]);
        index.set_files_contributions(vec![(caller, vec![(before, 0, LuaType::String)])]);

        // Two definitions now share the path, so which one the old id meant is
        // a guess, and guessing wrong merges one function's call sites into
        // another's.
        index.remove(callee);
        index.set_files_source_signatures(vec![(
            callee,
            vec![
                ("m.f".to_string(), signature_id(callee, 101), Vec::new()),
                ("m.f".to_string(), signature_id(callee, 220), Vec::new()),
            ],
        )]);

        assert_eq!(index.get_inferred_param(&before, 0), Some(&LuaType::String));
        assert_eq!(
            index.get_inferred_param(&signature_id(callee, 101), 0),
            None
        );
    }

    #[test]
    fn a_park_survives_the_group_analysed_before_it() {
        let library = FileId::new(1);
        let main = FileId::new(2);
        let caller = FileId::new(3);
        let before = signature_id(main, 100);
        let after = signature_id(main, 101);

        let mut index = CallSiteParamIndex::new();
        index.set_files_source_signatures(vec![
            (
                library,
                vec![("lib.f".to_string(), signature_id(library, 10), Vec::new())],
            ),
            (main, vec![("m.f".to_string(), before, Vec::new())]),
        ]);
        index.set_files_contributions(vec![(caller, vec![(before, 0, LuaType::String)])]);

        // `analyze` runs this pass once per workspace group, so one batch
        // spanning both files installs the library's signatures before the main
        // workspace's. The main file's park has to still be there when its own
        // group arrives.
        index.remove_files(&[library, main]);
        index.set_files_source_signatures(vec![(
            library,
            vec![("lib.f".to_string(), signature_id(library, 10), Vec::new())],
        )]);
        index.set_files_source_signatures(vec![(
            main,
            vec![("m.f".to_string(), after, Vec::new())],
        )]);

        assert_eq!(index.get_inferred_param(&after, 0), Some(&LuaType::String));
        assert_eq!(index.get_inferred_param(&before, 0), None);
    }

    #[test]
    fn a_contributor_stays_a_dependent_after_its_contribution_goes() {
        let callee = FileId::new(1);
        let caller = FileId::new(2);
        let signature_id = signature_id(callee, 100);
        let callee_files = HashSet::from_iter([callee]);

        let mut index = CallSiteParamIndex::new();
        index.set_files_contributions(vec![(caller, vec![(signature_id, 0, LuaType::String)])]);
        assert_eq!(index.collect_contributor_files(&callee_files), vec![caller]);

        // An edit that stops the call from resolving drops the contribution.
        // The caller still has to be re-analysed when that edit is taken back
        // out, so the edge cannot be derived from the contributions alone.
        index.remove(caller);
        index.set_files_contributions(vec![(caller, Vec::new())]);

        assert_eq!(index.get_inferred_param(&signature_id, 0), None);
        assert_eq!(index.collect_contributor_files(&callee_files), vec![caller]);
    }

    #[test]
    fn a_contributor_is_reachable_from_the_signature_and_from_its_file() {
        let callee = FileId::new(1);
        let caller = FileId::new(2);
        let other_id = signature_id(callee, 200);
        let signature_id = signature_id(callee, 100);

        let mut index = CallSiteParamIndex::new();
        index.set_files_contributions(vec![(caller, vec![(signature_id, 0, LuaType::String)])]);

        assert_eq!(
            index.collect_signature_contributor_files(&[signature_id]),
            vec![caller]
        );
        assert!(
            index
                .collect_signature_contributor_files(&[other_id])
                .is_empty()
        );
        assert_eq!(
            index.collect_contributor_files(&HashSet::from_iter([callee])),
            vec![caller]
        );
    }

    #[test]
    fn forgetting_a_removed_callee_clears_both_contributor_views() {
        let callee = FileId::new(1);
        let caller = FileId::new(2);
        let signature_id = signature_id(callee, 100);

        let mut index = CallSiteParamIndex::new();
        index.set_files_contributions(vec![(caller, vec![(signature_id, 0, LuaType::String)])]);
        index.forget_removed_files(&HashSet::from_iter([callee]));

        assert!(
            index
                .collect_signature_contributor_files(&[signature_id])
                .is_empty()
        );
        assert!(
            index
                .collect_contributor_files(&HashSet::from_iter([callee]))
                .is_empty()
        );
    }

    #[test]
    fn forgetting_a_removed_caller_clears_both_contributor_views() {
        let callee = FileId::new(1);
        let caller = FileId::new(2);
        let signature_id = signature_id(callee, 100);

        let mut index = CallSiteParamIndex::new();
        index.set_files_contributions(vec![(caller, vec![(signature_id, 0, LuaType::String)])]);
        index.forget_removed_files(&HashSet::from_iter([caller]));

        assert!(
            index
                .collect_signature_contributor_files(&[signature_id])
                .is_empty()
        );
        assert!(
            index
                .collect_contributor_files(&HashSet::from_iter([callee]))
                .is_empty()
        );
    }
}
