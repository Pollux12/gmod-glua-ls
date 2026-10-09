mod flow_node;
mod flow_tree;

use crate::db_index::cow_shards::{CowCell, CowShardSet, CowShards};
use rowan::{TextRange, TextSize};
use rustc_hash::{FxHashMap as HashMap, FxHashSet};

use std::hash::{Hash, Hasher};

use crate::{
    FileId, InFiled, LuaSignatureId, LuaType, VarRefId,
    db_index::{
        LuaOperatorId, LuaOperatorOwner, LuaTypeDeclId,
        derivation::{self, FactKey, Fingerprint},
    },
};
pub use flow_node::*;
pub use flow_tree::{
    AssignmentFlowInfo, AssignmentNameTarget, BranchLabelInfo, FileNarrowingCapability, FlowTree,
};

use super::traits::LuaIndex;

#[derive(Debug, Clone)]
pub struct LuaFlowIndex {
    file_flow_tree: CowCell<HashMap<FileId, FlowTree>>,
    /// [`FlowTree::content_fingerprint`] of each stored tree.
    flow_tree_fingerprints: CowCell<HashMap<FileId, Fingerprint>>,
    special_call_effects: CowShards<FileId, HashMap<TextSize, Vec<LuaSpecialCallEffect>>>,
    /// Every special-call fact installed, keyed canonically.
    /// One call-site can install several facts (a synthesized class plus its
    /// super edge, say), and several call-sites can install facts for the same
    /// class, so neither the call-site nor the installed identity alone tells
    /// a replay from a novel fact: the pair does. Re-settling an identical
    /// fact set hits this record in any visit order and adds nothing.
    settled_special_call_facts: CowShardSet<SpecialCallFact>,
    /// Multiset fingerprint of `settled_special_call_facts` per site.
    special_call_fact_fingerprints: CowShards<(FileId, TextSize), Fingerprint>,
    /// File -> the holders whose special-call effect types name a position in
    /// it.
    special_call_holders: CowShards<FileId, FxHashSet<FileId>>,
}

impl Default for LuaFlowIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaFlowIndex {
    pub fn new() -> Self {
        Self {
            file_flow_tree: Default::default(),
            flow_tree_fingerprints: Default::default(),
            special_call_effects: Default::default(),
            settled_special_call_facts: Default::default(),
            special_call_fact_fingerprints: Default::default(),
            special_call_holders: Default::default(),
        }
    }

    pub fn add_flow_tree(&mut self, file_id: FileId, flow_tree: FlowTree) {
        derivation::note_write(|| FactKey::FlowTree(file_id));
        self.flow_tree_fingerprints
            .insert(file_id, flow_tree.content_fingerprint());
        self.file_flow_tree.insert(file_id, flow_tree);
    }

    pub fn get_flow_tree(&self, file_id: &FileId) -> Option<&FlowTree> {
        derivation::note_file_fact_read(derivation::FileFact::FlowTree, *file_id);
        self.file_flow_tree.get(file_id)
    }

    pub fn flow_trees(&self) -> impl Iterator<Item = (&FileId, &FlowTree)> {
        self.file_flow_tree.iter()
    }

