use std::collections::HashMap;

use rowan::TextSize;

use super::traits::LuaIndex;
use crate::{
    FileId, LuaInferenceConfidence, LuaInferenceDiagnosticEvent, LuaInferenceStep, LuaSignatureId,
    LuaType, LuaTypeFact,
};

#[derive(Debug, Clone)]
struct CallSiteParamContribution {
    signature_id: LuaSignatureId,
    param_idx: usize,
    param_fact: LuaTypeFact,
}

struct CallSiteParamAccumulator {
    first_fact: LuaTypeFact,
    additional_types: Vec<LuaType>,
    confidence: LuaInferenceConfidence,
    additional_provenance: Vec<LuaInferenceStep>,
}

impl CallSiteParamAccumulator {
    fn new(fact: &LuaTypeFact) -> Self {
        Self {
            first_fact: fact.clone(),
            additional_types: Vec::new(),
            confidence: fact.confidence(),
            additional_provenance: Vec::new(),
        }
    }

    fn push(&mut self, fact: &LuaTypeFact) {
        self.additional_types.push(fact.typ().clone());
        self.confidence = self.confidence.max(fact.confidence());
        self.additional_provenance
            .extend_from_slice(fact.provenance());
    }

    fn finish(self) -> LuaTypeFact {
        if self.additional_types.is_empty() {
            return self.first_fact;
        }
        let mut types = Vec::with_capacity(self.additional_types.len() + 1);
        types.push(self.first_fact.typ().clone());
        types.extend(self.additional_types);
        let mut provenance = Vec::with_capacity(
            self.first_fact.provenance().len() + self.additional_provenance.len(),
        );
        provenance.extend_from_slice(self.first_fact.provenance());
        provenance.extend(self.additional_provenance);
        LuaTypeFact::new(
            LuaType::from_vec(types),
            self.confidence,
            provenance.into(),
        )
    }
}

fn sorted_file_ids<V>(map: &HashMap<FileId, V>) -> Vec<FileId> {
    let mut file_ids = map.keys().copied().collect::<Vec<_>>();
    file_ids.sort_unstable();
    file_ids
}

#[derive(Debug, Default)]
pub struct CallSiteParamIndex {
    /// file → source function access paths and their mutated parameter indexes declared by that file.
    file_source_signatures: HashMap<FileId, Vec<(String, LuaSignatureId, Vec<usize>)>>,
    /// access path → current source function signature candidates.
    source_signatures_by_path: HashMap<String, Vec<LuaSignatureId>>,
    /// Flat map for fast check: signature_id -> list of mutated parameter indices.
    mutated_params: HashMap<LuaSignatureId, Vec<usize>>,
    /// file → observed call-site param evidence contributed by calls in that file.
    file_contributions: HashMap<FileId, Vec<CallSiteParamContribution>>,
    /// signature → param index → union of all observed types from current file contributions.
    inferred_params: HashMap<LuaSignatureId, HashMap<usize, LuaTypeFact>>,
    inference_events_by_file: HashMap<FileId, Vec<LuaInferenceDiagnosticEvent>>,
}

