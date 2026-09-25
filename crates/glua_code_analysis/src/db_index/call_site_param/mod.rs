mod test;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::collections::BTreeSet;

use super::traits::LuaIndex;
use crate::db_index::derivation::{self, FactKey, Fingerprint};
use crate::{
    FileId, LuaInferenceConfidence, LuaInferenceDiagnosticEvent, LuaInferenceStep, LuaSignatureId,
    LuaType, LuaTypeFact,
};

#[derive(Debug, Clone, PartialEq, Hash)]
pub struct CallSiteParamContribution {
    pub signature_id: LuaSignatureId,
    pub param_idx: usize,
    pub param_fact: LuaTypeFact,
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

/// Call-site parameter evidence. Each file's `CallSiteParams` derivation
/// replaces that file's evidence ([`FactKey::CallSiteFile`]); a parameter's
/// inferred fact ([`FactKey::Contrib`]) is the fold of every file's evidence
/// for it, in file order.
#[derive(Debug, Default, Clone)]
pub struct CallSiteParamIndex {
    /// file → the evidence its calls contributed. No empty entries.
    file_contributions: HashMap<FileId, Vec<CallSiteParamContribution>>,
    /// parameter → the files contributing evidence for it.
    contributors: HashMap<ParamSlot, BTreeSet<FileId>>,
    /// signature → param index → the folded fact.
    inferred_params: HashMap<LuaSignatureId, HashMap<usize, LuaTypeFact>>,
}

impl CallSiteParamIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the evidence `file_id`'s calls contribute.
    pub fn set_file_contributions(
        &mut self,
        file_id: FileId,
        contributions: Vec<(LuaSignatureId, usize, LuaTypeFact)>,
    ) {
        derivation::note_write(|| FactKey::CallSiteFile(file_id));
        let mut slots = self.take_file(file_id);
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
            self.contributors.entry(slot).or_default().insert(file_id);
            slots.insert(slot);
        }
        if !contributions.is_empty() {
            self.file_contributions.insert(file_id, contributions);
        }
        self.refold(slots);
    }

    /// Drops the evidence `file_id`'s calls contributed.
    pub(crate) fn retract_file(&mut self, file_id: FileId) {
        let slots = self.take_file(file_id);
        self.refold(slots);
    }

    fn take_file(&mut self, file_id: FileId) -> HashSet<ParamSlot> {
        let Some(contributions) = self.file_contributions.remove(&file_id) else {
            return HashSet::default();
        };
        derivation::note_write(|| FactKey::CallSiteFile(file_id));
        let mut slots = HashSet::default();
        for contribution in contributions {
            let slot = (contribution.signature_id, contribution.param_idx);
            if let Some(files) = self.contributors.get_mut(&slot) {
                files.remove(&file_id);
                if files.is_empty() {
                    self.contributors.remove(&slot);
                }
            }
            slots.insert(slot);
        }
        slots
    }

    /// Re-folds `slots` from their contributors' evidence, in file order.
    fn refold(&mut self, slots: HashSet<ParamSlot>) {
        for (signature_id, param_idx) in slots {
            derivation::note_write(|| FactKey::Contrib(signature_id, param_idx));
            let mut accumulator: Option<CallSiteParamAccumulator> = None;
            for file_id in self
                .contributors
                .get(&(signature_id, param_idx))
                .into_iter()
                .flatten()
            {
                for contribution in self.file_contributions[file_id]
                    .iter()
                    .filter(|contribution| {
                        contribution.signature_id == signature_id
                            && contribution.param_idx == param_idx
                    })
                {
                    match &mut accumulator {
                        Some(accumulator) => accumulator.push(&contribution.param_fact),
                        None => {
                            accumulator =
                                Some(CallSiteParamAccumulator::new(&contribution.param_fact))
                        }
                    }
                }
            }
            match accumulator {
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

    /// Observed call-site param evidence contributed by calls in each file.
    pub fn iter_file_contributions(
        &self,
    ) -> impl Iterator<Item = (&FileId, &Vec<CallSiteParamContribution>)> {
        self.file_contributions.iter()
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

    /// The evidence `file_id`'s calls contributed.
    pub(crate) fn file_contributions(&self, file_id: FileId) -> &[CallSiteParamContribution] {
        derivation::note_read(|| FactKey::CallSiteFile(file_id));
        self.file_contributions
            .get(&file_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// The inference events `file_id`'s own evidence carries, each with the
    /// parameter's folded fact.
    pub fn get_inference_events_for_file(
        &self,
        file_id: FileId,
    ) -> Vec<LuaInferenceDiagnosticEvent> {
        let mut events = Vec::new();
        for contribution in self.file_contributions(file_id) {
            let Some(fact) =
                self.get_inferred_param_fact(&contribution.signature_id, contribution.param_idx)
            else {
                continue;
            };
            events.extend(
                contribution
                    .param_fact
                    .provenance()
                    .iter()
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

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::Contrib(signature_id, param_idx) => derivation::fingerprint_opt(
                self.inferred_params
                    .get(signature_id)
                    .and_then(|params| params.get(param_idx)),
            ),
            FactKey::CallSiteFile(file_id) => {
                derivation::fingerprint_opt(self.file_contributions.get(file_id))
            }
            _ => {
                debug_assert!(false, "{key:?} is not a call-site param fact");
                derivation::ABSENT
            }
        }
    }
}

impl LuaIndex for CallSiteParamIndex {
    fn remove(&mut self, file_id: FileId) {
        self.retract_file(file_id);
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
        for file_id in self.file_contributions.keys() {
            derivation::note_write(|| FactKey::CallSiteFile(*file_id));
        }
        self.file_contributions.clear();
        self.contributors.clear();
        self.inferred_params.clear();
    }
}