    /// Installs one special-call effect, reporting whether it was new.
    pub fn add_special_call_effect(
        &mut self,
        file_id: FileId,
        position: TextSize,
        target: VarRefId,
        type_ref: LuaType,
    ) -> bool {
        let effects = self
            .special_call_effects
            .entry(file_id)
            .or_default()
            .entry(position)
            .or_default();
        let effect = LuaSpecialCallEffect {
            target: target.clone(),
            type_ref: type_ref.clone(),
        };
        if effects.contains(&effect) {
            return false;
        }
        effects.push(effect);
        for named in crate::db_index::r#type::type_referenced_files(&type_ref) {
            if !self
                .special_call_holders
                .get(&named)
                .is_some_and(|holders| holders.contains(&file_id))
            {
                self.special_call_holders
                    .entry(named)
                    .or_default()
                    .insert(file_id);
            }
        }
        derivation::note_write(|| FactKey::SpecialCall(file_id, position));
        derivation::note_write(|| FactKey::SpecialCallFile(file_id));
        self.install_special_call_facts(
            file_id,
            position,
            [SpecialCallFact::flow_effect(
                file_id, position, target, type_ref,
            )],
        );
        true
    }

    /// Records special-call facts a settle installed, reporting whether any of
    /// them is novel. Replaying an identical fact set in any order adds
    /// nothing.
    pub(crate) fn install_special_call_facts(
        &mut self,
        file_id: FileId,
        position: TextSize,
        facts: impl IntoIterator<Item = SpecialCallFact>,
    ) -> bool {
        let mut novel = false;
        for fact in facts {
            let site = fact.site();
            let fingerprint = derivation::fingerprint_of(&fact);
            if self.settled_special_call_facts.insert(fact) {
                novel = true;
                derivation::note_write(|| FactKey::SpecialCall(site.0, site.1));
                let sum = self.special_call_fact_fingerprints.entry(site).or_default();
                *sum = sum.wrapping_add(fingerprint);
            }
        }
        if novel {
            self.note_special_call_write(file_id, position);
        }
        novel
    }

    /// Notes a novel special-call materialization at `position` in `file_id`.
    pub(crate) fn note_special_call_write(&mut self, file_id: FileId, position: TextSize) {
        derivation::note_write(|| FactKey::SpecialCall(file_id, position));
    }

    /// Rewrites the types stored on special-call effects.
    ///
    /// `type_ref` is the callee's declared out-param type, so it can name a
    /// table literal or signature in a file other than the caller this effect
    /// is filed under. A special call whose type names something the edit
    /// destroyed keeps its stored value and is reported to be settled again
    /// instead.
    pub fn remap_special_call_effect_types(
        &mut self,
        remap: &crate::FileRemap,
    ) -> Vec<crate::LostFact> {
        let mut lost = Vec::new();
        if remap.map.is_identity() {
            return lost;
        }
        let mut holders = self
            .special_call_holders
            .get(&remap.file_id)
            .map(|holders| holders.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        holders.sort_unstable();
        for holder in holders {
            let Some(effects) = self.special_call_effects.get_mut(&holder) else {
                continue;
            };
            for (position, effects) in effects.iter_mut() {
                for effect in effects.iter_mut() {
                    let result = crate::db_index::remap_identities_in_type(&effect.type_ref, remap);
                    if result.lost {
                        lost.push(crate::LostFact::new(
                            holder,
                            FactKey::SpecialCall(holder, *position),
                        ));
                    } else if let Some(typ) = result.typ {
                        effect.type_ref = typ;
                        derivation::note_write(|| FactKey::SpecialCall(holder, *position));
                        derivation::note_write(|| FactKey::FlowTree(holder));
                    }
                }
            }
        }
        lost
    }

    /// The files `holder`'s special-call effect types name.
    fn special_call_files(&self, holder: FileId) -> FxHashSet<FileId> {
        self.effects_in(&holder)
            .flat_map(|(_, effect)| {
                crate::db_index::r#type::type_referenced_files(&effect.type_ref)
            })
            .collect()
    }

    /// Re-files `holder` under the files its effect types name now, given
    /// those `linked` it named before its effects were dropped.
    fn unlink_special_call_holder(&mut self, holder: FileId, linked: FxHashSet<FileId>) {
        let named = self.special_call_files(holder);
        for file_id in linked.difference(&named) {
            if let Some(holders) = self.special_call_holders.get_mut(file_id) {
                holders.remove(&holder);
                if holders.is_empty() {
                    self.special_call_holders.remove(file_id);
                }
            }
        }
    }

    /// Drops what the special call at `position` settled, before the
    /// derivation that settled it runs again.
    pub(crate) fn reset_special_call(&mut self, file_id: FileId, position: TextSize) {
        derivation::note_write(|| FactKey::SpecialCall(file_id, position));
        let linked = self.special_call_files(file_id);
        if let Some(effects) = self.special_call_effects.get_mut(&file_id)
            && effects.remove(&position).is_some()
        {
            derivation::note_write(|| FactKey::SpecialCallFile(file_id));
            if effects.is_empty() {
                self.special_call_effects.remove(&file_id);
            }
            self.unlink_special_call_holder(file_id, linked);
        }
        self.settled_special_call_facts
            .retain(|fact| fact.site() != (file_id, position));
        self.special_call_fact_fingerprints
            .remove(&(file_id, position));
    }

    pub fn get_special_call_effects(
        &self,
        file_id: &FileId,
        position: TextSize,
    ) -> Option<&[LuaSpecialCallEffect]> {
        derivation::note_special_call_read(*file_id, position);
        self.special_call_effects
            .get(file_id)?
            .get(&position)
            .map(|effects| effects.as_slice())
    }

    pub fn special_call_effects(
        &self,
    ) -> impl Iterator<Item = (&FileId, &TextSize, &LuaSpecialCallEffect)> {
        self.special_call_effects
            .iter()
            .flat_map(|(file_id, effects)| {
                effects.iter().flat_map(move |(position, entries)| {
                    entries.iter().map(move |entry| (file_id, position, entry))
                })
            })
    }

    pub fn settled_special_call_facts(&self) -> impl Iterator<Item = &SpecialCallFact> {
        self.settled_special_call_facts.iter()
    }

    /// The special-call effects of `file_id`, with their positions.
    fn effects_in(
        &self,
        file_id: &FileId,
    ) -> impl Iterator<Item = (&TextSize, &LuaSpecialCallEffect)> {
        self.special_call_effects
            .get(file_id)
            .into_iter()
            .flat_map(|effects| {
                effects.iter().flat_map(|(position, effects)| {
                    effects.iter().map(move |effect| (position, effect))
                })
            })
    }

    /// Whether a special-call effect of `file_id` narrows `var_ref_id`: the
    /// flow walk's special calls change nothing else about it.
    pub fn has_special_call_effect_for(&self, file_id: &FileId, var_ref_id: &VarRefId) -> bool {
        derivation::note_read(|| FactKey::SpecialCallFor(*file_id, var_ref_id.clone()));
        self.effects_in(file_id)
            .any(|(_, effect)| effect.narrows(var_ref_id))
    }

    pub fn has_special_call_effect_before(
        &self,
        file_id: &FileId,
        position: TextSize,
        target: &VarRefId,
    ) -> bool {
        derivation::note_read(|| FactKey::SpecialCallFor(*file_id, target.clone()));
        self.effects_in(file_id).any(|(effect_position, effect)| {
            *effect_position < position && &effect.target == target
        })
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::FlowTree(file_id) if derivation::mapping_back() => {
                derivation::fingerprint_opt(
                    self.file_flow_tree
                        .get(file_id)
                        .map(FlowTree::content_fingerprint)
                        .as_ref(),
                )
            }
            FactKey::FlowTree(file_id) => {
                derivation::fingerprint_opt(self.flow_tree_fingerprints.get(file_id))
            }
            FactKey::SpecialCallFile(file_id) => {
                self.special_call_effects
                    .get(file_id)
                    .map_or(derivation::ABSENT, |effects| {
                        effects
                            .iter()
                            .map(|(position, entries)| {
                                derivation::fingerprint_of(&(derivation::Local(position), entries))
                            })
                            .fold(0u64, u64::wrapping_add)
                            .max(1)
                    })
            }
            FactKey::SpecialCallFor(file_id, var_ref_id) => {
                let mut effects = self
                    .effects_in(file_id)
                    .filter(|(_, effect)| effect.may_narrow(var_ref_id))
                    .peekable();
                if effects.peek().is_none() {
                    return derivation::ABSENT;
                }
                effects
                    .map(|(position, effect)| {
                        derivation::fingerprint_of(&(derivation::Local(position), &effect.target))
                    })
                    .fold(0, Fingerprint::wrapping_add)
                    .max(1)
            }
            FactKey::SpecialCall(file_id, position) => {
                let effects = self
                    .special_call_effects
                    .get(file_id)
                    .and_then(|effects| effects.get(position));
                // The sums are folded as the facts were installed; mapped
                // back, each fact hashes as it stood before the edits.
                let facts = if derivation::mapping_back() {
                    self.settled_special_call_facts
                        .iter()
                        .filter(|fact| fact.site() == (*file_id, *position))
                        .map(derivation::fingerprint_of)
                        .fold(0, Fingerprint::wrapping_add)
                } else {
                    self.special_call_fact_fingerprints
                        .get(&(*file_id, *position))
                        .copied()
                        .unwrap_or(0)
                };
                if effects.is_none() && facts == 0 {
                    return derivation::ABSENT;
                }
                derivation::fingerprint_of(&(effects, facts))
            }
            _ => {
                debug_assert!(false, "{key:?} is not a flow fact");
                derivation::ABSENT
            }
        }
    }
}