impl CallSiteParamIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_files_source_signatures(
        &mut self,
        updates: Vec<(FileId, Vec<(String, LuaSignatureId, Vec<usize>)>)>,
    ) {
        for (file_id, signatures) in updates {
            self.file_source_signatures.insert(file_id, signatures);
        }
        self.rebuild_source_signatures();
    }

    pub fn get_source_signature_for_file_at(
        &self,
        path: &str,
        file_id: FileId,
        position: TextSize,
    ) -> Option<LuaSignatureId> {
        self.source_signatures_by_path
            .get(path)?
            .iter()
            .rev()
            .copied()
            .find(|signature_id| {
                signature_id.get_file_id() == file_id && signature_id.get_position() <= position
            })
    }

    pub fn is_param_mutated(&self, signature_id: &LuaSignatureId, param_idx: usize) -> bool {
        self.mutated_params
            .get(signature_id)
            .is_some_and(|mutated| mutated.contains(&param_idx))
    }

    pub fn set_files_contributions(
        &mut self,
        updates: Vec<(FileId, Vec<(LuaSignatureId, usize, LuaType)>)>,
    ) {
        self.set_files_fact_contributions(
            updates
                .into_iter()
                .map(|(file_id, contributions)| {
                    (
                        file_id,
                        contributions
                            .into_iter()
                            .map(|(signature_id, param_idx, typ)| {
                                (signature_id, param_idx, LuaTypeFact::certain(typ))
                            })
                            .collect(),
                    )
                })
                .collect(),
        );
    }

    pub fn set_files_fact_contributions(
        &mut self,
        updates: Vec<(FileId, Vec<(LuaSignatureId, usize, LuaTypeFact)>)>,
    ) {
        for (file_id, contributions) in updates {
            self.file_contributions.insert(
                file_id,
                contributions
                    .into_iter()
                    .map(
                        |(signature_id, param_idx, param_fact)| CallSiteParamContribution {
                            signature_id,
                            param_idx,
                            param_fact,
                        },
                    )
                    .collect(),
            );
        }
        self.rebuild_derived_state();
    }

    pub fn get_inferred_param(
        &self,
        signature_id: &LuaSignatureId,
        param_idx: usize,
    ) -> Option<&LuaType> {
        self.get_inferred_param_fact(signature_id, param_idx)
            .map(LuaTypeFact::typ)
    }

    pub fn get_inferred_param_fact(
        &self,
        signature_id: &LuaSignatureId,
        param_idx: usize,
    ) -> Option<&LuaTypeFact> {
        self.inferred_params
            .get(signature_id)
            .and_then(|params| params.get(&param_idx))
    }

    pub fn get_inference_events_for_file(&self, file_id: FileId) -> &[LuaInferenceDiagnosticEvent] {
        self.inference_events_by_file
            .get(&file_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn rebuild_derived_state(&mut self) {
        self.inferred_params.clear();
        self.inference_events_by_file.clear();

        let mut accumulators =
            HashMap::<LuaSignatureId, HashMap<usize, CallSiteParamAccumulator>>::new();

        for file_id in sorted_file_ids(&self.file_contributions) {
            let Some(contributions) = self.file_contributions.get(&file_id) else {
                continue;
            };

            for contribution in contributions {
                accumulators
                    .entry(contribution.signature_id)
                    .or_default()
                    .entry(contribution.param_idx)
                    .and_modify(|current| current.push(&contribution.param_fact))
                    .or_insert_with(|| CallSiteParamAccumulator::new(&contribution.param_fact));
            }
        }

        self.inferred_params = accumulators
            .into_iter()
            .map(|(signature_id, params)| {
                (
                    signature_id,
                    params
                        .into_iter()
                        .map(|(param_idx, accumulator)| (param_idx, accumulator.finish()))
                        .collect(),
                )
            })
            .collect();

        for params in self.inferred_params.values() {
            for fact in params.values() {
                for step in fact.provenance() {
                    self.inference_events_by_file
                        .entry(step.event.source.file_id)
                        .or_default()
                        .push(LuaInferenceDiagnosticEvent {
                            event: step.event.clone(),
                            fact: fact.clone(),
                        });
                }
            }
        }
        for events in self.inference_events_by_file.values_mut() {
            events.sort_by(|left, right| left.event.stable_cmp(&right.event));
            events.dedup_by(|left, right| left.event == right.event);
        }
    }

    fn rebuild_source_signatures(&mut self) {
        self.source_signatures_by_path.clear();
        self.mutated_params.clear();

        for file_id in sorted_file_ids(&self.file_source_signatures) {
            let Some(signatures) = self.file_source_signatures.get(&file_id) else {
                continue;
            };
            for (path, signature_id, mutated) in signatures {
                self.source_signatures_by_path
                    .entry(path.clone())
                    .or_default()
                    .push(*signature_id);
                if !mutated.is_empty() {
                    self.mutated_params.insert(*signature_id, mutated.clone());
                }
            }
        }
    }
}

impl LuaIndex for CallSiteParamIndex {
    fn remove(&mut self, file_id: FileId) {
        self.remove_files(std::slice::from_ref(&file_id));
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        for &file_id in file_ids {
            self.file_source_signatures.remove(&file_id);
            self.file_contributions.remove(&file_id);
        }

        self.rebuild_derived_state();
        self.rebuild_source_signatures();
    }

    fn clear(&mut self) {
        self.file_source_signatures.clear();
        self.source_signatures_by_path.clear();
        self.file_contributions.clear();
        self.inferred_params.clear();
        self.inference_events_by_file.clear();
        self.mutated_params.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let expected = vec![LuaType::String, LuaType::Boolean];
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
}
