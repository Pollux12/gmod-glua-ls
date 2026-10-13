mod test;

use crate::db_index::cow_shards::CowShards;
use rowan::{TextRange, TextSize};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::traits::LuaIndex;
use crate::db_index::derivation::{self, FactKey, Fingerprint};
use crate::db_index::r#type::{remap_type_fact, type_fact_referenced_files};
use crate::{
    FileId, FileRemap, InFiled, LuaInferenceConfidence, LuaInferenceDiagnosticEvent,
    LuaInferenceStep, LuaSignatureId, LuaType, LuaTypeFact, Remap,
};

#[derive(Debug, Clone, PartialEq, Hash)]
pub struct CallSiteParamContribution {
    pub signature_id: LuaSignatureId,
    pub param_idx: usize,
    pub param_fact: Option<LuaTypeFact>,
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

type ParamSlot = (LuaSignatureId, usize);

/// Where a file's evidence comes from: one call, or its callback receivers.
/// Ordered as a walk of the tree meets the calls (by start, the outer of two
/// calls starting together first), the callback receivers last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum EvidenceSource {
    Call(TextSize, Reverse<TextSize>),
    Callbacks,
}

impl EvidenceSource {
    fn call(range: TextRange) -> Self {
        Self::Call(range.start(), Reverse(range.end()))
    }

    fn range(self) -> Option<TextRange> {
        match self {
            Self::Call(start, Reverse(end)) => Some(TextRange::new(start, end)),
            Self::Callbacks => None,
        }
    }

    fn fact_key(self, file_id: FileId) -> FactKey {
        match self.range() {
            Some(range) => FactKey::CallSiteParam(InFiled::new(file_id, range)),
            None => FactKey::CallSiteCallbacks(file_id),
        }
    }
}

/// What one file's sources contributed, and how many of them name an identity
/// of each other file.
#[derive(Debug, Clone, Default, PartialEq)]
struct FileEvidence {
    sources: BTreeMap<EvidenceSource, Vec<CallSiteParamContribution>>,
    named: HashMap<FileId, usize>,
}

/// Call-site parameter evidence. Each call's `CallSiteParam` derivation
/// replaces that call's evidence ([`FactKey::CallSiteParam`]) and a file's
/// `CallSiteParams` derivation its callback receivers'
/// ([`FactKey::CallSiteCallbacks`]); a parameter's inferred fact
/// ([`FactKey::Contrib`]) is the fold of every source's evidence for it, in
/// file order and then source order.
#[derive(Debug, Default, Clone)]
pub struct CallSiteParamIndex {
    /// file → the evidence its sources contributed. No empty entries.
    file_contributions: CowShards<FileId, Arc<FileEvidence>>,
    /// parameter → the sources contributing evidence for it.
    contributors: CowShards<ParamSlot, BTreeSet<(FileId, EvidenceSource)>>,
    /// signature → param index → the folded fact.
    inferred_params: CowShards<LuaSignatureId, HashMap<usize, LuaTypeFact>>,
    /// file → the other files whose evidence names one of its identities: a
    /// callee signature, or a table, signature or node its types and
    /// provenance name.
    referencing: CowShards<FileId, BTreeSet<FileId>>,
}