impl LuaIndex for LuaFlowIndex {
    fn remove(&mut self, file_id: FileId) {
        derivation::note_write(|| FactKey::FlowTree(file_id));
        self.file_flow_tree.remove(&file_id);
        self.flow_tree_fingerprints.remove(&file_id);
        self.settled_special_call_facts
            .retain(|fact| fact.site_file() != file_id);
        self.special_call_fact_fingerprints.retain(|site, _| {
            let keep = site.0 != file_id;
            if !keep {
                derivation::note_write(|| FactKey::SpecialCall(site.0, site.1));
            }
            keep
        });
        // Dropping the effects is a novel fact for their readers, mirroring
        // the signature index's removal tombstones.
        let linked = self.special_call_files(file_id);
        if let Some(by_position) = self.special_call_effects.remove(&file_id) {
            for position in by_position.keys() {
                self.note_special_call_write(file_id, *position);
            }
            self.unlink_special_call_holder(file_id, linked);
        }
    }
    fn clear(&mut self) {
        if derivation::recording() {
            for file_id in self
                .file_flow_tree
                .keys()
                .chain(self.special_call_effects.keys())
            {
                derivation::note_write(|| FactKey::FlowTree(*file_id));
            }
            for (file_id, effects) in &self.special_call_effects {
                for position in effects.keys() {
                    derivation::note_write(|| FactKey::SpecialCall(*file_id, *position));
                }
            }
            for site in self.special_call_fact_fingerprints.keys() {
                derivation::note_write(|| FactKey::SpecialCall(site.0, site.1));
            }
        }
        self.file_flow_tree.clear();
        self.flow_tree_fingerprints.clear();
        self.special_call_fact_fingerprints.clear();
        self.special_call_effects.clear();
        self.settled_special_call_facts.clear();
        self.special_call_holders.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LuaSpecialCallEffect {
    pub target: VarRefId,
    pub type_ref: LuaType,
}

impl LuaSpecialCallEffect {
    /// Whether the effect narrows `var_ref_id`: its target, or a field of the
    /// receiver a `self` target names.
    pub fn narrows(&self, var_ref_id: &VarRefId) -> bool {
        &self.target == var_ref_id
            || matches!(
                (&self.target, var_ref_id),
                (VarRefId::SelfRef(effect_self), VarRefId::IndexRef(root, _))
                    if root.receiver_eq(&effect_self.receiver)
            )
    }

    /// Whether the effect narrows `var_ref_id` under any receiver: a `self`
    /// root's receiver is inferred and not part of the reference's identity,
    /// so [`FactKey::SpecialCallFor`] covers every `self` target for it.
    fn may_narrow(&self, var_ref_id: &VarRefId) -> bool {
        self.narrows(var_ref_id)
            || matches!(
                (&self.target, var_ref_id),
                (
                    VarRefId::SelfRef(_),
                    VarRefId::IndexRef(crate::VarRefRootId::SelfRef(_), _)
                )
            )
    }
}

/// One materialized special-call fact, keyed canonically.
///
/// The key covers every input of the install: the call-site that produced it
/// plus the installed identity (target, type, class, edge, operator and the
/// prefix-derived callable details as applicable). Two settlements that
/// install the same fact build the same key however far apart the pass visits
/// them, so the second is always recognised as a replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecialCallFact {
    /// A `---@param x \`T\`` class synthesized for a call-site argument.
    StrTplDecl {
        site_file: FileId,
        site_position: TextSize,
        class: LuaTypeDeclId,
        constraint: LuaType,
        param_idx: usize,
        is_colon_define: bool,
        is_colon_call: bool,
    },
    /// An inheritance edge installed for a synthesized class (str-tpl or
    /// constructor root edge).
    SuperEdge {
        site_file: FileId,
        site_position: TextSize,
        child: LuaTypeDeclId,
        super_type: LuaType,
        source_file: FileId,
        source_range: TextRange,
    },
    /// A `__call` operator installed for a constructor target.
    CallOperator {
        site_file: FileId,
        site_position: TextSize,
        operator: LuaOperatorId,
        owner: LuaOperatorOwner,
        signature: LuaSignatureId,
        strip_self: bool,
        return_self: bool,
    },
    /// A flow effect published for an out-param, mirroring the stored effect.
    FlowEffect {
        site_file: FileId,
        site_position: TextSize,
        target: VarRefId,
        type_ref: LuaType,
    },
}

/// Positions hash as [`InFiled`] ones, so a fingerprint taken with edits mapped
/// back sees them where they stood before the edits.
impl Hash for SpecialCallFact {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::StrTplDecl {
                site_file,
                site_position,
                class,
                constraint,
                param_idx,
                is_colon_define,
                is_colon_call,
            } => {
                InFiled::new(*site_file, *site_position).hash(state);
                class.hash(state);
                constraint.hash(state);
                param_idx.hash(state);
                is_colon_define.hash(state);
                is_colon_call.hash(state);
            }
            Self::SuperEdge {
                site_file,
                site_position,
                child,
                super_type,
                source_file,
                source_range,
            } => {
                InFiled::new(*site_file, *site_position).hash(state);
                child.hash(state);
                super_type.hash(state);
                InFiled::new(*source_file, *source_range).hash(state);
            }
            Self::CallOperator {
                site_file,
                site_position,
                operator,
                owner,
                signature,
                strip_self,
                return_self,
            } => {
                InFiled::new(*site_file, *site_position).hash(state);
                InFiled::new(operator.file_id, operator.position).hash(state);
                owner.hash(state);
                signature.hash(state);
                strip_self.hash(state);
                return_self.hash(state);
            }
            Self::FlowEffect {
                site_file,
                site_position,
                target,
                type_ref,
            } => {
                InFiled::new(*site_file, *site_position).hash(state);
                target.hash(state);
                type_ref.hash(state);
            }
        }
    }
}

