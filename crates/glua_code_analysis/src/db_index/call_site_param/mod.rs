mod test;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::path::PathBuf;

use rowan::TextSize;

use super::traits::LuaIndex;
use crate::{
    FileId, InFiled, LuaDeclId, LuaDefinitionId, LuaInferenceConfidence,
    LuaInferenceDiagnosticEvent, LuaInferenceProvenanceKind, LuaInferenceStep, LuaMemberId,
    LuaSignatureId, LuaType, LuaTypeFact,
};

/// A single thing outside a file that the file's call-site inference read.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CallSiteSourceId {
    /// The syntax node a contribution's provenance step came from.
    Node(InFiled<glua_parser::LuaSyntaxId>),
    /// The signature whose return a consumer in this file reads.
    Signature(LuaSignatureId),
}

impl CallSiteSourceId {
    /// The file the source lives in.
    pub fn file_id(&self) -> FileId {
        match self {
            Self::Node(node) => node.file_id,
            Self::Signature(signature_id) => signature_id.get_file_id(),
        }
    }
}

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
    /// The source signatures a re-indexed file had before its removal, kept so
    /// the update can tell which of its signatures merely moved. A signature is
    /// identified by position, so an edit that shifts offsets leaves every
    /// contribution made by a file outside the re-index expansion pointing at a
    /// position no signature occupies any more.
    previous_source_signatures: HashMap<FileId, Vec<(String, LuaSignatureId)>>,
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
    /// Count of call-site-inferred parameter changes, keyed by the callee
    /// slot that holds them. Contributions are queued by the unresolve items
    /// but applied wholesale by the flush/rebuild paths, so without this
    /// counter every queued settle looks like unversioned state and forces a
    /// full fixpoint requeue.
    contrib_writes: u64,
    contrib_write_versions: HashMap<(LuaSignatureId, usize), u64>,
    /// signature → param index → union of all observed types from current file contributions.
    inferred_params: HashMap<LuaSignatureId, HashMap<usize, LuaTypeFact>>,
    /// file declaring a signature → files whose calls have supplied evidence for
    /// it.
    ///
    /// Accumulated rather than rebuilt, like `file_source_dependencies`: an
    /// edit that stops a call from resolving drops the contribution, and an
    /// edge rebuilt from the current contributions would go with it - leaving
    /// nothing to re-analyse the caller with when the edit is taken back out.
    contributor_files_by_signature: HashMap<LuaSignatureId, HashSet<FileId>>,
    /// File declaring a signature -> the signatures of that file that have
    /// contributors, so the file-level view is a lookup rather than a scan.
    contributor_signatures_by_file: HashMap<FileId, HashSet<LuaSignatureId>>,
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
    file_source_dependencies: HashMap<FileId, HashSet<CallSiteSourceId>>,
    source_dependents: HashMap<CallSiteSourceId, HashSet<FileId>>,
    source_file_dependents: HashMap<FileId, HashSet<FileId>>,
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
        let moved = self.moved_signatures(&updates);
        for (file_id, signatures) in updates {
            // Drained per file rather than wholesale: `analyze` runs this pass
            // once per workspace group, so a batch spanning a library and the
            // main workspace reaches here more than once and the later group's
            // parks have to survive the earlier one.
            self.previous_source_signatures.remove(&file_id);
            self.file_source_signatures.insert(file_id, signatures);
        }
        self.rebuild_source_signatures();
        if !moved.is_empty() {
            self.remap_signatures(&moved);
        }
    }

    /// Every signature of `file_id` this index still holds a reference to.
    ///
    /// `remove_files` drops what the file itself contributed, never what a
    /// caller outside the re-index recorded about it, so these are the ids
    /// that survive an edit on the old position.
    pub fn stored_signature_ids_for_file(&self, file_id: FileId) -> HashSet<LuaSignatureId> {
        let mut ids: HashSet<LuaSignatureId> = HashSet::default();
        if let Some(signatures) = self.contributor_signatures_by_file.get(&file_id) {
            ids.extend(signatures.iter().copied());
        }
        for consumer in self
            .return_consumers_by_signature_file
            .get(&file_id)
            .into_iter()
            .flatten()
        {
            ids.insert(consumer.signature_id);
        }
        for (_, contribution) in &self.deferred_contributions {
            if contribution.signature_id.get_file_id() == file_id {
                ids.insert(contribution.signature_id);
            }
        }
        // Parked by `remove_files` before the new index existed, so nothing
        // else will re-home it.
        for (signature_id, _) in self.pending_previous_params.keys() {
            if signature_id.get_file_id() == file_id {
                ids.insert(*signature_id);
            }
        }
        ids
    }

    /// Re-keys every stored reference to a signature the edit moved inside
    /// `remap.file_id`, and drops the ones it destroyed.
    ///
    /// Returns the contributor files whose evidence named a destroyed
    /// signature: their entry is gone and only re-analysing them can rebuild
    /// it. The candidate signatures are looked up through the reverse indexes
    /// rather than scanned - `remove_files` never touches a contribution filed
    /// under a *caller* that is not itself being re-indexed, which is exactly
    /// the set this has to fix.
    pub fn remap_file_signatures(&mut self, remap: &crate::FileRemap) -> HashSet<FileId> {
        let file_id = remap.file_id;
        let candidates = self.stored_signature_ids_for_file(file_id);
        // The re-keying below rebuilds the folded facts wholesale, so the
        // versions tick from a whole-index snapshot rather than a per-slot
        // delta. Cold builds never reach here; this is the incremental path.
        let facts_before = self.snapshot_all_param_facts();

        let mut moved = HashMap::default();
        let mut lost: HashSet<LuaSignatureId> = HashSet::default();
        for signature_id in candidates {
            match remap.signature_id(signature_id) {
                crate::Remap::Moved(new_id) => {
                    if new_id != signature_id {
                        moved.insert(signature_id, new_id);
                    }
                }
                crate::Remap::Unrelated => {}
                crate::Remap::Lost => {
                    lost.insert(signature_id);
                }
            }
        }

        let mut dirty = HashSet::default();
        if !lost.is_empty() {
            for signature_id in &lost {
                if let Some(files) = self.contributor_files_by_signature.remove(signature_id) {
                    dirty.extend(files);
                }
                if let Some(signatures) = self
                    .contributor_signatures_by_file
                    .get_mut(&signature_id.get_file_id())
                {
                    signatures.remove(signature_id);
                }
            }
            for (contributor, contributions) in &mut self.file_contributions {
                let before = contributions.len();
                contributions.retain(|c| !lost.contains(&c.signature_id));
                if contributions.len() != before {
                    dirty.insert(*contributor);
                }
            }
            self.deferred_contributions.retain(|(contributor, c)| {
                let keep = !lost.contains(&c.signature_id);
                if !keep {
                    dirty.insert(*contributor);
                }
                keep
            });
            for (consumer_file, consumers) in &mut self.file_return_consumers {
                let before = consumers.len();
                consumers.retain(|c| !lost.contains(&c.signature_id));
                if consumers.len() != before {
                    dirty.insert(*consumer_file);
                }
            }
            self.pending_previous_params
                .retain(|(signature_id, _), _| !lost.contains(signature_id));
        }

        if !moved.is_empty() {
            self.remap_signatures_inner(&moved);
        }
        if !moved.is_empty() || !lost.is_empty() {
            self.rebuild_derived_state();
            self.rebuild_return_consumers();
            self.rebuild_source_dependents();
            self.tick_changed_contrib_writes(&facts_before);
        }
        dirty.remove(&file_id);
        dirty
    }

    /// Old → new signature id for every source function this batch re-indexed
    /// that kept its access path but changed position.
    ///
    /// Paths repeat when a file defines the same function twice, so a path is
    /// matched positionally within its group, and a group whose size changed is
    /// skipped outright: a definition added or removed makes the pairing a
    /// guess, and a wrong pairing merges one function's call sites into
    /// another's. A group that lost one definition and gained another in the
    /// same edit keeps its size and is still paired positionally, which is as
    /// close as the access path can get without a second key to match on.
    fn moved_signatures(
        &self,
        updates: &[(FileId, Vec<(String, LuaSignatureId, Vec<usize>)>)],
    ) -> HashMap<LuaSignatureId, LuaSignatureId> {
        let mut moved = HashMap::default();
        for (file_id, signatures) in updates {
            let Some(previous) = self.previous_source_signatures.get(file_id) else {
                continue;
            };
            let mut by_path: HashMap<&str, Vec<LuaSignatureId>> = HashMap::default();
            for (path, signature_id) in previous {
                by_path
                    .entry(path.as_str())
                    .or_default()
                    .push(*signature_id);
            }
            let mut current: HashMap<&str, Vec<LuaSignatureId>> = HashMap::default();
            for (path, signature_id, _) in signatures {
                current
                    .entry(path.as_str())
                    .or_default()
                    .push(*signature_id);
            }
            for (path, old_ids) in by_path {
                let Some(new_ids) = current.get(path) else {
                    continue;
                };
                if new_ids.len() != old_ids.len() {
                    continue;
                }
                for (old_id, new_id) in old_ids.into_iter().zip(new_ids) {
                    if old_id != *new_id {
                        moved.insert(old_id, *new_id);
                    }
                }
            }
        }
        moved
    }

    /// Re-keys every stored reference to a signature that moved.
    ///
    /// Only the files the re-index visited rebuild their own contributions; a
    /// contributor outside the expansion keeps the id it recorded, so without
    /// this its evidence is stranded on a position the callee no longer has.
    fn remap_signatures(&mut self, moved: &HashMap<LuaSignatureId, LuaSignatureId>) {
        let facts_before = self.snapshot_all_param_facts();
        self.remap_signatures_inner(moved);
        self.tick_changed_contrib_writes(&facts_before);
    }

    /// The re-keying half of [`Self::remap_signatures`], without the version
    /// ticks, so the file-remap path (which rekeys more than signatures) can
    /// tick once for the whole edit instead of once per re-keying.
    fn remap_signatures_inner(&mut self, moved: &HashMap<LuaSignatureId, LuaSignatureId>) {
        for contributions in self.file_contributions.values_mut() {
            for contribution in contributions {
                if let Some(new_id) = moved.get(&contribution.signature_id) {
                    contribution.signature_id = *new_id;
                }
            }
        }
        for (_, contribution) in &mut self.deferred_contributions {
            if let Some(new_id) = moved.get(&contribution.signature_id) {
                contribution.signature_id = *new_id;
            }
        }
        for consumers in self.file_return_consumers.values_mut() {
            for consumer in consumers {
                if let Some(new_id) = moved.get(&consumer.signature_id) {
                    consumer.signature_id = *new_id;
                }
            }
        }
        let contributors = std::mem::take(&mut self.contributor_files_by_signature);
        self.contributor_signatures_by_file.clear();
        for (signature_id, files) in contributors {
            let signature_id = moved.get(&signature_id).copied().unwrap_or(signature_id);
            self.contributor_signatures_by_file
                .entry(signature_id.get_file_id())
                .or_default()
                .insert(signature_id);
            self.contributor_files_by_signature
                .entry(signature_id)
                .or_default()
                .extend(files);
        }
        for sources in self.file_source_dependencies.values_mut() {
            *sources = sources
                .iter()
                .map(|source| match source {
                    CallSiteSourceId::Signature(signature_id) => match moved.get(signature_id) {
                        Some(new_id) => CallSiteSourceId::Signature(*new_id),
                        None => source.clone(),
                    },
                    CallSiteSourceId::Node(_) => source.clone(),
                })
                .collect();
        }
        let parked = std::mem::take(&mut self.pending_previous_params);
        self.pending_previous_params =
            HashMap::with_capacity_and_hasher(parked.len(), Default::default());
        for ((signature_id, param_idx), fact) in parked {
            match moved.get(&signature_id) {
                // A remapped entry describes the signature now at that
                // position, so it wins over one parked there before the edit
                // moved its owner away. Without the split the winner would be
                // whichever the map happened to yield last.
                Some(new_id) => {
                    self.pending_previous_params
                        .insert((*new_id, param_idx), fact);
                }
                None => {
                    self.pending_previous_params
                        .entry((signature_id, param_idx))
                        .or_insert(fact);
                }
            }
        }
        self.rebuild_derived_state();
        self.rebuild_return_consumers();
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
        let mut affected_params = HashSet::default();
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

        self.tick_changed_contrib_writes(&previous);
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
    ///
    /// Reports whether the fact was queued. An identical fact filed by the
    /// same file is a re-settled duplicate: skipping it keeps the flush from
    /// folding the same evidence twice, which would only grow provenance.
    pub(crate) fn queue_deferred_contribution(
        &mut self,
        file_id: FileId,
        signature_id: LuaSignatureId,
        param_idx: usize,
        param_fact: LuaTypeFact,
    ) -> bool {
        let duplicate = self
            .deferred_contributions
            .iter()
            .map(|(_, contribution)| contribution)
            .chain(self.file_contributions.get(&file_id).into_iter().flatten())
            .any(|contribution| {
                contribution.signature_id == signature_id
                    && contribution.param_idx == param_idx
                    && contribution.param_fact == param_fact
            });
        if duplicate {
            return false;
        }
        self.deferred_contributions.push((
            file_id,
            CallSiteParamContribution {
                signature_id,
                param_idx,
                param_fact,
            },
        ));
        true
    }

    /// Compare it across an operation to tell whether the operation changed
    /// any call-site-inferred parameter fact.
    pub fn contrib_writes(&self) -> u64 {
        self.contrib_writes
    }

    /// Records a call-site-inferred parameter change. Callers bump only for
    /// slots whose folded fact actually changed; re-folding an identical fact
    /// must not bump, or the fixpoint's staleness filter degrades to a full
    /// requeue.
    pub(crate) fn note_contrib_write(&mut self, signature_id: LuaSignatureId, param_idx: usize) {
        self.contrib_writes += 1;
        self.contrib_write_versions
            .insert((signature_id, param_idx), self.contrib_writes);
    }

    pub(crate) fn contrib_write_version(
        &self,
        signature_id: &LuaSignatureId,
        param_idx: usize,
    ) -> u64 {
        self.contrib_write_versions
            .get(&(*signature_id, param_idx))
            .copied()
            .unwrap_or(0)
    }

    /// Bumps the version of every slot whose current fact differs from
    /// `previous`, in a stable slot order so the counter values do not depend
    /// on hash iteration. Comparison-only: the values never feed inference.
    fn tick_changed_contrib_writes(
        &mut self,
        previous: &HashMap<(LuaSignatureId, usize), Option<LuaTypeFact>>,
    ) {
        let mut changed = previous
            .iter()
            .filter_map(|(key, previous_fact)| {
                let current = self.get_inferred_param_fact(&key.0, key.1);
                (previous_fact.as_ref() != current).then_some(*key)
            })
            .collect::<Vec<_>>();
        changed.sort_by_key(|(signature_id, param_idx)| {
            (
                signature_id.get_file_id(),
                signature_id.get_position(),
                *param_idx,
            )
        });
        for (signature_id, param_idx) in changed {
            self.note_contrib_write(signature_id, param_idx);
        }
    }

    /// Every currently folded fact, for snapshotting across the rebuild paths
    /// that hold no per-slot delta of their own.
    fn snapshot_all_param_facts(&self) -> HashMap<(LuaSignatureId, usize), Option<LuaTypeFact>> {
        self.inferred_params
            .iter()
            .flat_map(|(signature_id, params)| {
                params
                    .iter()
                    .map(|(param_idx, fact)| ((*signature_id, *param_idx), Some(fact.clone())))
            })
            .collect()
    }

    /// Apply every queued deferred contribution and rebuild derived state once.
    ///
    /// Returns the signatures whose inferred params actually changed, so the
    /// caller can requeue the returns and consumers derived from them.
    pub(crate) fn flush_deferred_contributions(&mut self) -> HashSet<LuaSignatureId> {
        if self.deferred_contributions.is_empty() {
            return HashSet::default();
        }
        let queued = std::mem::take(&mut self.deferred_contributions);
        let affected = queued
            .iter()
            .map(|(_, contribution)| (contribution.signature_id, contribution.param_idx))
            .collect();
        let previous = self.snapshot_param_facts(affected, HashMap::default());
        for (file_id, contribution) in queued {
            self.file_contributions
                .entry(file_id)
                .or_default()
                .push(contribution);
        }
        self.rebuild_derived_state();
        self.tick_changed_contrib_writes(&previous);
        self.changed_signatures(previous)
    }

    /// The inferred parameter facts of every signature that `file_ids`
    /// supply call-site evidence for.
    pub(crate) fn inferred_params_for_contributor_files(
        &self,
        file_ids: &HashSet<FileId>,
    ) -> HashMap<(LuaSignatureId, usize), LuaType> {
        let mut out = HashMap::default();
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
        crate::db_index::read_set::record_contrib(signature_id, param_idx);
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

    /// The files whose calls supply the call-site param evidence for signatures
    /// declared in `signature_files`.
    ///
    /// A re-index rebuilds only the files it visits, so a contributor left out
    /// keeps evidence derived from - and keyed by - the callee's previous text.
    pub fn collect_contributor_files(&self, signature_files: &HashSet<FileId>) -> Vec<FileId> {
        let mut files = signature_files
            .iter()
            .filter_map(|file_id| self.contributor_signatures_by_file.get(file_id))
            .flatten()
            .filter_map(|signature_id| self.contributor_files_by_signature.get(signature_id))
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        files.sort_unstable();
        files.dedup();
        files
    }

    /// The files whose calls supply the call-site param evidence for
    /// `signatures`.
    pub fn collect_signature_contributor_files(
        &self,
        signatures: &[LuaSignatureId],
    ) -> Vec<FileId> {
        let mut files = signatures
            .iter()
            .filter_map(|signature_id| self.contributor_files_by_signature.get(signature_id))
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        files.sort_unstable();
        files.dedup();
        files
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

    pub fn collect_source_dependents<S: std::hash::BuildHasher>(
        &self,
        source_files: &std::collections::HashSet<FileId, S>,
    ) -> Vec<FileId> {
        let mut dependents = source_files
            .iter()
            .filter_map(|file_id| self.source_file_dependents.get(file_id))
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

    /// The files whose call-site inference read one of `sources`.
    pub fn collect_source_node_dependents(&self, sources: &[CallSiteSourceId]) -> Vec<FileId> {
        let mut dependents = sources
            .iter()
            .filter_map(|source| self.source_dependents.get(source))
            .flatten()
            .copied()
            .collect::<Vec<_>>();
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

    /// Drops the state kept for files that are gone rather than re-indexed.
    ///
    /// Both stores here deliberately outlive a removal - one so an edit's
    /// signature moves can still be matched, the other so a caller stays a
    /// dependent while its contribution is absent. Neither is worth keeping
    /// once the file itself is gone.
    pub fn forget_removed_files(&mut self, file_ids: &HashSet<FileId>) {
        for file_id in file_ids {
            self.previous_source_signatures.remove(file_id);
            if let Some(signatures) = self.contributor_signatures_by_file.remove(file_id) {
                for signature_id in signatures {
                    self.contributor_files_by_signature.remove(&signature_id);
                }
            }
        }
        let contributor_signatures_by_file = &mut self.contributor_signatures_by_file;
        self.contributor_files_by_signature
            .retain(|signature_id, contributors| {
                contributors.retain(|file_id| !file_ids.contains(file_id));
                if contributors.is_empty() {
                    if let Some(signatures) =
                        contributor_signatures_by_file.get_mut(&signature_id.get_file_id())
                    {
                        signatures.remove(signature_id);
                    }
                    return false;
                }
                true
            });
        contributor_signatures_by_file.retain(|_, signatures| !signatures.is_empty());
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
            HashMap::<LuaSignatureId, HashMap<usize, CallSiteParamAccumulator>>::default();

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
                            .insert(CallSiteSourceId::Node(step.event.source.clone()));
                    }
                }
                self.contributor_files_by_signature
                    .entry(contribution.signature_id)
                    .or_default()
                    .insert(file_id);
                self.contributor_signatures_by_file
                    .entry(contribution.signature_id.get_file_id())
                    .or_default()
                    .insert(contribution.signature_id);
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

    fn current_file_source_dependencies(&self, file_id: FileId) -> HashSet<CallSiteSourceId> {
        let contribution_sources = self
            .file_contributions
            .get(&file_id)
            .into_iter()
            .flatten()
            .flat_map(|contribution| contribution.param_fact.provenance())
            .map(|step| CallSiteSourceId::Node(step.event.source.clone()));
        let return_sources = self
            .file_return_consumers
            .get(&file_id)
            .into_iter()
            .flatten()
            .map(|consumer| CallSiteSourceId::Signature(consumer.signature_id));
        contribution_sources
            .chain(return_sources)
            .filter(|source| source.file_id() != file_id)
            .collect()
    }

    fn rebuild_source_dependents(&mut self) {
        self.source_dependents.clear();
        self.source_file_dependents.clear();
        self.source_path_dependents.clear();
        for (consumer_file_id, sources) in &self.file_source_dependencies {
            for source in sources {
                self.source_dependents
                    .entry(source.clone())
                    .or_default()
                    .insert(*consumer_file_id);
                let source_file_id = source.file_id();
                self.source_file_dependents
                    .entry(source_file_id)
                    .or_default()
                    .insert(*consumer_file_id);
                if let Some(path) = self.source_paths.get(&source_file_id) {
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
                            .insert(CallSiteSourceId::Signature(consumer.signature_id));
                    }
                }
            }
        }
        self.rebuild_source_dependents();
    }

    /// Drops `signature_ids` from the source-signature derived maps, the
    /// incremental counterpart of [`Self::rebuild_source_signatures`].
    ///
    /// The full rebuild pushes surviving entries in sorted-file order, so
    /// retaining (which preserves order) over the stored vecs reaches the
    /// same state while visiting only the removed signatures' paths.
    fn prune_source_signatures(&mut self, signature_ids: &HashSet<LuaSignatureId>) {
        if signature_ids.is_empty() {
            return;
        }
        for signature_id in signature_ids {
            self.mutated_params.remove(signature_id);
        }
        let mut emptied_paths = Vec::new();
        for (path, signatures) in self.source_signatures_by_path.iter_mut() {
            signatures.retain(|signature_id| !signature_ids.contains(signature_id));
            if signatures.is_empty() {
                emptied_paths.push(path.clone());
            }
        }
        for path in emptied_paths {
            self.source_signatures_by_path.remove(&path);
        }
    }

    /// Drops `consumers` from the return-consumer derived maps, the
    /// incremental counterpart of [`Self::rebuild_return_consumers`].
    ///
    /// The dependency maps are left alone: both rebuilds that feed them only
    /// ever insert, so removing consumers never shrinks them, and rebuilding
    /// the dependents from the unchanged forward map would be an identity.
    fn prune_return_consumers(&mut self, consumers: &[CallSiteReturnConsumer]) {
        for consumer in consumers {
            if let Some(stored) = self.return_consumers.get_mut(&consumer.signature_id) {
                stored.retain(|existing| existing != consumer);
                if stored.is_empty() {
                    self.return_consumers.remove(&consumer.signature_id);
                }
            }
            let signature_file_id = consumer.signature_id.get_file_id();
            if let Some(stored) = self
                .return_consumers_by_signature_file
                .get_mut(&signature_file_id)
            {
                stored.retain(|existing| existing != consumer);
                if stored.is_empty() {
                    self.return_consumers_by_signature_file
                        .remove(&signature_file_id);
                }
            }
        }
    }

    /// Re-folds exactly the pairs `affected` names from the surviving
    /// contributions, the incremental counterpart of [`Self::rebuild_derived_state`].
    ///
    /// Candidates come from `contributor_files_by_signature`, which accumulates
    /// every file that ever contributed to the signature and so is a superset
    /// of the current contributors; files are visited in sorted order with
    /// their stored contribution order, exactly as the full fold does. The
    /// contributor and dependency maps accumulate rather than rebuild, so they
    /// are left alone, matching what the full rebuild re-inserts.
    fn refold_removed_params(
        &mut self,
        affected: &[(LuaSignatureId, usize)],
        previous_facts: &HashMap<(LuaSignatureId, usize), Option<LuaTypeFact>>,
    ) {
        if affected.is_empty() {
            return;
        }
        let mut affected_signatures = HashSet::default();
        let mut touched_event_files = HashSet::default();
        for (signature_id, param_idx) in affected {
            affected_signatures.insert(*signature_id);
            let mut contributor_files = self
                .contributor_files_by_signature
                .get(signature_id)
                .map(|files| files.iter().copied().collect::<Vec<_>>())
                .unwrap_or_default();
            contributor_files.sort_unstable();

            let mut accumulator: Option<CallSiteParamAccumulator> = None;
            for file_id in contributor_files {
                let Some(contributions) = self.file_contributions.get(&file_id) else {
                    continue;
                };
                for contribution in contributions {
                    if contribution.signature_id != *signature_id
                        || contribution.param_idx != *param_idx
                    {
                        continue;
                    }
                    match &mut accumulator {
                        None => {
                            accumulator =
                                Some(CallSiteParamAccumulator::new(&contribution.param_fact));
                        }
                        Some(current) => current.push(&contribution.param_fact),
                    }
                }
            }
            match accumulator {
                None => {
                    if let Some(params) = self.inferred_params.get_mut(signature_id) {
                        params.remove(param_idx);
                        if params.is_empty() {
                            self.inferred_params.remove(signature_id);
                        }
                    }
                }
                Some(accumulator) => {
                    self.inferred_params
                        .entry(*signature_id)
                        .or_default()
                        .insert(*param_idx, accumulator.finish());
                }
            }

            let old_fact = previous_facts
                .get(&(*signature_id, *param_idx))
                .and_then(Clone::clone);
            let new_fact = self
                .get_inferred_param_fact(signature_id, *param_idx)
                .cloned();
            // The refold above rewrote this pair's fact: readers of the slot
            // go stale exactly when the fact differs.
            if old_fact != new_fact {
                self.note_contrib_write(*signature_id, *param_idx);
            }
            self.refresh_param_events(
                old_fact.as_ref(),
                new_fact.as_ref(),
                &mut touched_event_files,
            );
        }
        let mut touched_files = touched_event_files.into_iter().collect::<Vec<_>>();
        touched_files.sort_unstable();
        let events_by_file = &mut self.inference_events_by_file;
        for file_id in touched_files {
            if let Some(events) = events_by_file.get_mut(&file_id) {
                events.sort_by(|left, right| left.event.stable_cmp(&right.event));
                events.dedup_by(|left, right| left.event == right.event);
            }
        }
        self.inference_events_by_file
            .retain(|_, events| !events.is_empty());
        self.refresh_callback_files(&affected_signatures);
    }

    /// Swaps one pair's old event entries for its new ones. Events are keyed
    /// per source file exactly as [`Self::rebuild_derived_state`] files them;
    /// the caller re-sorts and dedups the touched files afterwards.
    fn refresh_param_events(
        &mut self,
        old_fact: Option<&LuaTypeFact>,
        new_fact: Option<&LuaTypeFact>,
        touched: &mut HashSet<FileId>,
    ) {
        if let Some(old_fact) = old_fact {
            for step in old_fact.provenance() {
                touched.insert(step.event.source.file_id);
                if let Some(events) = self
                    .inference_events_by_file
                    .get_mut(&step.event.source.file_id)
                {
                    events.retain(|existing| {
                        !(existing.event == step.event && existing.fact == *old_fact)
                    });
                }
            }
        }
        if let Some(new_fact) = new_fact {
            for step in new_fact.provenance() {
                touched.insert(step.event.source.file_id);
                self.inference_events_by_file
                    .entry(step.event.source.file_id)
                    .or_default()
                    .push(LuaInferenceDiagnosticEvent {
                        event: step.event.clone(),
                        fact: new_fact.clone(),
                    });
            }
        }
    }

    /// Re-derives [`Self::concrete_structural_callback_files`] membership for
    /// the files owning `signatures`, matching the full rebuild's pure
    /// function of the current inferred params.
    fn refresh_callback_files(&mut self, signatures: &HashSet<LuaSignatureId>) {
        let mut files = signatures
            .iter()
            .map(|signature_id| signature_id.get_file_id())
            .collect::<Vec<_>>();
        files.sort_unstable();
        files.dedup();
        for file_id in files {
            let has_callback = self.inferred_params.iter().any(|(signature_id, params)| {
                signature_id.get_file_id() == file_id
                    && params.values().any(is_concrete_structural_callback_fact)
            });
            if has_callback {
                self.concrete_structural_callback_files.insert(file_id);
            } else {
                self.concrete_structural_callback_files.remove(&file_id);
            }
        }
    }
}

/// Deterministic order for param updates: the folds they rebuild are
/// sensitive to application order through the parked previous facts.
fn param_sort_key(
    (signature_id, param_idx): &(LuaSignatureId, usize),
) -> (FileId, TextSize, usize) {
    (
        signature_id.get_file_id(),
        signature_id.get_position(),
        *param_idx,
    )
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
        // The current facts of the affected pairs, for the event surgery once
        // the contributions are gone. The parked map below may already hold an
        // older fact from a previous removal, so this snapshot is taken
        // separately.
        let mut affected_sorted = affected_params.iter().copied().collect::<Vec<_>>();
        affected_sorted.sort_by_key(param_sort_key);
        let previous_facts = affected_sorted
            .iter()
            .map(|(signature_id, param_idx)| {
                (
                    (*signature_id, *param_idx),
                    self.get_inferred_param_fact(signature_id, *param_idx)
                        .cloned(),
                )
            })
            .collect::<HashMap<_, _>>();
        for (signature_id, param_idx) in &affected_sorted {
            if let Some(fact) = self
                .get_inferred_param_fact(signature_id, *param_idx)
                .cloned()
            {
                self.pending_previous_params
                    .entry((*signature_id, *param_idx))
                    .or_insert(fact);
            }
        }
        self.deferred_contributions
            .retain(|(file_id, _)| !file_ids.contains(file_id));
        let mut removed_source_signatures = HashSet::default();
        let mut removed_consumers = Vec::new();
        for &file_id in file_ids {
            if let Some(signatures) = self.file_source_signatures.remove(&file_id) {
                // The oldest surviving entry is the one the stored ids belong
                // to: a second removal before any update would otherwise record
                // positions the contributions never referred to.
                self.previous_source_signatures
                    .entry(file_id)
                    .or_insert_with(|| {
                        signatures
                            .iter()
                            .map(|(path, signature_id, _)| (path.clone(), *signature_id))
                            .collect()
                    });
                removed_source_signatures.extend(
                    signatures
                        .into_iter()
                        .map(|(_, signature_id, _)| signature_id),
                );
            }
            if let Some(consumers) = self.file_return_consumers.remove(&file_id) {
                removed_consumers.extend(consumers);
            }
            self.file_contributions.remove(&file_id);
        }

        self.prune_source_signatures(&removed_source_signatures);
        self.prune_return_consumers(&removed_consumers);
        self.refold_removed_params(&affected_sorted, &previous_facts);
    }

    fn clear(&mut self) {
        self.file_source_signatures.clear();
        self.previous_source_signatures.clear();
        self.contributor_files_by_signature.clear();
        self.contributor_signatures_by_file.clear();
        self.source_signatures_by_path.clear();
        self.file_contributions.clear();
        self.deferred_contributions.clear();
        self.inferred_params.clear();
        self.contrib_write_versions.clear();
        self.pending_previous_params.clear();
        self.file_return_consumers.clear();
        self.return_consumers.clear();
        self.return_consumers_by_signature_file.clear();
        self.concrete_structural_callback_files.clear();
        self.inference_events_by_file.clear();
        self.file_source_dependencies.clear();
        self.source_dependents.clear();
        self.source_file_dependents.clear();
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
