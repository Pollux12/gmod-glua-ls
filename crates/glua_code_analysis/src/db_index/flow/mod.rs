mod flow_node;
mod flow_tree;
mod signature_cast;

use rowan::{TextRange, TextSize};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::{
    FileId, LuaSignatureId, LuaType, VarRefId,
    db_index::{LuaOperatorId, LuaOperatorOwner, LuaTypeDeclId},
};
pub use flow_node::*;
pub use flow_tree::{
    AssignmentFlowInfo, AssignmentNameTarget, BranchLabelInfo, FileNarrowingCapability, FlowTree,
};
use glua_parser::{LuaAstPtr, LuaDocOpType};
pub use signature_cast::LuaSignatureCast;

use super::traits::LuaIndex;

#[derive(Debug)]
pub struct LuaFlowIndex {
    file_flow_tree: HashMap<FileId, FlowTree>,
    signature_cast_cache: HashMap<FileId, HashMap<LuaSignatureId, LuaSignatureCast>>,
    special_call_effects: HashMap<FileId, HashMap<TextSize, Vec<LuaSpecialCallEffect>>>,
    /// Every special-call fact the epoch below has counted, keyed canonically.
    /// One call-site can install several facts (a synthesized class plus its
    /// super edge, say), and several call-sites can install facts for the same
    /// class, so neither the call-site nor the installed identity alone tells
    /// a replay from a novel fact: the pair does. Re-settling an identical
    /// fact set hits this record in any visit order and bumps nothing.
    settled_special_call_facts: HashSet<SpecialCallFact>,
    /// Count of special-call materializations, keyed by the call-site that
    /// produced them. Special calls publish str-tpl decls, super edges,
    /// `__call` installs and flow effects from the unresolve items, so without
    /// this counter every such settle looks like unversioned state and forces
    /// a full fixpoint requeue.
    special_call_writes: u64,
    special_call_write_versions: HashMap<(FileId, TextSize), u64>,
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
            signature_cast_cache: HashMap::default(),
            special_call_effects: HashMap::default(),
            settled_special_call_facts: HashSet::default(),
            special_call_writes: 0,
            special_call_write_versions: HashMap::default(),
        }
    }

    pub fn add_flow_tree(&mut self, file_id: FileId, flow_tree: FlowTree) {
        self.file_flow_tree.insert(file_id, flow_tree);
    }

    pub fn get_flow_tree(&self, file_id: &FileId) -> Option<&FlowTree> {
        self.file_flow_tree.get(file_id)
    }

    pub fn get_signature_cast(&self, signature_id: &LuaSignatureId) -> Option<&LuaSignatureCast> {
        self.signature_cast_cache
            .get(&signature_id.get_file_id())?
            .get(signature_id)
    }

    pub fn add_signature_cast(
        &mut self,
        file_id: FileId,
        signature_id: LuaSignatureId,
        name: String,
        cast: LuaAstPtr<LuaDocOpType>,
        fallback_cast: Option<LuaAstPtr<LuaDocOpType>>,
    ) {
        self.signature_cast_cache
            .entry(file_id)
            .or_default()
            .insert(
                signature_id,
                LuaSignatureCast {
                    name,
                    cast,
                    fallback_cast,
                },
            );
    }

    /// Installs one special-call effect, reporting whether it was new.
    ///
    /// Re-settling an identical call replays the same effects: only a novel
    /// fact may bump the special-call epoch, or the fixpoint's staleness
    /// filter degrades to a full requeue.
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
        self.install_special_call_facts(
            file_id,
            position,
            [SpecialCallFact::flow_effect(
                file_id, position, target, type_ref,
            )],
        );
        true
    }

    /// Records special-call facts a settle installed, bumping the epoch once
    /// when any of them is novel.
    ///
    /// The duplicate check, the record and the version bump happen together
    /// under one `&mut` borrow, so two settlements of the same fact cannot
    /// interleave a duplicate check past each other: the second sees the
    /// first's record whatever order the pass visits them in. Callers consult
    /// this only after their own index reported the install novel; the record
    /// here is what makes the replay of an identical fact set bump nothing.
    pub(crate) fn install_special_call_facts(
        &mut self,
        file_id: FileId,
        position: TextSize,
        facts: impl IntoIterator<Item = SpecialCallFact>,
    ) -> bool {
        let mut novel = false;
        for fact in facts {
            novel |= self.settled_special_call_facts.insert(fact);
        }
        if novel {
            self.note_special_call_write(file_id, position);
        }
        novel
    }

    /// Compare it across an operation to tell whether the operation
    /// materialized a novel special-call fact.
    pub fn special_call_writes(&self) -> u64 {
        self.special_call_writes
    }

    /// Records a special-call materialization at `position` in `file_id`.
    /// Callers bump only when the write added a novel fact; re-settling an
    /// identical call-site must not bump, or the fixpoint's staleness filter
    /// degrades to a full requeue.
    pub(crate) fn note_special_call_write(&mut self, file_id: FileId, position: TextSize) {
        self.special_call_writes += 1;
        self.special_call_write_versions
            .insert((file_id, position), self.special_call_writes);
    }

    pub(crate) fn special_call_write_version(&self, file_id: &FileId, position: &TextSize) -> u64 {
        self.special_call_write_versions
            .get(&(*file_id, *position))
            .copied()
            .unwrap_or(0)
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
            for effects in effects.values_mut() {
                for effect in effects.iter_mut() {
                    let result = crate::db_index::remap_identities_in_type(&effect.type_ref, remap);
                    if result.lost {
                        dirty.insert(*holder);
                    } else if let Some(typ) = result.typ {
                        effect.type_ref = typ;
                    }
                }
            }
        }
        dirty
    }

    pub fn get_special_call_effects(
        &self,
        file_id: &FileId,
        position: TextSize,
    ) -> Option<&[LuaSpecialCallEffect]> {
        crate::db_index::read_set::record_special_call(file_id, position);
        self.special_call_effects
            .get(file_id)?
            .get(&position)
            .map(|effects| effects.as_slice())
    }

    /// Whether the file has ANY special-call effect. Used by the flow-walk skip
    /// fast-path: special calls can narrow arbitrary var-refs at runtime, so a
    /// file with any such effect must not skip narrowing walks.
    pub fn has_any_special_call_effect(&self, file_id: &FileId) -> bool {
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
}