impl SpecialCallFact {
    pub(crate) fn str_tpl_decl(
        site_file: FileId,
        site_position: TextSize,
        class: LuaTypeDeclId,
        constraint: LuaType,
        param_idx: usize,
        is_colon_define: bool,
        is_colon_call: bool,
    ) -> Self {
        Self::StrTplDecl {
            site_file,
            site_position,
            class,
            constraint,
            param_idx,
            is_colon_define,
            is_colon_call,
        }
    }

    pub(crate) fn super_edge(
        site_file: FileId,
        site_position: TextSize,
        child: LuaTypeDeclId,
        super_type: LuaType,
        source_file: FileId,
        source_range: TextRange,
    ) -> Self {
        Self::SuperEdge {
            site_file,
            site_position,
            child,
            super_type,
            source_file,
            source_range,
        }
    }

    pub(crate) fn call_operator(
        site_file: FileId,
        site_position: TextSize,
        operator: LuaOperatorId,
        owner: LuaOperatorOwner,
        signature: LuaSignatureId,
        strip_self: bool,
        return_self: bool,
    ) -> Self {
        Self::CallOperator {
            site_file,
            site_position,
            operator,
            owner,
            signature,
            strip_self,
            return_self,
        }
    }

    pub(crate) fn flow_effect(
        site_file: FileId,
        site_position: TextSize,
        target: VarRefId,
        type_ref: LuaType,
    ) -> Self {
        Self::FlowEffect {
            site_file,
            site_position,
            target,
            type_ref,
        }
    }

