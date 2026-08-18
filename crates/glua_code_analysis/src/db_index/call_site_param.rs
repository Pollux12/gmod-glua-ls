use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use rowan::TextSize;

use super::traits::LuaIndex;
use crate::{
    FileId, LuaDeclId, LuaDefinitionId, LuaInferenceConfidence, LuaInferenceDiagnosticEvent,
    LuaInferenceProvenanceKind, LuaInferenceStep, LuaMemberId, LuaSignatureId, LuaType,
    LuaTypeFact,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallSiteReturnConsumer {
    pub signature_id: LuaSignatureId,
    pub file_id: FileId,
    pub call_syntax_id: glua_parser::LuaSyntaxId,
    pub ret_idx: usize,
    pub target: CallSiteReturnConsumerTarget,
    pub definition: Option<LuaDefinitionId>,
    /// The actual call result has a proven informative later slot, while this
    /// target still holds the earlier single-result cache.
    pub needs_result_refresh: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallSiteReturnConsumerTarget {
    Decl(LuaDeclId),
    Member(LuaMemberId),
}

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
            LuaType::from_inferred_vec(types),
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
    /// Contributions recovered by deferred resolution after this file's batch
    /// collection already ran. Buffered rather than applied directly because
    /// every apply rebuilds the whole derived state; see
    /// `flush_deferred_contributions`.
    deferred_contributions: Vec<(FileId, CallSiteParamContribution)>,
    /// signature → param index → union of all observed types from current file contributions.
    inferred_params: HashMap<LuaSignatureId, HashMap<usize, LuaTypeFact>>,
    pending_previous_params: HashMap<(LuaSignatureId, usize), LuaTypeFact>,
    file_return_consumers: HashMap<FileId, Vec<CallSiteReturnConsumer>>,
    return_consumers: HashMap<LuaSignatureId, Vec<CallSiteReturnConsumer>>,
    return_consumers_by_signature_file: HashMap<FileId, Vec<CallSiteReturnConsumer>>,
    /// Files owning callback parameters inferred from concrete structural table values.
    concrete_structural_callback_files: HashSet<FileId>,
    inference_events_by_file: HashMap<FileId, Vec<LuaInferenceDiagnosticEvent>>,
    /// consumer file → producer files used to infer callback parameter shapes.
    ///
    /// These survive dependent reindexing while a producer is absent so reopening the producer
    /// can invalidate its consumers. Direct consumer edits refresh their entry exactly.
    file_source_dependencies: HashMap<FileId, HashSet<FileId>>,
    source_dependents: HashMap<FileId, HashSet<FileId>>,
    source_paths: HashMap<FileId, PathBuf>,
    source_path_dependents: HashMap<PathBuf, HashSet<FileId>>,
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
        let _ = self.set_files_fact_contributions(
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
    ) -> HashSet<LuaSignatureId> {
        let mut affected_params = HashSet::new();
        for (file_id, _) in &updates {
            if let Some(contributions) = self.file_contributions.get(file_id) {
                affected_params.extend(
                    contributions
                        .iter()
                        .map(|contribution| (contribution.signature_id, contribution.param_idx)),
                );
            }
        }
        affected_params.extend(updates.iter().flat_map(|(_, contributions)| {
            contributions
                .iter()
                .map(|(signature_id, param_idx, _)| (*signature_id, *param_idx))
        }));
        let parked = std::mem::take(&mut self.pending_previous_params)
            .into_iter()
            .map(|(key, fact)| (key, Some(fact)))
            .collect::<HashMap<_, _>>();
        let previous = self.snapshot_param_facts(affected_params, parked);

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

        self.changed_signatures(previous)
    }

    /// The inferred fact each affected param currently holds, keeping whatever
    /// `parked` already recorded for params a removal has since dropped.
    fn snapshot_param_facts(
        &self,
        affected: HashSet<(LuaSignatureId, usize)>,
        mut parked: HashMap<(LuaSignatureId, usize), Option<LuaTypeFact>>,
    ) -> HashMap<(LuaSignatureId, usize), Option<LuaTypeFact>> {
        for (signature_id, param_idx) in affected {
            parked.entry((signature_id, param_idx)).or_insert_with(|| {
                self.get_inferred_param_fact(&signature_id, param_idx)
                    .cloned()
            });
        }
        parked
    }

    /// The signatures whose inferred param facts differ from `previous`.
    fn changed_signatures(
        &self,
        previous: HashMap<(LuaSignatureId, usize), Option<LuaTypeFact>>,
    ) -> HashSet<LuaSignatureId> {
        previous
            .into_iter()
            .filter_map(|((signature_id, param_idx), previous_fact)| {
                let current = self.get_inferred_param_fact(&signature_id, param_idx);
                (previous_fact.as_ref() != current).then_some(signature_id)
            })
            .collect()
    }

    /// Queue one call-site contribution recovered by deferred resolution.
    pub(crate) fn queue_deferred_contribution(
        &mut self,
        file_id: FileId,
        signature_id: LuaSignatureId,
        param_idx: usize,
        param_fact: LuaTypeFact,
    ) {
        self.deferred_contributions.push((
            file_id,
            CallSiteParamContribution {
                signature_id,
                param_idx,
                param_fact,
            },
        ));
    }

    /// Apply every queued deferred contribution and rebuild derived state once.
    ///
    /// Returns the signatures whose inferred params actually changed, so the
    /// caller can requeue the returns and consumers derived from them.
    pub(crate) fn flush_deferred_contributions(&mut self) -> HashSet<LuaSignatureId> {
        if self.deferred_contributions.is_empty() {
            return HashSet::new();
        }
        let queued = std::mem::take(&mut self.deferred_contributions);
        let affected = queued
            .iter()
            .map(|(_, contribution)| (contribution.signature_id, contribution.param_idx))
            .collect();
        let previous = self.snapshot_param_facts(affected, HashMap::new());
        for (file_id, contribution) in queued {
            self.file_contributions
                .entry(file_id)
                .or_default()
                .push(contribution);
        }
        self.rebuild_derived_state();
        self.changed_signatures(previous)
    }

    /// The inferred parameter facts of every signature that `file_ids`
    /// supply call-site evidence for.
    pub(crate) fn inferred_params_for_contributor_files(
        &self,
        file_ids: &HashSet<FileId>,
    ) -> HashMap<(LuaSignatureId, usize), LuaType> {
        let mut out = HashMap::new();
        for file_id in file_ids {
            let Some(contributions) = self.file_contributions.get(file_id) else {
                continue;
            };
            for contribution in contributions {
                let key = (contribution.signature_id, contribution.param_idx);
                if out.contains_key(&key) {
                    continue;
                }
                if let Some(fact) =
                    self.get_inferred_param_fact(&contribution.signature_id, contribution.param_idx)
                {
                    out.insert(key, fact.typ().clone());
                }
            }
        }
        out
    }

    /// Every call-site-inferred parameter type currently indexed.
    pub fn iter_inferred_params(
        &self,
    ) -> impl Iterator<Item = (&LuaSignatureId, usize, &LuaTypeFact)> {
        self.inferred_params
            .iter()
            .flat_map(|(signature_id, params)| {
                params
                    .iter()
                    .map(move |(param_idx, fact)| (signature_id, *param_idx, fact))
            })
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

    pub(crate) fn set_files_return_consumers(
        &mut self,
        updates: Vec<(FileId, Vec<CallSiteReturnConsumer>)>,
    ) {
        for (file_id, consumers) in updates {
            self.file_return_consumers.insert(file_id, consumers);
        }
        self.rebuild_return_consumers();
    }

    pub(crate) fn get_return_consumers(
        &self,
        signature_ids: &HashSet<LuaSignatureId>,
    ) -> Vec<CallSiteReturnConsumer> {
        let mut consumers = signature_ids
            .iter()
            .filter_map(|signature_id| self.return_consumers.get(signature_id))
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        consumers.sort_unstable_by_key(|consumer| {
            (
                consumer.file_id,
                consumer.call_syntax_id.get_range().start(),
                consumer.ret_idx,
            )
        });
        consumers.dedup();
        consumers
    }

    pub(crate) fn get_return_consumers_for_signature_files(
        &self,
        signature_file_ids: &HashSet<FileId>,
    ) -> Vec<CallSiteReturnConsumer> {
        let mut consumers = signature_file_ids
            .iter()
            .filter_map(|file_id| self.return_consumers_by_signature_file.get(file_id))
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        consumers.sort_unstable_by_key(|consumer| {
            (
                consumer.file_id,
                consumer.call_syntax_id.get_range().start(),
                consumer.ret_idx,
            )
        });
        consumers.dedup();
        consumers
    }

    pub fn collect_contribution_signature_files(
        &self,
        source_files: &HashSet<FileId>,
    ) -> Vec<FileId> {
        let mut files = source_files
            .iter()
            .filter_map(|file_id| self.file_contributions.get(file_id))
            .flatten()
            .map(|contribution| contribution.signature_id.get_file_id())
            .collect::<Vec<_>>();
        files.sort_unstable();
        files.dedup();
        files
    }

    pub fn is_concrete_structural_callback_param(
        &self,
        signature_id: &LuaSignatureId,
        param_idx: usize,
    ) -> bool {
        self.get_inferred_param_fact(signature_id, param_idx)
            .is_some_and(is_concrete_structural_callback_fact)
    }

    pub fn has_concrete_structural_callback_params(&self, file_id: FileId) -> bool {
        self.concrete_structural_callback_files.contains(&file_id)
    }

    pub fn get_inference_events_for_file(&self, file_id: FileId) -> &[LuaInferenceDiagnosticEvent] {
        self.inference_events_by_file
            .get(&file_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn collect_source_dependents(&self, source_files: &HashSet<FileId>) -> Vec<FileId> {
        let mut dependents = source_files
            .iter()
            .filter_map(|file_id| self.source_dependents.get(file_id))
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        dependents.extend(
            source_files
                .iter()
                .filter_map(|file_id| self.source_paths.get(file_id))
                .filter_map(|path| self.source_path_dependents.get(path))
                .flatten()
                .copied(),
        );
        dependents.sort_unstable();
        dependents.dedup();
        dependents
    }

    pub fn collect_source_path_dependents<'a>(
        &self,
        source_paths: impl IntoIterator<Item = &'a PathBuf>,
    ) -> Vec<FileId> {
        let mut dependents = source_paths
            .into_iter()
            .filter_map(|path| self.source_path_dependents.get(path))
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        dependents.sort_unstable();
        dependents.dedup();
        dependents
    }

    pub fn record_source_paths(&mut self, paths: impl IntoIterator<Item = (FileId, PathBuf)>) {
        self.source_paths.extend(paths);
        self.rebuild_source_dependents();
    }

    pub fn refresh_file_source_dependencies(&mut self, file_id: FileId) {
        let dependencies = self.current_file_source_dependencies(file_id);
        if dependencies.is_empty() {
            self.file_source_dependencies.remove(&file_id);
        } else {
            self.file_source_dependencies.insert(file_id, dependencies);
        }
        self.rebuild_source_dependents();
    }

    fn rebuild_derived_state(&mut self) {
        self.inferred_params.clear();
        self.concrete_structural_callback_files.clear();
        self.inference_events_by_file.clear();

        let mut accumulators =
            HashMap::<LuaSignatureId, HashMap<usize, CallSiteParamAccumulator>>::new();

        for file_id in sorted_file_ids(&self.file_contributions) {
            let Some(contributions) = self.file_contributions.get(&file_id) else {
                continue;
            };

            for contribution in contributions {
                for step in contribution.param_fact.provenance() {
                    if step.event.source.file_id != file_id {
                        self.file_source_dependencies
                            .entry(file_id)
                            .or_default()
                            .insert(step.event.source.file_id);
                    }
                }
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

        for (signature_id, params) in &self.inferred_params {
            for fact in params.values() {
                if is_concrete_structural_callback_fact(fact) {
                    self.concrete_structural_callback_files
                        .insert(signature_id.get_file_id());
                }
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
        self.rebuild_source_dependents();
    }

    fn current_file_source_dependencies(&self, file_id: FileId) -> HashSet<FileId> {
        let contribution_sources = self
            .file_contributions
            .get(&file_id)
            .into_iter()
            .flatten()
            .flat_map(|contribution| contribution.param_fact.provenance())
            .map(|step| step.event.source.file_id);
        let return_sources = self
            .file_return_consumers
            .get(&file_id)
            .into_iter()
            .flatten()
            .map(|consumer| consumer.signature_id.get_file_id());
        contribution_sources
            .chain(return_sources)
            .filter(|source_file_id| *source_file_id != file_id)
            .collect()
    }

    fn rebuild_source_dependents(&mut self) {
        self.source_dependents.clear();
        self.source_path_dependents.clear();
        for (consumer_file_id, source_file_ids) in &self.file_source_dependencies {
            for source_file_id in source_file_ids {
                self.source_dependents
                    .entry(*source_file_id)
                    .or_default()
                    .insert(*consumer_file_id);
                if let Some(path) = self.source_paths.get(source_file_id) {
                    self.source_path_dependents
                        .entry(path.clone())
                        .or_default()
                        .insert(*consumer_file_id);
                }
            }
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

    fn rebuild_return_consumers(&mut self) {
        self.return_consumers.clear();
        self.return_consumers_by_signature_file.clear();
        for file_id in sorted_file_ids(&self.file_return_consumers) {
            if let Some(consumers) = self.file_return_consumers.get(&file_id) {
                for consumer in consumers {
                    let signature_file_id = consumer.signature_id.get_file_id();
                    self.return_consumers
                        .entry(consumer.signature_id)
                        .or_default()
                        .push(consumer.clone());
                    self.return_consumers_by_signature_file
                        .entry(signature_file_id)
                        .or_default()
                        .push(consumer.clone());
                    if signature_file_id != consumer.file_id {
                        self.file_source_dependencies
                            .entry(consumer.file_id)
                            .or_default()
                            .insert(signature_file_id);
                    }
                }
            }
        }
        self.rebuild_source_dependents();
    }
}

impl LuaIndex for CallSiteParamIndex {
    fn remove(&mut self, file_id: FileId) {
        self.remove_files(std::slice::from_ref(&file_id));
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        let affected_params = file_ids
            .iter()
            .filter_map(|file_id| self.file_contributions.get(file_id))
            .flatten()
            .map(|contribution| (contribution.signature_id, contribution.param_idx))
            .collect::<HashSet<_>>();
        for (signature_id, param_idx) in affected_params {
            if let Some(fact) = self
                .get_inferred_param_fact(&signature_id, param_idx)
                .cloned()
            {
                self.pending_previous_params
                    .entry((signature_id, param_idx))
                    .or_insert(fact);
            }
        }
        self.deferred_contributions
            .retain(|(file_id, _)| !file_ids.contains(file_id));
        for &file_id in file_ids {
            self.file_source_signatures.remove(&file_id);
            self.file_return_consumers.remove(&file_id);
            self.file_contributions.remove(&file_id);
        }

        self.rebuild_derived_state();
        self.rebuild_source_signatures();
        self.rebuild_return_consumers();
    }

    fn clear(&mut self) {
        self.file_source_signatures.clear();
        self.source_signatures_by_path.clear();
        self.file_contributions.clear();
        self.deferred_contributions.clear();
        self.inferred_params.clear();
        self.pending_previous_params.clear();
        self.file_return_consumers.clear();
        self.return_consumers.clear();
        self.return_consumers_by_signature_file.clear();
        self.concrete_structural_callback_files.clear();
        self.inference_events_by_file.clear();
        self.file_source_dependencies.clear();
        self.source_dependents.clear();
        self.source_paths.clear();
        self.source_path_dependents.clear();
        self.mutated_params.clear();
    }
}

fn is_concrete_structural_callback_fact(fact: &LuaTypeFact) -> bool {
    fact.typ().contains_object_type()
        && fact
            .provenance()
            .iter()
            .any(|step| step.event.kind == LuaInferenceProvenanceKind::ConcreteValue)
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

        assert_eq!(changed, HashSet::from([signature_id]));
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
            HashSet::from([signature_id])
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
}
