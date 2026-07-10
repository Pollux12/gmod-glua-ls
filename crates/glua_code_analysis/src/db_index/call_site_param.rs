use std::collections::HashMap;

use rowan::TextSize;

use super::traits::LuaIndex;
use crate::{FileId, LuaInferenceDiagnosticEvent, LuaSignatureId, LuaType, LuaTypeFact};

#[derive(Debug, Clone)]
struct CallSiteParamContribution {
    signature_id: LuaSignatureId,
    param_idx: usize,
    param_fact: LuaTypeFact,
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

        for file_id in sorted_file_ids(&self.file_contributions) {
            let Some(contributions) = self.file_contributions.get(&file_id) else {
                continue;
            };

            for contribution in contributions {
                self.inferred_params
                    .entry(contribution.signature_id)
                    .or_default()
                    .entry(contribution.param_idx)
                    .and_modify(|current| {
                        let mut provenance = current.provenance().to_vec();
                        provenance.extend_from_slice(contribution.param_fact.provenance());
                        *current = LuaTypeFact::new(
                            LuaType::from_vec(vec![
                                current.typ().clone(),
                                contribution.param_fact.typ().clone(),
                            ]),
                            current
                                .confidence()
                                .max(contribution.param_fact.confidence()),
                            provenance.into(),
                        )
                    })
                    .or_insert_with(|| contribution.param_fact.clone());
            }
        }

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
        self.file_source_signatures.remove(&file_id);
        self.file_contributions.remove(&file_id);

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
}
