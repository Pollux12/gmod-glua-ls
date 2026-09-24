mod flow_node;
mod flow_tree;

use rowan::{TextRange, TextSize};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::{
    FileId, LuaSignatureId, LuaType, VarRefId,
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
    file_flow_tree: HashMap<FileId, FlowTree>,
    /// [`FlowTree::content_fingerprint`] of each stored tree.
    flow_tree_fingerprints: HashMap<FileId, Fingerprint>,
    special_call_effects: HashMap<FileId, HashMap<TextSize, Vec<LuaSpecialCallEffect>>>,
    /// Every special-call fact installed, keyed canonically.
    /// One call-site can install several facts (a synthesized class plus its
    /// super edge, say), and several call-sites can install facts for the same
    /// class, so neither the call-site nor the installed identity alone tells
    /// a replay from a novel fact: the pair does. Re-settling an identical
    /// fact set hits this record in any visit order and adds nothing.
    settled_special_call_facts: HashSet<SpecialCallFact>,
    /// Multiset fingerprint of `settled_special_call_facts` per site.
    special_call_fact_fingerprints: HashMap<(FileId, TextSize), Fingerprint>,
}

impl Default for LuaFlowIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaFlowIndex {
    pub fn new() -> Self {
        Self {
            file_flow_tree: HashMap::default(),
            flow_tree_fingerprints: HashMap::default(),
            special_call_effects: HashMap::default(),
            settled_special_call_facts: HashSet::default(),
            special_call_fact_fingerprints: HashMap::default(),
        }
    }

    pub fn add_flow_tree(&mut self, file_id: FileId, flow_tree: FlowTree) {
        derivation::note_write(|| FactKey::FlowTree(file_id));
        self.flow_tree_fingerprints
            .insert(file_id, flow_tree.content_fingerprint());
        self.file_flow_tree.insert(file_id, flow_tree);
    }

    pub fn get_flow_tree(&self, file_id: &FileId) -> Option<&FlowTree> {
        derivation::note_read(|| FactKey::FlowTree(*file_id));
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
    /// is filed under. A holder whose type names something the edit destroyed
    /// keeps its stored value and is reported dirty for re-analysis instead.
    pub fn remap_special_call_effect_types(
        &mut self,
        remap: &crate::FileRemap,
    ) -> rustc_hash::FxHashSet<FileId> {
        if remap.map.is_identity() {
            return rustc_hash::FxHashSet::default();
        }
        let mut dirty = rustc_hash::FxHashSet::default();
        for (holder, effects) in self.special_call_effects.iter_mut() {
            for (position, effects) in effects.iter_mut() {
                for effect in effects.iter_mut() {
                    let result = crate::db_index::remap_identities_in_type(&effect.type_ref, remap);
                    if result.lost {
                        dirty.insert(*holder);
                    } else if let Some(typ) = result.typ {
                        effect.type_ref = typ;
                        derivation::note_write(|| FactKey::SpecialCall(*holder, *position));
                        derivation::note_write(|| FactKey::FlowTree(*holder));
                    }
                }
            }
        }
        dirty
    }

    /// Drops what the special call at `position` settled, before the
    /// derivation that settled it runs again.
    pub(crate) fn reset_special_call(&mut self, file_id: FileId, position: TextSize) {
        derivation::note_write(|| FactKey::SpecialCall(file_id, position));
        if let Some(effects) = self.special_call_effects.get_mut(&file_id)
            && effects.remove(&position).is_some()
        {
            derivation::note_write(|| FactKey::SpecialCallFile(file_id));
            if effects.is_empty() {
                self.special_call_effects.remove(&file_id);
            }
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
        derivation::note_read(|| FactKey::SpecialCall(*file_id, position));
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

    /// Whether the file has ANY special-call effect. Used by the flow-walk skip
    /// fast-path: special calls can narrow arbitrary var-refs at runtime, so a
    /// file with any such effect must not skip narrowing walks.
    pub fn has_any_special_call_effect(&self, file_id: &FileId) -> bool {
        derivation::note_read(|| FactKey::SpecialCallFile(*file_id));
        self.special_call_effects
            .get(file_id)
            .is_some_and(|by_pos| !by_pos.is_empty())
    }

    pub fn has_special_call_effect_before(
        &self,
        file_id: &FileId,
        position: TextSize,
        target: &VarRefId,
    ) -> bool {
        derivation::note_read(|| FactKey::SpecialCallFile(*file_id));
        self.special_call_effects
            .get(file_id)
            .is_some_and(|effects_by_position| {
                effects_by_position
                    .iter()
                    .any(|(effect_position, effects)| {
                        *effect_position < position
                            && effects.iter().any(|effect| &effect.target == target)
                    })
            })
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::FlowTree(file_id) => {
                derivation::fingerprint_opt(self.flow_tree_fingerprints.get(file_id))
            }
            FactKey::SpecialCallFile(file_id) => {
                self.special_call_effects
                    .get(file_id)
                    .map_or(derivation::ABSENT, |effects| {
                        effects
                            .iter()
                            .map(|entry| derivation::fingerprint_of(&entry))
                            .fold(0u64, u64::wrapping_add)
                            .max(1)
                    })
            }
            FactKey::SpecialCall(file_id, position) => {
                let effects = self
                    .special_call_effects
                    .get(file_id)
                    .and_then(|effects| effects.get(position));
                let facts = self
                    .special_call_fact_fingerprints
                    .get(&(*file_id, *position))
                    .copied()
                    .unwrap_or(0);
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
        if let Some(by_position) = self.special_call_effects.remove(&file_id) {
            for position in by_position.keys() {
                self.note_special_call_write(file_id, *position);
            }
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
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LuaSpecialCallEffect {
    pub target: VarRefId,
    pub type_ref: LuaType,
}

/// One materialized special-call fact, keyed canonically.
///
/// The key covers every input of the install: the call-site that produced it
/// plus the installed identity (target, type, class, edge, operator and the
/// prefix-derived callable details as applicable). Two settlements that
/// install the same fact build the same key however far apart the pass visits
/// them, so the second is always recognised as a replay.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

    fn site_file(&self) -> FileId {
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
