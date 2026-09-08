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

    /// Queuing an identical fact is a re-settled duplicate: it reports that
    /// nothing was queued, so the flush never folds the same evidence twice.
    #[test]
    fn queueing_an_identical_contribution_reports_not_queued() {
        let source_file = FileId::new(1);
        let signature_id = signature_id(FileId::new(2), 10);
        let mut index = CallSiteParamIndex::new();
        index.set_files_contributions(vec![(
            source_file,
            vec![(signature_id, 0, LuaType::String)],
        )]);
        let fact = LuaTypeFact::certain(LuaType::Boolean);

        assert!(index.queue_deferred_contribution(source_file, signature_id, 0, fact.clone()));
        assert!(!index.queue_deferred_contribution(source_file, signature_id, 0, fact));
        assert_ne!(
            index.queue_deferred_contribution(
                source_file,
                signature_id,
                1,
                LuaTypeFact::certain(LuaType::Boolean),
            ),
            false,
            "a different slot is a different fact and must queue"
        );
    }

    /// Only the slot whose folded fact changed may bump the contribution
    /// epoch; re-folding identical facts must not bump, or the fixpoint
    /// filter degrades to a full requeue.
    #[test]
    fn flush_ticks_only_the_changed_slot() {
        let source_file = FileId::new(1);
        let changed_id = signature_id(FileId::new(2), 10);
        let unchanged_id = signature_id(FileId::new(2), 20);
        let mut index = CallSiteParamIndex::new();
        index.set_files_fact_contributions(vec![(
            source_file,
            vec![
                (changed_id, 0, LuaTypeFact::certain(LuaType::String)),
                (unchanged_id, 0, LuaTypeFact::certain(LuaType::Integer)),
            ],
        )]);
        // `set_files_fact_contributions` ticks the novel installs; the flush
        // below must tick only the slot it actually changes.
        let writes_before_flush = index.contrib_writes();
        assert!(writes_before_flush > 0);
        let unchanged_before = index.contrib_write_version(&unchanged_id, 0);

        index.queue_deferred_contribution(
            source_file,
            changed_id,
            0,
            LuaTypeFact::certain(LuaType::Boolean),
        );
        index.flush_deferred_contributions();

        assert!(
            index.contrib_write_version(&changed_id, 0) > writes_before_flush,
            "the changed slot must tick"
        );
        assert_eq!(
            index.contrib_write_version(&unchanged_id, 0),
            unchanged_before,
            "re-folding an identical fact must not bump the slot"
        );
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

    fn provenanced_fact(
        typ: LuaType,
        signature_id: LuaSignatureId,
        source_file: FileId,
        start: u32,
    ) -> LuaTypeFact {
        use crate::{
            InFiled, LuaInferenceConfidence, LuaInferenceEventId, LuaInferenceNodeId,
            LuaInferenceProvenanceKind, LuaInferenceStep,
        };
        use glua_parser::{LuaKind, LuaSyntaxId, LuaSyntaxKind};
        use rowan::{TextRange, TextSize};

        let range = TextRange::new(TextSize::from(start), TextSize::from(start + 1));
        let kind: LuaKind = LuaSyntaxKind::LocalName.into();
        let step = LuaInferenceStep {
            event: LuaInferenceEventId {
                node: LuaInferenceNodeId::SignatureParam {
                    signature_id,
                    param_idx: 0,
                },
                kind: LuaInferenceProvenanceKind::ConcreteValue,
                source: InFiled::new(source_file, LuaSyntaxId::new(kind, range)),
            },
            support: std::sync::Arc::from([]),
            inferred_type: None,
            found_type: None,
        };
        LuaTypeFact::new(
            typ,
            LuaInferenceConfidence::Certain,
            std::sync::Arc::from([step]),
        )
    }

    /// Incremental removal re-folds the surviving contributions, so the
    /// inferred params and their diagnostic events match a fresh index built
    /// from the surviving files' inputs.
    #[test]
    fn removal_refolds_provenanced_params_and_events_like_a_fresh_build() {
        let caller_a = FileId::new(1);
        let caller_b = FileId::new(2);
        let callee = FileId::new(3);
        let signature_id = signature_id(callee, 100);

        let mut index = CallSiteParamIndex::new();
        index.set_files_fact_contributions(vec![
            (
                caller_a,
                vec![(
                    signature_id,
                    0,
                    provenanced_fact(LuaType::String, signature_id, caller_a, 10),
                )],
            ),
            (
                caller_b,
                vec![(
                    signature_id,
                    0,
                    provenanced_fact(LuaType::Boolean, signature_id, caller_b, 20),
                )],
            ),
        ]);

        index.remove_files(&[caller_a]);

        let mut expected = CallSiteParamIndex::new();
        expected.set_files_fact_contributions(vec![(
            caller_b,
            vec![(
                signature_id,
                0,
                provenanced_fact(LuaType::Boolean, signature_id, caller_b, 20),
            )],
        )]);

        assert_eq!(index.inferred_params, expected.inferred_params);
        assert_eq!(
            index.inference_events_by_file,
            expected.inference_events_by_file
        );
        assert_eq!(
            index.get_inferred_param(&signature_id, 0),
            Some(&LuaType::Boolean)
        );
        assert!(index.get_inference_events_for_file(caller_a).is_empty());
        assert_eq!(index.get_inference_events_for_file(caller_b).len(), 1);
    }

    fn return_consumer(
        signature_id: LuaSignatureId,
        file_id: FileId,
        start: u32,
    ) -> CallSiteReturnConsumer {
        use crate::LuaDeclId;
        use glua_parser::{LuaKind, LuaSyntaxId, LuaSyntaxKind};
        use rowan::{TextRange, TextSize};

        let range = TextRange::new(TextSize::from(start), TextSize::from(start + 1));
        let kind: LuaKind = LuaSyntaxKind::CallExpr.into();
        CallSiteReturnConsumer {
            signature_id,
            file_id,
            call_syntax_id: LuaSyntaxId::new(kind, range),
            ret_idx: 0,
            target: CallSiteReturnConsumerTarget::Decl(LuaDeclId::new(
                file_id,
                TextSize::from(start),
            )),
            definition: None,
            needs_result_refresh: false,
        }
    }

    /// Removing a consumer file prunes exactly its entries from the derived
    /// return-consumer maps, matching a fresh index built from the surviving
    /// files' consumers. Dependency edges intentionally outlive the removal
    /// (they are append-only until `forget_removed_files`), so they are
    /// excluded from the comparison.
    #[test]
    fn removal_prunes_return_consumers_like_a_fresh_build() {
        let callee = FileId::new(1);
        let consumer_a = FileId::new(2);
        let consumer_b = FileId::new(3);
        let signature_id = signature_id(callee, 100);
        let consumer_a_entry = return_consumer(signature_id, consumer_a, 10);
        let consumer_b_entry = return_consumer(signature_id, consumer_b, 20);

        let mut index = CallSiteParamIndex::new();
        index.set_files_return_consumers(vec![
            (consumer_a, vec![consumer_a_entry.clone()]),
            (consumer_b, vec![consumer_b_entry.clone()]),
        ]);

        index.remove_files(&[consumer_a]);

        let mut expected = CallSiteParamIndex::new();
        expected.set_files_return_consumers(vec![(consumer_b, vec![consumer_b_entry])]);

        assert_eq!(index.file_return_consumers, expected.file_return_consumers);
        assert_eq!(index.return_consumers, expected.return_consumers);
        assert_eq!(
            index.return_consumers_by_signature_file,
            expected.return_consumers_by_signature_file
        );
    }

    /// A remove followed by re-adding the same inputs restores the derived
    /// maps exactly, pinning the incremental removal against the installing
    /// paths rather than against a fresh build.
    #[test]
    fn remove_then_readd_restores_source_signatures_and_consumers() {
        let callee = FileId::new(1);
        let consumer = FileId::new(2);
        let signature_id = signature_id(callee, 100);
        let consumer_entry = return_consumer(signature_id, consumer, 10);
        let source_entry = ("m.f".to_string(), signature_id, Vec::new());

        let mut index = CallSiteParamIndex::new();
        index.set_files_source_signatures(vec![(callee, vec![source_entry.clone()])]);
        index.set_files_return_consumers(vec![(consumer, vec![consumer_entry.clone()])]);
        index.set_files_contributions(vec![(consumer, vec![(signature_id, 0, LuaType::String)])]);

        let snapshot = (
            index.source_signatures_by_path.clone(),
            index.mutated_params.clone(),
            index.return_consumers.clone(),
            index.return_consumers_by_signature_file.clone(),
            index.inferred_params.clone(),
        );

        index.remove_files(&[callee, consumer]);
        index.set_files_source_signatures(vec![(callee, vec![source_entry])]);
        index.set_files_return_consumers(vec![(consumer, vec![consumer_entry])]);
        index.set_files_contributions(vec![(consumer, vec![(signature_id, 0, LuaType::String)])]);

        assert_eq!(index.source_signatures_by_path, snapshot.0);
        assert_eq!(index.mutated_params, snapshot.1);
        assert_eq!(index.return_consumers, snapshot.2);
        assert_eq!(index.return_consumers_by_signature_file, snapshot.3);
        assert_eq!(index.inferred_params, snapshot.4);
    }
}