    fn site(&self) -> (FileId, TextSize) {
        match self {
            Self::StrTplDecl {
                site_file,
                site_position,
                ..
            }
            | Self::SuperEdge {
                site_file,
                site_position,
                ..
            }
            | Self::CallOperator {
                site_file,
                site_position,
                ..
            }
            | Self::FlowEffect {
                site_file,
                site_position,
                ..
            } => (*site_file, *site_position),
        }
    }

    pub(crate) fn site_file(&self) -> FileId {
        match self {
            Self::StrTplDecl { site_file, .. }
            | Self::SuperEdge { site_file, .. }
            | Self::CallOperator { site_file, .. }
            | Self::FlowEffect { site_file, .. } => *site_file,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effect_target(file_id: FileId) -> VarRefId {
        VarRefId::VarRef(crate::LuaDeclId::new(file_id, TextSize::new(0)))
    }

    /// Re-installing an identical effect reports a duplicate and stores it
    /// once.
    #[test]
    fn reinstalling_identical_special_call_effect_is_a_duplicate() {
        let mut index = LuaFlowIndex::new();
        let file_id = FileId::new(1);
        let position = TextSize::new(16);

        assert!(index.add_special_call_effect(
            file_id,
            position,
            effect_target(file_id),
            LuaType::String,
        ));
        assert!(!index.add_special_call_effect(
            file_id,
            position,
            effect_target(file_id),
            LuaType::String,
        ));
        assert_eq!(
            index
                .get_special_call_effects(&file_id, position)
                .expect("expected the effect")
                .len(),
            1
        );
    }

    /// A reader of whether a file's special calls narrow one reference records
    /// that reference's effects, read out of the file's: the file's first
    /// effect on another reference holds it, one on its own moves it.
    #[test]
    fn a_special_call_effect_on_another_reference_holds_a_reader_of_one() {
        let mut index = LuaFlowIndex::new();
        let file_id = FileId::new(1);
        let read_ref = effect_target(file_id);
        let other_ref = VarRefId::VarRef(crate::LuaDeclId::new(file_id, TextSize::new(8)));
        let _recording = derivation::begin(derivation::DerivationId::workspace(
            derivation::DerivationKind::GmodLoad,
        ));
        assert!(!index.has_special_call_effect_for(&file_id, &read_ref));
        assert!(!index.has_special_call_effect_before(&file_id, TextSize::new(32), &read_ref));
        let reads = derivation::end().into_recorded().2;
        let [read] = reads.as_slice() else {
            panic!("expected one read, got {reads:?}");
        };
        let file_effects = FactKey::SpecialCallFile(file_id);
        assert!(file_effects.is_base());
        assert_eq!(read.base(), Some(file_effects.clone()));
        let fingerprint = |index: &LuaFlowIndex, key: &FactKey| {
            derivation::in_file(Some(file_id), || index.fact_fingerprint(key))
        };
        let seen = fingerprint(&index, read);

        index.add_special_call_effect(file_id, TextSize::new(16), other_ref, LuaType::String);
        assert_ne!(fingerprint(&index, &file_effects), derivation::ABSENT);
        assert_eq!(fingerprint(&index, read), seen);

        index.add_special_call_effect(
            file_id,
            TextSize::new(24),
            read_ref.clone(),
            LuaType::String,
        );
        assert_ne!(fingerprint(&index, read), seen);
        assert!(index.has_special_call_effect_for(&file_id, &read_ref));
        assert!(index.has_special_call_effect_before(&file_id, TextSize::new(32), &read_ref));
    }

    /// A `self` field's key names its root by the method's `self` alone, so
    /// it covers a `self` effect under any receiver: one the reader's
    /// receiver would match moves it whatever receiver the key was made with.
    #[test]
    fn a_self_field_reference_key_covers_self_effects_under_any_receiver() {
        let file_id = FileId::new(1);
        let decl = |position: u32| crate::LuaDeclId::new(file_id, TextSize::new(position));
        let self_ref = |receiver: u32| crate::SelfRefId {
            self_decl_id: decl(4),
            receiver: crate::LuaDeclOrMemberId::Decl(decl(receiver)),
        };
        let field = |receiver: u32| {
            VarRefId::IndexRef(
                crate::VarRefRootId::SelfRef(self_ref(receiver)),
                internment::ArcIntern::new("x".into()),
            )
        };
        let mut index = LuaFlowIndex::new();
        let key = FactKey::SpecialCallFor(file_id, field(0));
        let seen = derivation::in_file(Some(file_id), || index.fact_fingerprint(&key));
        assert_eq!(seen, derivation::ABSENT);

        let effect_self = crate::SelfRefId {
            self_decl_id: decl(12),
            receiver: crate::LuaDeclOrMemberId::Decl(decl(8)),
        };
        index.add_special_call_effect(
            file_id,
            TextSize::new(16),
            VarRefId::SelfRef(effect_self),
            LuaType::String,
        );

        assert!(!index.has_special_call_effect_for(&file_id, &field(0)));
        assert!(index.has_special_call_effect_for(&file_id, &field(8)));
        assert_ne!(
            derivation::in_file(Some(file_id), || index.fact_fingerprint(&key)),
            seen
        );
    }

    /// A reader's key follows a shift of the file its reference is in, and
    /// the effects it covers fingerprint as before once the shift is mapped
    /// back.
    #[test]
    fn a_special_call_reference_key_follows_a_shift_of_its_file() {
        let file_id = FileId::new(1);
        let text = "0123456789abcdefghijklmnopqrstuv";
        let target = |position: u32| {
            VarRefId::VarRef(crate::LuaDeclId::new(file_id, TextSize::new(position)))
        };
        let mut before = LuaFlowIndex::new();
        before.add_special_call_effect(file_id, TextSize::new(16), target(4), LuaType::String);
        let mut after = LuaFlowIndex::new();
        after.add_special_call_effect(file_id, TextSize::new(17), target(5), LuaType::String);
        let edits = derivation::EditMaps::new([(
            file_id,
            crate::PositionMap::new(text, &format!("\n{text}")),
        )]);
        let key = FactKey::SpecialCallFor(file_id, target(4));
        let moved = edits.map_key(&key).expect("the shift loses no position");
        assert_eq!(moved, FactKey::SpecialCallFor(file_id, target(5)));

        let seen = derivation::in_file(Some(file_id), || before.fact_fingerprint(&key));
        let held = derivation::fingerprint_before_edits(&edits, || {
            derivation::in_file(Some(file_id), || after.fact_fingerprint(&moved))
        });

        assert_ne!(seen, derivation::ABSENT);
        assert_eq!(held, seen);
    }

    /// A file's effects that only moved fingerprint as they did before the
    /// move once it is mapped back, so a reader in another file holds.
    #[test]
    fn a_shifted_files_special_call_effects_hold_with_the_shift_mapped_back() {
        let file_id = FileId::new(1);
        let text = "0123456789abcdefghijklmnopqrstuv";
        let key = FactKey::SpecialCallFile(file_id);
        let mut before = LuaFlowIndex::new();
        before.add_special_call_effect(
            file_id,
            TextSize::new(16),
            VarRefId::VarRef(crate::LuaDeclId::new(file_id, TextSize::new(4))),
            LuaType::String,
        );
        let mut after = LuaFlowIndex::new();
        after.add_special_call_effect(
            file_id,
            TextSize::new(17),
            VarRefId::VarRef(crate::LuaDeclId::new(file_id, TextSize::new(5))),
            LuaType::String,
        );
        let edits = derivation::EditMaps::new([(
            file_id,
            crate::PositionMap::new(text, &format!("\n{text}")),
        )]);

        let seen = derivation::in_file(Some(file_id), || before.fact_fingerprint(&key));
        let held = derivation::fingerprint_before_edits(&edits, || {
            derivation::in_file(Some(file_id), || after.fact_fingerprint(&key))
        });

        assert_eq!(held, seen);
    }

    /// A call site's installed facts that only moved fingerprint as they did
    /// before the move once it is mapped back, so a reader in another file
    /// holds.
    #[test]
    fn a_shifted_call_sites_special_call_facts_hold_with_the_shift_mapped_back() {
        let file_id = FileId::new(1);
        let text = "0123456789abcdefghijklmnopqrstuv";
        let mut before = LuaFlowIndex::new();
        before.install_special_call_facts(
            file_id,
            TextSize::new(16),
            [super_edge_fact((file_id, TextSize::new(16)), 4)],
        );
        let mut after = LuaFlowIndex::new();
        after.install_special_call_facts(
            file_id,
            TextSize::new(17),
            [super_edge_fact((file_id, TextSize::new(17)), 5)],
        );
        let edits = derivation::EditMaps::new([(
            file_id,
            crate::PositionMap::new(
                text,
                &format!(
                    "
{text}"
                ),
            ),
        )]);

        let seen = derivation::in_file(Some(file_id), || {
            before.fact_fingerprint(&FactKey::SpecialCall(file_id, TextSize::new(16)))
        });
        let held = derivation::fingerprint_before_edits(&edits, || {
            derivation::in_file(Some(file_id), || {
                after.fact_fingerprint(&FactKey::SpecialCall(file_id, TextSize::new(17)))
            })
        });

        assert_eq!(held, seen);
    }

    fn str_tpl_decl_fact(site: (FileId, TextSize)) -> SpecialCallFact {
        SpecialCallFact::str_tpl_decl(
            site.0,
            site.1,
            crate::LuaTypeDeclId::global("panel_jeep"),
            LuaType::Ref(crate::LuaTypeDeclId::global("Entity")),
            0,
            false,
            false,
        )
    }

    fn super_edge_fact(site: (FileId, TextSize), source_start: u32) -> SpecialCallFact {
        SpecialCallFact::super_edge(
            site.0,
            site.1,
            crate::LuaTypeDeclId::global("panel_jeep"),
            LuaType::Ref(crate::LuaTypeDeclId::global("Entity")),
            site.0,
            TextRange::new(TextSize::new(source_start), TextSize::new(source_start + 8)),
        )
    }

    fn call_operator_fact(site: (FileId, TextSize)) -> SpecialCallFact {
        SpecialCallFact::call_operator(
            site.0,
            site.1,
            crate::db_index::LuaOperatorId::new(TextSize::new(4), FileId::new(9)),
            crate::db_index::LuaOperatorOwner::Type(crate::LuaTypeDeclId::global("panel_jeep")),
            LuaSignatureId::new(FileId::new(9), TextSize::new(4)),
            true,
            true,
        )
    }

    fn flow_effect_fact(site: (FileId, TextSize)) -> SpecialCallFact {
        SpecialCallFact::flow_effect(site.0, site.1, effect_target(site.0), LuaType::String)
    }

    /// Settling the same fact set in two orders reaches the same facts, and
    /// the second pass adds nothing in either order: identical facts always
    /// build identical keys no matter how the pass interleaves the sites.
    #[test]
    fn special_call_facts_settle_identically_in_any_order() {
        let site_a = (FileId::new(1), TextSize::new(16));
        let site_b = (FileId::new(2), TextSize::new(32));
        // Site B materializes the same class from its own argument range, the
        // shape two call-sites of one generic leave behind.
        let pass_a = vec![
            (
                site_a,
                vec![str_tpl_decl_fact(site_a), super_edge_fact(site_a, 20)],
            ),
            (
                site_b,
                vec![
                    super_edge_fact(site_b, 40),
                    flow_effect_fact(site_b),
                    call_operator_fact(site_b),
                ],
            ),
        ];
        let mut pass_b = pass_a.clone();
        pass_b.reverse();

        let mut first_order = LuaFlowIndex::new();
        for ((file_id, position), facts) in &pass_a {
            assert!(first_order.install_special_call_facts(*file_id, *position, facts.clone()));
        }
        // Pass B replays the identical fact set site by site in reverse order:
        // every fact is a duplicate.
        for ((file_id, position), facts) in &pass_b {
            assert!(
                !first_order.install_special_call_facts(*file_id, *position, facts.clone()),
                "replaying an identical fact is a duplicate"
            );
        }

        // A fresh index settled B-first reaches the same fact set.
        let mut second_order = LuaFlowIndex::new();
        for ((file_id, position), facts) in &pass_b {
            assert!(second_order.install_special_call_facts(*file_id, *position, facts.clone()));
        }
        assert_eq!(
            second_order.settled_special_call_facts, first_order.settled_special_call_facts,
            "the fact set must not depend on visit order"
        );
        for ((file_id, position), facts) in &pass_a {
            assert!(
                !second_order.install_special_call_facts(*file_id, *position, facts.clone()),
                "replaying an identical fact is a duplicate"
            );
        }
    }

    /// A duplicate inside one batch is stored once.
    #[test]
    fn duplicate_fact_inside_one_batch_is_stored_once() {
        let mut index = LuaFlowIndex::new();
        let site = (FileId::new(1), TextSize::new(16));
        let fact = flow_effect_fact(site);

        assert!(index.install_special_call_facts(site.0, site.1, [fact.clone(), fact]));
        assert_eq!(index.settled_special_call_facts().count(), 1);
    }

    /// Removing a file evicts its fact records, so reinstalling the same facts
    /// afterwards is novel again: the drop was observable to readers.
    /// `special_call_holders` names exactly the holders a scan of every
    /// effect's type finds.
    fn assert_special_call_holders_match_a_scan(index: &LuaFlowIndex) {
        let mut expected: HashMap<FileId, FxHashSet<FileId>> = HashMap::default();
        for (holder, _, effect) in index.special_call_effects() {
            for file_id in crate::db_index::r#type::type_referenced_files(&effect.type_ref) {
                expected.entry(file_id).or_default().insert(*holder);
            }
        }
        let indexed: HashMap<_, _> = index
            .special_call_holders
            .iter()
            .map(|(file_id, holders)| (*file_id, holders.clone()))
            .collect();
        assert_eq!(indexed, expected);
    }

    #[test]
    fn special_call_holders_follow_adds_resets_remaps_and_removals() {
        let (literal_file, holder) = (FileId::new(1), FileId::new(2));
        let literal = |start: u32| {
            LuaType::TableConst(InFiled::new(
                literal_file,
                TextRange::new(TextSize::new(start), TextSize::new(start + 2)),
            ))
        };
        let (kept, reset) = (TextSize::new(4), TextSize::new(8));
        let mut index = LuaFlowIndex::new();
        index.add_special_call_effect(holder, kept, effect_target(holder), literal(0));
        index.add_special_call_effect(holder, reset, effect_target(holder), literal(6));
        index.add_special_call_effect(literal_file, kept, effect_target(literal_file), literal(3));
        assert_special_call_holders_match_a_scan(&index);

        index.reset_special_call(holder, reset);
        assert_special_call_holders_match_a_scan(&index);

        let dirty = index.remap_special_call_effect_types(&crate::FileRemap::unvalidated(
            literal_file,
            crate::PositionMap::new("abc", "\nabc"),
        ));
        assert!(dirty.is_empty());
        assert_eq!(
            index
                .get_special_call_effects(&holder, kept)
                .expect("the effect")[0]
                .type_ref,
            literal(1)
        );
        assert_special_call_holders_match_a_scan(&index);

        index.remove(holder);
        index.remove(literal_file);
        assert!(index.special_call_holders.is_empty());
    }

    #[test]
    fn removing_file_evicts_its_special_call_facts() {
        let mut index = LuaFlowIndex::new();
        let site = (FileId::new(1), TextSize::new(16));
        assert!(index.install_special_call_facts(site.0, site.1, [str_tpl_decl_fact(site)]));
        assert!(!index.install_special_call_facts(site.0, site.1, [str_tpl_decl_fact(site)]));

        index.remove(site.0);

        assert!(
            index.install_special_call_facts(site.0, site.1, [str_tpl_decl_fact(site)]),
            "a fact dropped with its file must count as novel when rebuilt"
        );
    }
}

crate::db_index::cow_shards::task_fork_fields!(LuaFlowIndex {
    file_flow_tree,
    flow_tree_fingerprints,
    special_call_effects,
    settled_special_call_facts,
    special_call_fact_fingerprints,
    special_call_holders => crate::db_index::cow_shards::merge_set,
});