impl CallSiteParamIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the evidence the call at `range` of `file_id` contributes.
    pub fn set_call_contributions(
        &mut self,
        file_id: FileId,
        range: TextRange,
        contributions: Vec<(LuaSignatureId, usize, Option<LuaTypeFact>)>,
    ) {
        self.set_source(file_id, EvidenceSource::call(range), contributions);
    }

    /// Replaces the evidence the callback receivers of `file_id` contribute.
    pub fn set_callback_contributions(
        &mut self,
        file_id: FileId,
        contributions: Vec<(LuaSignatureId, usize, Option<LuaTypeFact>)>,
    ) {
        self.set_source(file_id, EvidenceSource::Callbacks, contributions);
    }

    fn set_source(
        &mut self,
        file_id: FileId,
        source: EvidenceSource,
        contributions: Vec<(LuaSignatureId, usize, Option<LuaTypeFact>)>,
    ) {
        derivation::note_write(|| source.fact_key(file_id));
        let mut slots = self.take_source(file_id, source);
        if contributions.is_empty() {
            self.refold(slots);
            return;
        }
        derivation::note_write(|| FactKey::CallSiteFile(file_id));
        let contributions = contributions
            .into_iter()
            .map(
                |(signature_id, param_idx, param_fact)| CallSiteParamContribution {
                    signature_id,
                    param_idx,
                    param_fact,
                },
            )
            .collect::<Vec<_>>();
        for contribution in &contributions {
            let slot = (contribution.signature_id, contribution.param_idx);
            self.contributors
                .entry(slot)
                .or_default()
                .insert((file_id, source));
            slots.insert(slot);
        }
        let held = Arc::make_mut(self.file_contributions.entry(file_id).or_default());
        let mut linked = Vec::new();
        for named in referenced_files(file_id, &contributions) {
            let count = held.named.entry(named).or_default();
            *count += 1;
            if *count == 1 {
                linked.push(named);
            }
        }
        held.sources.insert(source, contributions);
        self.relink(file_id, linked, Vec::new());
        self.refold(slots);
    }

    /// Drops the evidence the call at `range` of `file_id` contributed,
    /// before the derivation that collected it runs again.
    pub(crate) fn retract_call(&mut self, file_id: FileId, range: TextRange) {
        self.retract_source(file_id, EvidenceSource::call(range));
    }

    /// Drops the evidence the callback receivers of `file_id` contributed.
    pub(crate) fn retract_callbacks(&mut self, file_id: FileId) {
        self.retract_source(file_id, EvidenceSource::Callbacks);
    }

    fn retract_source(&mut self, file_id: FileId, source: EvidenceSource) {
        derivation::note_write(|| source.fact_key(file_id));
        let slots = self.take_source(file_id, source);
        self.refold(slots);
    }

    /// Re-files `holder` under the files its evidence came to name and no
    /// longer names.
    fn relink(&mut self, holder: FileId, linked: Vec<FileId>, unlinked: Vec<FileId>) {
        for file_id in unlinked {
            if let Some(holders) = self.referencing.get_mut(&file_id) {
                holders.remove(&holder);
                if holders.is_empty() {
                    self.referencing.remove(&file_id);
                }
            }
        }
        for file_id in linked {
            self.referencing.entry(file_id).or_default().insert(holder);
        }
    }

    /// Takes `source`'s evidence out of `file_id`'s, returning the slots it
    /// contributed to.
    fn take_source(&mut self, file_id: FileId, source: EvidenceSource) -> HashSet<ParamSlot> {
        let Some(held) = self.file_contributions.get_mut(&file_id) else {
            return HashSet::default();
        };
        if !held.sources.contains_key(&source) {
            return HashSet::default();
        }
        derivation::note_write(|| FactKey::CallSiteFile(file_id));
        let held = Arc::make_mut(held);
        let contributions = held.sources.remove(&source).unwrap_or_default();
        let mut unlinked = Vec::new();
        for named in referenced_files(file_id, &contributions) {
            let Some(count) = held.named.get_mut(&named) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                held.named.remove(&named);
                unlinked.push(named);
            }
        }
        if held.sources.is_empty() {
            self.file_contributions.remove(&file_id);
        }
        self.relink(file_id, Vec::new(), unlinked);
        self.unfile(file_id, source, &contributions)
    }

    /// Takes every source of `file_id` out, returning the slots they
    /// contributed to.
    fn take_file(&mut self, file_id: FileId) -> HashSet<ParamSlot> {
        let Some(held) = self.file_contributions.remove(&file_id) else {
            return HashSet::default();
        };
        derivation::note_write(|| FactKey::CallSiteFile(file_id));
        self.relink(file_id, Vec::new(), held.named.keys().copied().collect());
        let mut slots = HashSet::default();
        for (source, contributions) in &held.sources {
            derivation::note_write(|| source.fact_key(file_id));
            slots.extend(self.unfile(file_id, *source, contributions));
        }
        slots
    }

    /// Drops `source` of `file_id` from the contributors of the slots its
    /// `contributions` filled, returning them.
    fn unfile(
        &mut self,
        file_id: FileId,
        source: EvidenceSource,
        contributions: &[CallSiteParamContribution],
    ) -> HashSet<ParamSlot> {
        let mut slots = HashSet::default();
        for contribution in contributions {
            let slot = (contribution.signature_id, contribution.param_idx);
            if let Some(sources) = self.contributors.get_mut(&slot) {
                sources.remove(&(file_id, source));
                if sources.is_empty() {
                    self.contributors.remove(&slot);
                }
            }
            slots.insert(slot);
        }
        slots
    }

    /// The slots of each signature whose folded parameters both `self` and
    /// `fork`, a task fork of `frozen`, changed: the merged contributors
    /// re-fold them.
    fn slots_to_refold(&self, frozen: &Self, fork: &Self) -> HashSet<ParamSlot> {
        self.inferred_params
            .both_changed_keys(&frozen.inferred_params, &fork.inferred_params)
            .into_iter()
            .flat_map(|signature_id| {
                [self, frozen, fork]
                    .into_iter()
                    .filter_map(move |index| index.inferred_params.get(&signature_id))
                    .flat_map(|params| params.keys().copied())
                    .map(move |param_idx| (signature_id, param_idx))
            })
            .chain(
                self.contributors
                    .both_changed_keys(&frozen.contributors, &fork.contributors),
            )
            .collect()
    }

    /// Re-folds `slots` from their contributors' evidence, in file order and
    /// then source order.
    fn refold(&mut self, slots: HashSet<ParamSlot>) {
        for (signature_id, param_idx) in slots {
            derivation::note_write(|| FactKey::Contrib(signature_id, param_idx));
            let mut accumulator: Option<CallSiteParamAccumulator> = None;
            let mut complete = true;
            for (file_id, source) in self
                .contributors
                .get(&(signature_id, param_idx))
                .into_iter()
                .flatten()
            {
                for contribution in self.file_contributions[file_id].sources[source]
                    .iter()
                    .filter(|contribution| {
                        contribution.signature_id == signature_id
                            && contribution.param_idx == param_idx
                    })
                {
                    let Some(fact) = &contribution.param_fact else {
                        complete = false;
                        continue;
                    };
                    match &mut accumulator {
                        Some(accumulator) => accumulator.push(fact),
                        None => accumulator = Some(CallSiteParamAccumulator::new(fact)),
                    }
                }
            }
            match accumulator.filter(|_| complete) {
                Some(accumulator) => {
                    self.inferred_params
                        .entry(signature_id)
                        .or_default()
                        .insert(param_idx, accumulator.finish());
                }
                None => {
                    if let Some(params) = self.inferred_params.get_mut(&signature_id) {
                        params.remove(&param_idx);
                        if params.is_empty() {
                            self.inferred_params.remove(&signature_id);
                        }
                    }
                }
            }
        }
    }

    /// Carries the evidence other files contributed through the edited file's
    /// moved identities: a callee signature, and whatever its types and
    /// provenance name. Returns the evidence of each source that named an
    /// identity the edit destroyed; it stays as it is until it is computed
    /// again.
    pub fn remap_file_identities(&mut self, remap: &FileRemap) -> Vec<crate::LostFact> {
        let mut dirty = Vec::new();
        let mut holders = self
            .referencing
            .get(&remap.file_id)
            .map(|files| files.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        holders.sort_unstable();
        for holder in holders {
            let Some(held) = self.file_contributions.get(&holder) else {
                continue;
            };
            let mut moved_sources = Vec::new();
            for (source, contributions) in &held.sources {
                let mut lost = false;
                let mut moved = false;
                let remapped = contributions
                    .iter()
                    .map(|contribution| {
                        let signature_id = match remap.signature_id(contribution.signature_id) {
                            Remap::Moved(signature_id) => {
                                moved |= signature_id != contribution.signature_id;
                                signature_id
                            }
                            Remap::Unrelated => contribution.signature_id,
                            Remap::Lost => {
                                lost = true;
                                contribution.signature_id
                            }
                        };
                        let param_fact = contribution.param_fact.as_ref().map(|fact| {
                            remap_type_fact(fact, remap, &mut lost)
                                .inspect(|_| moved = true)
                                .unwrap_or_else(|| fact.clone())
                        });
                        (signature_id, contribution.param_idx, param_fact)
                    })
                    .collect::<Vec<_>>();
                if lost {
                    dirty.push(crate::LostFact::new(holder, source.fact_key(holder)));
                } else if moved {
                    moved_sources.push((*source, remapped));
                }
            }
            for (source, remapped) in moved_sources {
                self.set_source(holder, source, remapped);
            }
        }
        dirty
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

    /// Observed call-site param evidence contributed by each file's sources,
    /// in source order.
    pub fn iter_file_contributions(
        &self,
    ) -> impl Iterator<Item = (&FileId, Vec<&CallSiteParamContribution>)> {
        self.file_contributions
            .iter()
            .map(|(file_id, held)| (file_id, held.sources.values().flatten().collect()))
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
        derivation::note_read(|| FactKey::Contrib(*signature_id, param_idx));
        self.inferred_params
            .get(signature_id)
            .and_then(|params| params.get(&param_idx))
    }

    /// The inference events `file_id`'s own evidence carries, each with the
    /// parameter's folded fact.
    pub fn get_inference_events_for_file(
        &self,
        file_id: FileId,
    ) -> Vec<LuaInferenceDiagnosticEvent> {
        derivation::note_read(|| FactKey::CallSiteFile(file_id));
        let mut events = Vec::new();
        let Some(held) = self.file_contributions.get(&file_id) else {
            return events;
        };
        for contribution in held.sources.values().flatten() {
            let Some(fact) =
                self.get_inferred_param_fact(&contribution.signature_id, contribution.param_idx)
            else {
                continue;
            };
            events.extend(
                contribution
                    .param_fact
                    .iter()
                    .flat_map(|fact| fact.provenance())
                    .filter(|step| step.event.source.file_id == file_id)
                    .map(|step| LuaInferenceDiagnosticEvent {
                        event: step.event.clone(),
                        fact: fact.clone(),
                    }),
            );
        }
        events.sort_by(|left, right| left.event.stable_cmp(&right.event));
        events.dedup_by(|left, right| left.event == right.event);
        events
    }

    fn source_contributions(
        &self,
        file_id: FileId,
        source: EvidenceSource,
    ) -> Option<&Vec<CallSiteParamContribution>> {
        self.file_contributions.get(&file_id)?.sources.get(&source)
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::Contrib(signature_id, param_idx) => derivation::fingerprint_opt(
                self.inferred_params
                    .get(signature_id)
                    .and_then(|params| params.get(param_idx)),
            ),
            FactKey::CallSiteParam(site) => derivation::fingerprint_opt(
                self.source_contributions(site.file_id, EvidenceSource::call(site.value)),
            ),
            FactKey::CallSiteCallbacks(file_id) => derivation::fingerprint_opt(
                self.source_contributions(*file_id, EvidenceSource::Callbacks),
            ),
            FactKey::CallSiteFile(file_id) => self
                .file_contributions
                .get(file_id)
                .map_or(derivation::ABSENT, |held| {
                    evidence_fingerprint(*file_id, &held.sources)
                }),
            _ => {
                debug_assert!(false, "{key:?} is not a call-site param fact");
                derivation::ABSENT
            }
        }
    }
}