impl LuaIndex for LuaFlowIndex {
    fn remove(&mut self, file_id: FileId) {
        self.file_flow_tree.remove(&file_id);
        self.signature_cast_cache.remove(&file_id);
        self.settled_special_call_facts
            .retain(|fact| fact.site_file() != file_id);
        // Dropping the effects is a novel fact for their readers, mirroring
        // the signature index's removal tombstones.
        if let Some(by_position) = self.special_call_effects.remove(&file_id) {
            for position in by_position.keys() {
                self.note_special_call_write(file_id, *position);
            }
        }
    }

    fn clear(&mut self) {
        self.file_flow_tree.clear();
        self.signature_cast_cache.clear();
        self.special_call_effects.clear();
        self.settled_special_call_facts.clear();
        self.special_call_write_versions.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
pub(crate) enum SpecialCallFact {
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
    /// constructor root edge), mirroring `has_super_type_at_source`.
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

    #[test]
    fn special_call_write_versions_start_at_zero_and_bump_per_site() {
        let mut index = LuaFlowIndex::new();
        let file_id = FileId::new(1);
        let position = TextSize::new(16);

        assert_eq!(index.special_call_writes(), 0);
        assert_eq!(index.special_call_write_version(&file_id, &position), 0);

        index.note_special_call_write(file_id, position);
        assert_eq!(index.special_call_writes(), 1);
        assert_eq!(index.special_call_write_version(&file_id, &position), 1);
        assert_eq!(
            index.special_call_write_version(&file_id, &TextSize::new(64)),
            0
        );
    }

    /// Re-installing an identical effect reports a duplicate and must not
    /// bump, or the fixpoint filter degrades to a full requeue.
    #[test]
    fn reinstalling_identical_special_call_effect_bumps_no_epoch() {
        let mut index = LuaFlowIndex::new();
        let file_id = FileId::new(1);
        let position = TextSize::new(16);

        assert!(index.add_special_call_effect(
            file_id,
            position,
            effect_target(file_id),
            LuaType::String,
        ));
        assert_eq!(index.special_call_writes(), 1);

        assert!(!index.add_special_call_effect(
            file_id,
            position,
            effect_target(file_id),
            LuaType::String,
        ));
        assert_eq!(index.special_call_writes(), 1);
        assert_eq!(
            index
                .get_special_call_effects(&file_id, position)
                .expect("expected the effect")
                .len(),
            1
        );
    }

    #[test]
    fn removing_file_bumps_its_dropped_effect_versions() {
        let mut index = LuaFlowIndex::new();
        let file_id = FileId::new(1);
        let position = TextSize::new(16);
        index.add_special_call_effect(file_id, position, effect_target(file_id), LuaType::String);
        let version_before = index.special_call_write_version(&file_id, &position);

        index.remove(file_id);
        assert!(
            index.special_call_write_version(&file_id, &position) > version_before,
            "dropping the effect must invalidate its readers"
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

    /// Settling the same fact set in two orders reaches the same epoch, and
    /// the second pass bumps nothing in either order: identical facts always
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
        let writes_after_first_pass = first_order.special_call_writes();
        assert_eq!(writes_after_first_pass, 2);

        // Pass B replays the identical fact set site by site in reverse order:
        // every fact is a duplicate and bumps nothing.
        for ((file_id, position), facts) in &pass_b {
            assert!(
                !first_order.install_special_call_facts(*file_id, *position, facts.clone()),
                "replaying an identical fact must not bump"
            );
        }
        assert_eq!(first_order.special_call_writes(), writes_after_first_pass);
        assert_eq!(
            first_order.special_call_write_version(&site_a.0, &site_a.1),
            1
        );
        assert_eq!(
            first_order.special_call_write_version(&site_b.0, &site_b.1),
            2
        );

        // A fresh index settled B-first reaches the same epoch and fact set.
        let mut second_order = LuaFlowIndex::new();
        for ((file_id, position), facts) in &pass_b {
            assert!(second_order.install_special_call_facts(*file_id, *position, facts.clone()));
        }
        assert_eq!(second_order.special_call_writes(), writes_after_first_pass);
        assert_eq!(
            second_order.settled_special_call_facts, first_order.settled_special_call_facts,
            "the fact set must not depend on visit order"
        );
        for ((file_id, position), facts) in &pass_a {
            assert!(
                !second_order.install_special_call_facts(*file_id, *position, facts.clone()),
                "replaying an identical fact must not bump"
            );
        }
        assert_eq!(second_order.special_call_writes(), writes_after_first_pass);
    }

    /// A re-settled duplicate inside one batch bumps once for the batch, not
    /// once per fact: the epoch counts materializations, not keys.
    #[test]
    fn duplicate_fact_inside_one_batch_bumps_once() {
        let mut index = LuaFlowIndex::new();
        let site = (FileId::new(1), TextSize::new(16));
        let fact = flow_effect_fact(site);

        assert!(index.install_special_call_facts(site.0, site.1, [fact.clone(), fact]));
        assert_eq!(index.special_call_writes(), 1);
    }

    /// Removing a file evicts its fact records, so reinstalling the same facts
    /// afterwards is novel again: the drop was observable to readers.
    #[test]
    fn removing_file_evicts_its_special_call_facts() {
        let mut index = LuaFlowIndex::new();
        let site = (FileId::new(1), TextSize::new(16));
        assert!(index.install_special_call_facts(site.0, site.1, [str_tpl_decl_fact(site)]));
        assert!(!index.install_special_call_facts(site.0, site.1, [str_tpl_decl_fact(site)]));
        let writes_before_remove = index.special_call_writes();

        index.remove(site.0);

        assert!(
            index.install_special_call_facts(site.0, site.1, [str_tpl_decl_fact(site)]),
            "a fact dropped with its file must count as novel when rebuilt"
        );
        assert!(index.special_call_writes() > writes_before_remove);
    }
}