/// A file's sources as one fingerprint, independent of their order.
fn evidence_fingerprint(
    file_id: FileId,
    sources: &BTreeMap<EvidenceSource, Vec<CallSiteParamContribution>>,
) -> Fingerprint {
    derivation::in_file(Some(file_id), || {
        sources
            .iter()
            .map(|(source, contributions)| match source.range() {
                Some(range) => {
                    derivation::fingerprint_of(&(derivation::Local(&range), contributions))
                }
                None => derivation::fingerprint_of(contributions),
            })
            .fold(0u64, u64::wrapping_add)
            .max(1)
    })
}

impl LuaIndex for CallSiteParamIndex {
    fn remove(&mut self, file_id: FileId) {
        let slots = self.take_file(file_id);
        self.refold(slots);
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        let mut slots = HashSet::default();
        for file_id in file_ids {
            slots.extend(self.take_file(*file_id));
        }
        self.refold(slots);
    }

    fn clear(&mut self) {
        for (signature_id, params) in &self.inferred_params {
            for param_idx in params.keys() {
                derivation::note_write(|| FactKey::Contrib(*signature_id, *param_idx));
            }
        }
        for (file_id, held) in self.file_contributions.iter() {
            derivation::note_write(|| FactKey::CallSiteFile(*file_id));
            for source in held.sources.keys() {
                derivation::note_write(|| source.fact_key(*file_id));
            }
        }
        self.file_contributions.clear();
        self.contributors.clear();
        self.inferred_params.clear();
        self.referencing.clear();
    }
}

/// The files other than `file_id` that its evidence names.
fn referenced_files(
    file_id: FileId,
    contributions: &[CallSiteParamContribution],
) -> HashSet<FileId> {
    let mut files = HashSet::default();
    for contribution in contributions {
        files.insert(contribution.signature_id.get_file_id());
        if let Some(fact) = &contribution.param_fact {
            files.extend(type_fact_referenced_files(fact));
        }
    }
    files.remove(&file_id);
    files
}

crate::db_index::cow_shards::task_fork_fields!(CallSiteParamIndex {
    file_contributions,
    contributors => crate::db_index::cow_shards::merge_set,
    inferred_params => crate::db_index::cow_shards::rebuilt,
    referencing => crate::db_index::cow_shards::merge_set,
} prepare slots_to_refold then refold);
