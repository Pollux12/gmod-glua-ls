mod async_state;
mod gmod_domains;
#[allow(clippy::module_inception)]
mod signature;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use smol_str::SmolStr;

pub use async_state::AsyncState;
pub use gmod_domains::{
    GMOD_ATTR_NET_PAYLOAD, GMOD_ATTR_NET_SEND, GMOD_ATTR_SELF_CALL_VALID, GMOD_ATTR_SELF_GUARD,
    GMOD_ATTR_VALID_GUARD, GMOD_ATTR_WRITES_GLOBAL, GMOD_CALL_ARG_DOMAINS, GMOD_DOMAIN_CLASS_BASE,
    GMOD_DOMAIN_COLOR, GMOD_DOMAIN_CONCOMMAND, GMOD_DOMAIN_CONVAR, GMOD_DOMAIN_DERMA_SKIN,
    GMOD_DOMAIN_ENVIRONMENT, GMOD_DOMAIN_FILE_FIND, GMOD_DOMAIN_GAMEMODE, GMOD_DOMAIN_HOOK,
    GMOD_DOMAIN_LOAD, GMOD_DOMAIN_MEMBER_GUARD, GMOD_DOMAIN_NET_MESSAGE, GMOD_DOMAIN_NET_PAYLOAD,
    GMOD_DOMAIN_NETWORK_VAR, GMOD_DOMAIN_SELF_GUARD, GMOD_DOMAIN_TIMER, GMOD_DOMAIN_VALID_GUARD,
    GMOD_DOMAIN_VGUI_PANEL, GMOD_ROLE_EXISTS, GMOD_ROLE_NET_BITS, GMOD_ROLE_NET_CALLBACK,
    GMOD_ROLE_NET_TARGET, GMOD_ROLE_REFERENCE, GMOD_ROLE_VGUI_CHILD_SELF, GMOD_ROLE_VGUI_PARENT,
    GMOD_ROLE_VGUI_PARENT_SELF, GMOD_SIGNATURE_METADATA_DOMAINS, attribute_use_write_global_root,
    collect_call_arg_roles_for_param, find_best_call_arg_role_for_param,
    find_best_call_arg_role_from_type, find_best_direct_call_arg_role_for_param,
    find_best_direct_call_arg_role_from_type, find_signature_attribute_use,
    rebuild_effective_valid_guard_signatures, signature_attribute_uses,
    signature_has_net_op_metadata, signature_is_valid_guard_in_realm,
    signature_is_valid_guard_or_base_runtime_isvalid_in_realm, signature_net_bits_param_idx,
    signature_net_callback_param_idx, signature_net_payload, signature_net_send,
    signature_owner_for, signature_writes_global_roots, type_is_net_read_op,
};
pub use signature::{
    CALL_ARG_ATTRIBUTE, CALL_ARG_FIELD_ATTRIBUTE, LuaCallArgRole, LuaDocDefaultValue,
    LuaDocParamInfo, LuaDocReturnInfo, LuaGenericParamInfo, LuaNoDiscard, LuaOutParamInfo,
    LuaOutParamRoot, LuaReturnCorrelation, LuaSignature, LuaSignatureId,
    OVERLOAD_CALL_ARG_ATTRIBUTE, OVERLOAD_CALL_ARG_FIELD_ATTRIBUTE, ReturnTypeKind,
    SignaturePayloadRemap, SignatureReturnStatus, find_call_arg_role_from_type,
    visit_call_arg_roles_from_type,
};

use crate::{FileId, GmodStateMask, LuaType, db_index::LuaDeclId};

use super::traits::LuaIndex;

#[derive(Debug)]
pub struct LuaSignatureIndex {
    signatures: HashMap<LuaSignatureId, LuaSignature>,
    in_file_signatures: HashMap<FileId, HashSet<LuaSignatureId>>,
    local_func_decls: HashMap<LuaSignatureId, LuaDeclId>,
    effective_valid_guard_signatures: HashMap<LuaSignatureId, GmodStateMask>,
    /// Per-file valid-guard contributions feeding the effective set: file →
    /// the `(signature, mask)` pairs derived from that file's members. A file
    /// can contribute to another file's signature (a source function shadowing
    /// a metadata-bearing annotation), so removal drops these entries
    /// explicitly instead of filtering the effective map by signature file.
    valid_guard_file_contributions: HashMap<FileId, Vec<(LuaSignatureId, GmodStateMask)>>,
    /// The global-declaration part of the same derivation. `None` until the
    /// first rebuild; always recomputed together with any dirty file part.
    valid_guard_global_contributions: Option<Vec<(LuaSignatureId, GmodStateMask)>>,
    /// Files whose contributions must be re-derived on the next rebuild:
    /// every removal lands here, as does every global-decl change via
    /// [`Self::valid_guard_global_dirty`].
    valid_guard_contribution_dirty_files: HashSet<FileId>,
    valid_guard_global_dirty: bool,
    inferred_positive_guards: HashMap<LuaSignatureId, LuaInferredPositiveGuard>,
    inferred_guard_owners: HashMap<LuaSignatureId, LuaInferredGuardOwner>,
    inferred_guard_facts: HashMap<LuaInferredGuardOwner, LuaInferredPositiveGuard>,
    inferred_guard_owners_by_file: HashMap<FileId, HashSet<LuaInferredGuardOwner>>,
    inferred_guard_consumers: HashMap<LuaInferredGuardOwner, HashSet<FileId>>,
    inferred_guard_dependencies: HashMap<FileId, HashSet<LuaInferredGuardOwner>>,
    inferred_return_dependencies: HashMap<LuaSignatureId, HashSet<FileId>>,
    inferred_return_consumers: HashMap<FileId, HashSet<LuaSignatureId>>,
    settled_reads_by_file: HashMap<FileId, HashSet<LuaSignatureId>>,
    settled_read_dependents: HashMap<LuaSignatureId, HashSet<FileId>>,
    payload_sources_by_holder: HashMap<FileId, HashSet<FileId>>,
    payload_holders_by_source: HashMap<FileId, HashSet<FileId>>,
    inferred_positive_guards_changed: bool,
    /// Count of inferred-return changes, for telling a pass that moved a
    /// return from one that did not.
    return_writes: u64,
    return_write_versions: HashMap<LuaSignatureId, u64>,
    receiver_out_param_member_names: HashMap<String, usize>,
    in_file_receiver_out_param_member_names: HashMap<FileId, HashSet<String>>,
}

/// A conservative true-branch parameter narrowing derived from a function body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaInferredPositiveGuard {
    pub param_idx: usize,
    pub narrowed_type: LuaType,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LuaInferredGuardOwner {
    GlobalPath {
        signature_id: LuaSignatureId,
        state_mask: GmodStateMask,
        path: Box<[SmolStr]>,
    },
}

pub(crate) fn canonicalize_global_root_path(path: &mut Vec<SmolStr>) {
    if path.len() > 1 && matches!(path[0].as_str(), "_G" | "_ENV") {
        path.remove(0);
    }
}

impl LuaInferredGuardOwner {
    pub fn source_file_id(&self) -> FileId {
        match self {
            Self::GlobalPath { signature_id, .. } => signature_id.get_file_id(),
        }
    }

    pub fn signature_id(&self) -> LuaSignatureId {
        match self {
            Self::GlobalPath { signature_id, .. } => *signature_id,
        }
    }

    pub fn path(&self) -> &[SmolStr] {
        match self {
            Self::GlobalPath { path, .. } => path,
        }
    }

    pub fn state_mask(&self) -> GmodStateMask {
        match self {
            Self::GlobalPath { state_mask, .. } => *state_mask,
        }
    }

    pub fn source_position(&self) -> u32 {
        match self {
            Self::GlobalPath { signature_id, .. } => signature_id.get_position().into(),
        }
    }
}

impl Default for LuaSignatureIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaSignatureIndex {
    /// Compare it across an operation to tell whether the operation changed
    /// any inferred return.
    pub fn return_writes(&self) -> u64 {
        self.return_writes
    }

    pub(crate) fn note_return_write(&mut self, signature_id: LuaSignatureId) {
        self.return_writes += 1;
        self.return_write_versions
            .insert(signature_id, self.return_writes);
    }

    pub(crate) fn return_write_version(&self, signature_id: &LuaSignatureId) -> u64 {
        self.return_write_versions
            .get(signature_id)
            .copied()
            .unwrap_or(0)
    }

    pub fn new() -> Self {
        Self {
            signatures: HashMap::default(),
            in_file_signatures: HashMap::default(),
            local_func_decls: HashMap::default(),
            effective_valid_guard_signatures: HashMap::default(),
            valid_guard_file_contributions: HashMap::default(),
            valid_guard_global_contributions: None,
            valid_guard_contribution_dirty_files: HashSet::default(),
            valid_guard_global_dirty: false,
            inferred_positive_guards: HashMap::default(),
            inferred_guard_owners: HashMap::default(),
            inferred_guard_facts: HashMap::default(),
            inferred_guard_owners_by_file: HashMap::default(),
            inferred_guard_consumers: HashMap::default(),
            inferred_guard_dependencies: HashMap::default(),
            inferred_return_dependencies: HashMap::default(),
            inferred_return_consumers: HashMap::default(),
            settled_reads_by_file: HashMap::default(),
            settled_read_dependents: HashMap::default(),
            payload_sources_by_holder: HashMap::default(),
            payload_holders_by_source: HashMap::default(),
            inferred_positive_guards_changed: false,
            return_writes: 0,
            return_write_versions: HashMap::default(),
            receiver_out_param_member_names: HashMap::default(),
            in_file_receiver_out_param_member_names: HashMap::default(),
        }
    }

    pub fn get_or_create(&mut self, signature_id: LuaSignatureId) -> &mut LuaSignature {
        self.in_file_signatures
            .entry(signature_id.get_file_id())
            .or_default()
            .insert(signature_id);
        self.signatures.entry(signature_id).or_default()
    }

    pub fn get(&self, signature_id: &LuaSignatureId) -> Option<&LuaSignature> {
        crate::db_index::read_set::record_signature(signature_id);
        self.signatures.get(signature_id)
    }

    pub fn get_mut(&mut self, signature_id: &LuaSignatureId) -> Option<&mut LuaSignature> {
        self.signatures.get_mut(signature_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&LuaSignatureId, &LuaSignature)> {
        self.signatures.iter()
    }

    /// Number of signatures currently indexed. `O(1)`.
    pub fn indexed_signature_count(&self) -> usize {
        self.signatures.len()
    }

    pub fn local_func_decl_for(&self, signature_id: &LuaSignatureId) -> Option<LuaDeclId> {
        self.local_func_decls.get(signature_id).copied()
    }

    pub fn bind_local_func_decl(&mut self, signature_id: LuaSignatureId, decl_id: LuaDeclId) {
        self.local_func_decls.insert(signature_id, decl_id);
    }

    pub fn clear_effective_valid_guard_signatures(&mut self) {
        self.effective_valid_guard_signatures.clear();
    }

    pub fn mark_effective_valid_guard_signature(
        &mut self,
        signature_id: LuaSignatureId,
        state_mask: GmodStateMask,
    ) {
        self.effective_valid_guard_signatures
            .entry(signature_id)
            .and_modify(|existing| existing.insert(state_mask))
            .or_insert(state_mask);
    }

    pub fn is_effective_valid_guard_signature(&self, signature_id: &LuaSignatureId) -> bool {
        self.effective_valid_guard_signatures
            .contains_key(signature_id)
    }

    pub fn effective_valid_guard_signature_mask(
        &self,
        signature_id: &LuaSignatureId,
    ) -> Option<GmodStateMask> {
        self.effective_valid_guard_signatures
            .get(signature_id)
            .copied()
    }

    pub(crate) fn valid_guard_global_contributions(
        &self,
    ) -> Option<&[(LuaSignatureId, GmodStateMask)]> {
        self.valid_guard_global_contributions.as_deref()
    }

    pub(crate) fn valid_guard_global_dirty(&self) -> bool {
        self.valid_guard_global_dirty
    }

    pub(crate) fn valid_guard_file_contributions(
        &self,
    ) -> &HashMap<FileId, Vec<(LuaSignatureId, GmodStateMask)>> {
        &self.valid_guard_file_contributions
    }

    pub(crate) fn take_valid_guard_dirty_files(&mut self) -> HashSet<FileId> {
        std::mem::take(&mut self.valid_guard_contribution_dirty_files)
    }

    pub(crate) fn store_valid_guard_global_contributions(
        &mut self,
        contributions: Vec<(LuaSignatureId, GmodStateMask)>,
    ) {
        self.valid_guard_global_contributions = Some(contributions);
        self.valid_guard_global_dirty = false;
    }

    pub(crate) fn store_valid_guard_file_contributions(
        &mut self,
        file_id: FileId,
        contributions: Vec<(LuaSignatureId, GmodStateMask)>,
    ) {
        self.valid_guard_file_contributions
            .insert(file_id, contributions);
        self.valid_guard_contribution_dirty_files.remove(&file_id);
    }

    /// Drops one file's contributions explicitly and marks the global part
    /// dirty: member removal can change the global member-identity masks, and
    /// decl removal can drop a global identity altogether.
    pub(crate) fn remove_valid_guard_file_contributions(&mut self, file_id: FileId) {
        self.valid_guard_file_contributions.remove(&file_id);
        self.valid_guard_contribution_dirty_files.remove(&file_id);
        self.valid_guard_global_dirty = true;
    }

    /// Re-derives the effective set as the union of the stored contributions.
    /// Union is commutative, so the result is independent of hash iteration
    /// order.
    pub(crate) fn rederive_effective_valid_guard_signatures(&mut self) {
        let mut effective: HashMap<LuaSignatureId, GmodStateMask> = HashMap::default();
        let union = |effective: &mut HashMap<LuaSignatureId, GmodStateMask>,
                     signature_id: LuaSignatureId,
                     state_mask: GmodStateMask| {
            effective
                .entry(signature_id)
                .and_modify(|existing| existing.insert(state_mask))
                .or_insert(state_mask);
        };
        if let Some(global) = self.valid_guard_global_contributions.clone() {
            for (signature_id, state_mask) in global {
                union(&mut effective, signature_id, state_mask);
            }
        }
        for (signature_id, state_mask) in self.valid_guard_file_contributions.values().flatten() {
            union(&mut effective, *signature_id, *state_mask);
        }
        self.effective_valid_guard_signatures = effective;
    }

    pub(crate) fn clear_valid_guard_contributions(&mut self) {
        self.valid_guard_file_contributions.clear();
        self.valid_guard_global_contributions = None;
        self.valid_guard_contribution_dirty_files.clear();
        self.valid_guard_global_dirty = false;
    }

    pub fn set_inferred_positive_guard(
        &mut self,
        signature_id: LuaSignatureId,
        guard: LuaInferredPositiveGuard,
    ) {
        if self.inferred_positive_guards.get(&signature_id) != Some(&guard) {
            self.inferred_positive_guards_changed = true;
        }
        self.inferred_positive_guards.insert(signature_id, guard);
    }

    pub fn set_owned_inferred_positive_guard(
        &mut self,
        signature_id: LuaSignatureId,
        owner: LuaInferredGuardOwner,
        guard: LuaInferredPositiveGuard,
    ) {
        if self.inferred_guard_facts.get(&owner) != Some(&guard) {
            self.inferred_positive_guards_changed = true;
        }
        self.inferred_positive_guards
            .insert(signature_id, guard.clone());
        self.inferred_guard_owners
            .insert(signature_id, owner.clone());
        self.inferred_guard_facts.insert(owner.clone(), guard);
        self.inferred_guard_owners_by_file
            .entry(owner.source_file_id())
            .or_default()
            .insert(owner);
    }

    pub fn inferred_positive_guard(
        &self,
        signature_id: &LuaSignatureId,
    ) -> Option<&LuaInferredPositiveGuard> {
        self.inferred_positive_guards.get(signature_id)
    }

    pub fn inferred_guard_owner(
        &self,
        signature_id: &LuaSignatureId,
    ) -> Option<&LuaInferredGuardOwner> {
        self.inferred_guard_owners.get(signature_id)
    }

    pub fn inferred_guard_facts_for_files(
        &self,
        file_ids: &HashSet<FileId>,
    ) -> HashMap<LuaInferredGuardOwner, LuaInferredPositiveGuard> {
        file_ids
            .iter()
            .filter_map(|file_id| self.inferred_guard_owners_by_file.get(file_id))
            .flatten()
            .filter_map(|owner| {
                self.inferred_guard_facts
                    .get(owner)
                    .cloned()
                    .map(|guard| (owner.clone(), guard))
            })
            .collect()
    }

    pub fn inferred_guard_consumers_for_files(
        &self,
        file_ids: &HashSet<FileId>,
    ) -> HashSet<FileId> {
        file_ids
            .iter()
            .filter_map(|file_id| self.inferred_guard_owners_by_file.get(file_id))
            .flatten()
            .filter_map(|owner| self.inferred_guard_consumers.get(owner))
            .flatten()
            .copied()
            .collect()
    }

    pub fn inferred_guard_consumers(
        &self,
        owner: &LuaInferredGuardOwner,
    ) -> impl Iterator<Item = FileId> + '_ {
        self.inferred_guard_consumers
            .get(owner)
            .into_iter()
            .flatten()
            .copied()
    }

    pub fn migrate_inferred_guard_consumers(
        &mut self,
        owner: LuaInferredGuardOwner,
        consumers: &HashSet<FileId>,
        reindexed_file_ids: &HashSet<FileId>,
    ) {
        for consumer_file_id in consumers
            .iter()
            .filter(|file_id| !reindexed_file_ids.contains(file_id))
        {
            self.inferred_guard_consumers
                .entry(owner.clone())
                .or_default()
                .insert(*consumer_file_id);
            self.inferred_guard_dependencies
                .entry(*consumer_file_id)
                .or_default()
                .insert(owner.clone());
        }
    }

    pub fn set_inferred_guard_dependencies(
        &mut self,
        consumer_file_id: FileId,
        owners: HashSet<LuaInferredGuardOwner>,
    ) {
        self.clear_inferred_guard_dependencies(consumer_file_id);
        for owner in &owners {
            self.inferred_guard_consumers
                .entry(owner.clone())
                .or_default()
                .insert(consumer_file_id);
        }
        if !owners.is_empty() {
            self.inferred_guard_dependencies
                .insert(consumer_file_id, owners);
        }
    }

    fn clear_inferred_guard_dependencies(&mut self, consumer_file_id: FileId) {
        let Some(owners) = self.inferred_guard_dependencies.remove(&consumer_file_id) else {
            return;
        };
        for owner in owners {
            if let Some(consumers) = self.inferred_guard_consumers.get_mut(&owner) {
                consumers.remove(&consumer_file_id);
                if consumers.is_empty() {
                    self.inferred_guard_consumers.remove(&owner);
                }
            }
        }
    }

    pub(crate) fn set_inferred_return_dependencies(
        &mut self,
        signature_id: LuaSignatureId,
        source_file_ids: HashSet<FileId>,
    ) {
        self.clear_inferred_return_dependencies(signature_id);
        for source_file_id in &source_file_ids {
            self.inferred_return_consumers
                .entry(*source_file_id)
                .or_default()
                .insert(signature_id);
        }
        if !source_file_ids.is_empty() {
            self.inferred_return_dependencies
                .insert(signature_id, source_file_ids);
        }
    }

    fn clear_inferred_return_dependencies(&mut self, signature_id: LuaSignatureId) {
        let Some(source_file_ids) = self.inferred_return_dependencies.remove(&signature_id) else {
            return;
        };
        for source_file_id in source_file_ids {
            if let Some(consumers) = self.inferred_return_consumers.get_mut(&source_file_id) {
                consumers.remove(&signature_id);
                if consumers.is_empty() {
                    self.inferred_return_consumers.remove(&source_file_id);
                }
            }
        }
    }

    pub(crate) fn inferred_return_dependents_for_files(
        &self,
        source_file_ids: &HashSet<FileId>,
    ) -> HashSet<FileId> {
        source_file_ids
            .iter()
            .filter_map(|file_id| self.inferred_return_consumers.get(file_id))
            .flatten()
            .map(LuaSignatureId::get_file_id)
            .collect()
    }

    pub(crate) fn set_settled_reads(
        &mut self,
        file_id: FileId,
        reads: impl IntoIterator<Item = LuaSignatureId>,
    ) {
        self.clear_settled_reads(file_id);
        let reads = reads
            .into_iter()
            .filter(|signature_id| signature_id.get_file_id() != file_id)
            .collect::<HashSet<_>>();
        for signature_id in &reads {
            self.settled_read_dependents
                .entry(*signature_id)
                .or_default()
                .insert(file_id);
        }
        if !reads.is_empty() {
            self.settled_reads_by_file.insert(file_id, reads);
        }
    }

    fn clear_settled_reads(&mut self, file_id: FileId) {
        let Some(reads) = self.settled_reads_by_file.remove(&file_id) else {
            return;
        };
        for signature_id in reads {
            if let Some(dependents) = self.settled_read_dependents.get_mut(&signature_id) {
                dependents.remove(&file_id);
                if dependents.is_empty() {
                    self.settled_read_dependents.remove(&signature_id);
                }
            }
        }
    }

    pub(crate) fn settled_read_dependents_for_signatures(
        &self,
        signature_ids: &[LuaSignatureId],
    ) -> HashSet<FileId> {
        signature_ids
            .iter()
            .filter_map(|signature_id| self.settled_read_dependents.get(signature_id))
            .flatten()
            .copied()
            .collect()
    }

    pub(crate) fn settled_read_dependents_for_files(
        &self,
        file_ids: &HashSet<FileId>,
    ) -> HashSet<FileId> {
        self.settled_read_dependents
            .iter()
            .filter(|(signature_id, _)| file_ids.contains(&signature_id.get_file_id()))
            .flat_map(|(_, dependents)| dependents.iter().copied())
            .collect()
    }

    pub(crate) fn remap_settled_reads(&mut self, remap: &crate::FileRemap) -> HashSet<FileId> {
        let old_ids = self
            .settled_read_dependents
            .keys()
            .filter(|signature_id| signature_id.get_file_id() == remap.file_id)
            .copied()
            .collect::<Vec<_>>();
        let mut dirty = HashSet::default();
        let mut updates = Vec::new();
        for old_id in old_ids {
            let new_id = match remap.signature_id(old_id) {
                crate::Remap::Moved(new_id) if new_id != old_id => Some(new_id),
                crate::Remap::Moved(_) | crate::Remap::Unrelated => continue,
                crate::Remap::Lost => None,
            };
            let dependents = self
                .settled_read_dependents
                .get(&old_id)
                .cloned()
                .unwrap_or_default();
            if new_id.is_none() {
                dirty.extend(dependents.iter().copied());
            }
            updates.push((old_id, new_id, dependents));
        }

        for (old_id, _, _) in &updates {
            self.settled_read_dependents.remove(old_id);
        }
        for (old_id, new_id, dependents) in updates {
            for dependent in &dependents {
                if let Some(reads) = self.settled_reads_by_file.get_mut(dependent) {
                    reads.remove(&old_id);
                    if let Some(new_id) = new_id {
                        reads.insert(new_id);
                    }
                }
            }
            if let Some(new_id) = new_id {
                self.settled_read_dependents
                    .entry(new_id)
                    .or_default()
                    .extend(dependents);
            }
        }
        dirty
    }

    /// Indexed per-holder payload population shared by snapshot and remap.
    ///
    /// `in_file_signatures[holder]` owns the holder's signature ids (position
    /// sorted); looking each id up in `inferred_positive_guards` covers the
    /// holder's positive guards without scanning the workspace map.
    /// `inferred_guard_owners_by_file[holder]` owns the holder's guard-fact
    /// owners directly. Owner order does not affect the observable `HashSet`
    /// outputs, so owners are left unsorted.
    fn holder_payload_keys(
        &self,
        holder: FileId,
    ) -> (Vec<LuaSignatureId>, Vec<LuaInferredGuardOwner>) {
        let mut signature_ids: Vec<_> = self
            .in_file_signatures
            .get(&holder)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        signature_ids.sort_by_key(|id| u32::from(id.get_position()));
        let fact_owners: Vec<_> = self
            .inferred_guard_owners_by_file
            .get(&holder)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        (signature_ids, fact_owners)
    }

    /// Rebuilds the file-granularity reverse edges for one holder file.
    ///
    /// Inspects every signature owned by `holder` plus the typed
    /// inferred-guard payloads owned/filed there, collecting the source files
    /// named by nested `TableConst`, `Instance` and `Signature` identities.
    /// The holder's old reverse edges are removed and the new ones added
    /// atomically, so reanalysis never leaves stale edges behind. Self-file
    /// references are excluded from the cross-file set.
    pub(crate) fn snapshot_payload_refs(&mut self, holder: FileId) {
        let mut sources = HashSet::default();
        let mut collect = |ty: &LuaType| {
            ty.visit_type_identities(&mut |inner| match inner {
                LuaType::TableConst(range) => {
                    if range.file_id != holder {
                        sources.insert(range.file_id);
                    }
                }
                LuaType::Instance(instance) => {
                    let source = instance.get_range().file_id;
                    if source != holder {
                        sources.insert(source);
                    }
                }
                LuaType::Signature(signature_id) => {
                    let source = signature_id.get_file_id();
                    if source != holder {
                        sources.insert(source);
                    }
                }
                _ => {}
            });
        };

        let (signature_ids, fact_owners) = self.holder_payload_keys(holder);
        for signature_id in &signature_ids {
            if let Some(signature) = self.signatures.get(signature_id) {
                signature.visit_payload_types(&mut |ty| collect(ty));
            }
            if let Some(guard) = self.inferred_positive_guards.get(signature_id) {
                collect(&guard.narrowed_type);
            }
        }

        for owner in &fact_owners {
            if let Some(guard) = self.inferred_guard_facts.get(owner) {
                collect(&guard.narrowed_type);
            }
        }

        let old_sources = self
            .payload_sources_by_holder
            .get(&holder)
            .cloned()
            .unwrap_or_default();
        if old_sources == sources {
            return;
        }
        let mut old_sorted: Vec<FileId> = old_sources.into_iter().collect();
        old_sorted.sort();
        for source in old_sorted {
            if let Some(holders) = self.payload_holders_by_source.get_mut(&source) {
                holders.remove(&holder);
                if holders.is_empty() {
                    self.payload_holders_by_source.remove(&source);
                }
            }
        }
        let mut new_sorted: Vec<FileId> = sources.iter().copied().collect();
        new_sorted.sort();
        for source in new_sorted {
            self.payload_holders_by_source
                .entry(source)
                .or_default()
                .insert(holder);
        }
        if sources.is_empty() {
            self.payload_sources_by_holder.remove(&holder);
        } else {
            self.payload_sources_by_holder.insert(holder, sources);
        }
    }

    fn drop_payload_holder_edges(&mut self, holder: FileId) {
        let Some(old_sources) = self.payload_sources_by_holder.remove(&holder) else {
            return;
        };
        let mut old_sorted: Vec<FileId> = old_sources.into_iter().collect();
        old_sorted.sort();
        for source in old_sorted {
            if let Some(holders) = self.payload_holders_by_source.get_mut(&source) {
                holders.remove(&holder);
                if holders.is_empty() {
                    self.payload_holders_by_source.remove(&source);
                }
            }
        }
    }

    /// Rewrites foreign signature payloads that name the edited file.
    ///
    /// Candidates come only from the reverse payload map, never from a scan.
    /// Each affected signature/guard payload is atomic: a lost nested identity
    /// leaves that payload untouched and dirties its holder file instead.
    /// Coordinate-only moves never call `note_return_write`, so export hashing
    /// with the same `FileRemap` stays stable.
    pub(crate) fn remap_payload_types(&mut self, remap: &crate::FileRemap) -> HashSet<FileId> {
        let mut candidates: Vec<FileId> = self
            .payload_holders_by_source
            .get(&remap.file_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        candidates.sort();
        let mut dirty = HashSet::default();
        for holder in candidates {
            if holder == remap.file_id {
                continue;
            }
            let mut holder_lost = false;
            let (signature_ids, fact_owners) = self.holder_payload_keys(holder);
            for signature_id in signature_ids {
                if let Some(signature) = self.signatures.get_mut(&signature_id) {
                    let outcome = signature.remap_payload_types(remap);
                    if outcome.lost {
                        holder_lost = true;
                    }
                }
                if let Some(guard) = self.inferred_positive_guards.get_mut(&signature_id) {
                    let result =
                        crate::db_index::remap_identities_in_type(&guard.narrowed_type, remap);
                    if result.lost {
                        holder_lost = true;
                    } else if let Some(new_ty) = result.typ {
                        guard.narrowed_type = new_ty;
                    }
                }
            }

            for owner in fact_owners {
                if let Some(guard) = self.inferred_guard_facts.get_mut(&owner) {
                    let result =
                        crate::db_index::remap_identities_in_type(&guard.narrowed_type, remap);
                    if result.lost {
                        holder_lost = true;
                    } else if let Some(new_ty) = result.typ {
                        guard.narrowed_type = new_ty;
                    }
                }
            }

            if holder_lost {
                dirty.insert(holder);
            }
        }
        dirty.remove(&remap.file_id);
        dirty
    }

    pub fn clear_inferred_positive_guards_for_file(&mut self, file_id: FileId) {
        let old_owners = self
            .inferred_guard_owners_by_file
            .remove(&file_id)
            .unwrap_or_default();
        let changed = !old_owners.is_empty();
        for owner in old_owners {
            self.inferred_guard_facts.remove(&owner);
            if let Some(consumers) = self.inferred_guard_consumers.remove(&owner) {
                for consumer_file_id in consumers {
                    if let Some(dependencies) =
                        self.inferred_guard_dependencies.get_mut(&consumer_file_id)
                    {
                        dependencies.remove(&owner);
                        if dependencies.is_empty() {
                            self.inferred_guard_dependencies.remove(&consumer_file_id);
                        }
                    }
                }
            }
        }
        self.inferred_positive_guards
            .retain(|signature_id, _| signature_id.get_file_id() != file_id);
        self.inferred_guard_owners
            .retain(|signature_id, _| signature_id.get_file_id() != file_id);
        self.inferred_positive_guards_changed |= changed;
    }

    pub fn take_inferred_positive_guards_changed(&mut self) -> bool {
        std::mem::take(&mut self.inferred_positive_guards_changed)
    }

    pub fn add_receiver_out_param_member_name(&mut self, file_id: FileId, member_name: String) {
        if !self
            .in_file_receiver_out_param_member_names
            .entry(file_id)
            .or_default()
            .insert(member_name.clone())
        {
            return;
        }

        *self
            .receiver_out_param_member_names
            .entry(member_name)
            .or_default() += 1;
    }

    pub fn has_receiver_out_param_member_name(&self, member_name: &str) -> bool {
        self.receiver_out_param_member_names
            .contains_key(member_name)
    }

    pub fn receiver_out_param_member_names(&self) -> impl Iterator<Item = &str> {
        self.receiver_out_param_member_names
            .keys()
            .map(String::as_str)
    }

    pub fn get_file_signature_ids(&self, file_id: FileId) -> Option<&HashSet<LuaSignatureId>> {
        self.in_file_signatures.get(&file_id)
    }
}

impl LuaIndex for LuaSignatureIndex {
    fn remove(&mut self, file_id: FileId) {
        // Drop only the removed file's holder edges. Reverse edges where it is
        // the referenced source stay so a later remap can still report loss.
        self.drop_payload_holder_edges(file_id);
        self.clear_settled_reads(file_id);
        self.clear_inferred_guard_dependencies(file_id);
        if let Some(signature_ids) = self.in_file_signatures.remove(&file_id) {
            for signature_id in signature_ids {
                self.clear_inferred_return_dependencies(signature_id);
                if self.signatures.remove(&signature_id).is_some() {
                    self.note_return_write(signature_id);
                }
                self.local_func_decls.remove(&signature_id);
            }
        }
        // Drop this file's valid-guard contributions explicitly and re-derive
        // the effective set from what remains: a removed file can contribute
        // to another file's signature (a source function shadowing a
        // metadata-bearing annotation), which a signature-file filter on the
        // effective map would leave stale.
        self.remove_valid_guard_file_contributions(file_id);
        self.rederive_effective_valid_guard_signatures();
        self.clear_inferred_positive_guards_for_file(file_id);

        if let Some(member_names) = self
            .in_file_receiver_out_param_member_names
            .remove(&file_id)
        {
            for member_name in member_names {
                match self.receiver_out_param_member_names.get_mut(&member_name) {
                    Some(count) if *count > 1 => *count -= 1,
                    Some(_) => {
                        self.receiver_out_param_member_names.remove(&member_name);
                    }
                    None => {}
                }
            }
        }

        // Also drop entries whose target decl lived in the removed file, even if
        // the signature key was not tracked in that file's signature set.
        self.local_func_decls
            .retain(|_, decl_id| decl_id.file_id != file_id);
    }

    fn clear(&mut self) {
        self.signatures.clear();
        self.in_file_signatures.clear();
        self.local_func_decls.clear();
        self.return_write_versions.clear();
        self.effective_valid_guard_signatures.clear();
        self.clear_valid_guard_contributions();
        self.inferred_positive_guards.clear();
        self.inferred_guard_owners.clear();
        self.inferred_guard_facts.clear();
        self.inferred_guard_owners_by_file.clear();
        self.inferred_guard_consumers.clear();
        self.inferred_guard_dependencies.clear();
        self.inferred_return_dependencies.clear();
        self.inferred_return_consumers.clear();
        self.settled_reads_by_file.clear();
        self.settled_read_dependents.clear();
        self.payload_sources_by_holder.clear();
        self.payload_holders_by_source.clear();
        self.inferred_positive_guards_changed = false;
        self.receiver_out_param_member_names.clear();
        self.in_file_receiver_out_param_member_names.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(source_file_id: FileId) -> LuaInferredGuardOwner {
        LuaInferredGuardOwner::GlobalPath {
            signature_id: LuaSignatureId::new(source_file_id, 0.into()),
            state_mask: GmodStateMask::empty(),
            path: vec!["IsPlayer".into()].into_boxed_slice(),
        }
    }

    #[test]
    fn removing_consumer_clears_reverse_inferred_guard_dependency() {
        let source_file_id = FileId::new(1);
        let consumer_file_id = FileId::new(2);
        let owner = owner(source_file_id);
        let mut index = LuaSignatureIndex::new();
        index
            .inferred_guard_owners_by_file
            .entry(source_file_id)
            .or_default()
            .insert(owner.clone());
        index
            .set_inferred_guard_dependencies(consumer_file_id, HashSet::from_iter([owner.clone()]));

        index.remove(consumer_file_id);

        assert!(index.inferred_guard_dependencies.is_empty());
        assert!(index.inferred_guard_consumers.is_empty());
        assert!(
            index
                .inferred_guard_consumers_for_files(&HashSet::from_iter([source_file_id]))
                .is_empty()
        );
    }

    #[test]
    fn removing_guard_source_clears_consumer_forward_dependency() {
        let source_file_id = FileId::new(1);
        let consumer_file_id = FileId::new(2);
        let owner = owner(source_file_id);
        let mut index = LuaSignatureIndex::new();
        index
            .inferred_guard_owners_by_file
            .entry(source_file_id)
            .or_default()
            .insert(owner.clone());
        index.inferred_guard_facts.insert(
            owner.clone(),
            LuaInferredPositiveGuard {
                param_idx: 0,
                narrowed_type: LuaType::String,
            },
        );
        index.set_inferred_guard_dependencies(consumer_file_id, HashSet::from_iter([owner]));

        index.remove(source_file_id);

        assert!(index.inferred_guard_facts.is_empty());
        assert!(index.inferred_guard_owners_by_file.is_empty());
        assert!(index.inferred_guard_dependencies.is_empty());
        assert!(index.inferred_guard_consumers.is_empty());
    }

    fn table_ref(source: FileId, start: u32, end: u32) -> LuaType {
        LuaType::TableConst(crate::InFiled::new(
            source,
            rowan::TextRange::new(start.into(), end.into()),
        ))
    }

    fn signature_ref(source: FileId, position: u32) -> LuaType {
        LuaType::Signature(LuaSignatureId::new(source, position.into()))
    }

    #[test]
    fn payload_snapshot_replaces_stale_reverse_edges() {
        let holder = FileId::new(10);
        let source_a = FileId::new(20);
        let source_b = FileId::new(30);
        let source_c = FileId::new(40);
        let mut index = LuaSignatureIndex::new();

        let signature_id = LuaSignatureId::new(holder, 0.into());
        {
            let signature = index.get_or_create(signature_id);
            signature
                .return_docs
                .push(crate::db_index::signature::LuaDocReturnInfo {
                    name: None,
                    type_ref: table_ref(source_a, 0, 1),
                    default_value: None,
                    description: None,
                    attributes: None,
                    return_kind: crate::db_index::signature::ReturnTypeKind::Reference,
                });
            signature.generic_params.push(std::sync::Arc::new(
                crate::db_index::signature::LuaGenericParamInfo::new(
                    "T".to_string(),
                    Some(signature_ref(source_a, 7)),
                    None,
                ),
            ));
        }
        index.set_inferred_positive_guard(
            signature_id,
            LuaInferredPositiveGuard {
                param_idx: 0,
                narrowed_type: table_ref(source_b, 0, 1),
            },
        );
        let fact_owner = LuaInferredGuardOwner::GlobalPath {
            signature_id: LuaSignatureId::new(holder, 5.into()),
            state_mask: GmodStateMask::empty(),
            path: vec!["Fact".into()].into_boxed_slice(),
        };
        index.set_owned_inferred_positive_guard(
            fact_owner.signature_id(),
            fact_owner.clone(),
            LuaInferredPositiveGuard {
                param_idx: 0,
                narrowed_type: table_ref(source_c, 0, 1),
            },
        );

        index.snapshot_payload_refs(holder);

        assert_eq!(
            index
                .payload_sources_by_holder
                .get(&holder)
                .cloned()
                .unwrap_or_default(),
            HashSet::from_iter([source_a, source_b, source_c]),
        );
        for source in [source_a, source_b, source_c] {
            assert!(
                index
                    .payload_holders_by_source
                    .get(&source)
                    .is_some_and(|holders| holders.contains(&holder)),
                "reverse edge for {source:?} should hold {holder:?}",
            );
        }

        // Drop the signature payloads; only the guard/fact edges survive.
        if let Some(signature) = index.signatures.get_mut(&signature_id) {
            for ret in &mut signature.return_docs {
                ret.type_ref = LuaType::String;
            }
            for generic in &mut signature.generic_params {
                let rebuilt = crate::db_index::signature::LuaGenericParamInfo::new(
                    generic.name.clone(),
                    None,
                    generic.attributes.clone(),
                );
                *generic = std::sync::Arc::new(rebuilt);
            }
        }
        index.snapshot_payload_refs(holder);

        assert_eq!(
            index
                .payload_sources_by_holder
                .get(&holder)
                .cloned()
                .unwrap_or_default(),
            HashSet::from_iter([source_b, source_c]),
        );
        assert!(
            !index.payload_holders_by_source.contains_key(&source_a),
            "stale reverse edge for {source_a:?} should be gone",
        );
        for source in [source_b, source_c] {
            assert!(
                index
                    .payload_holders_by_source
                    .get(&source)
                    .is_some_and(|holders| holders.contains(&holder)),
            );
        }

        // Self-file references never enter the cross-file set.
        if let Some(signature) = index.signatures.get_mut(&signature_id) {
            for ret in &mut signature.return_docs {
                ret.type_ref = table_ref(holder, 0, 1);
            }
        }
        index
            .inferred_positive_guards
            .get_mut(&signature_id)
            .expect("guard")
            .narrowed_type = LuaType::String;
        index
            .inferred_guard_facts
            .get_mut(&fact_owner)
            .expect("fact")
            .narrowed_type = LuaType::String;
        index.snapshot_payload_refs(holder);

        assert!(
            index
                .payload_sources_by_holder
                .get(&holder)
                .cloned()
                .unwrap_or_default()
                .is_empty()
                || !index
                    .payload_sources_by_holder
                    .get(&holder)
                    .cloned()
                    .unwrap_or_default()
                    .contains(&holder),
            "self-file refs must stay out of the forward set",
        );
        assert!(!index.payload_holders_by_source.contains_key(&holder));
        assert!(!index.payload_holders_by_source.contains_key(&source_b));
        assert!(!index.payload_holders_by_source.contains_key(&source_c));
    }

    #[test]
    fn removing_holder_cleans_payload_edges_but_removing_source_preserves_them() {
        let holder_a = FileId::new(11);
        let holder_b = FileId::new(12);
        let source = FileId::new(20);
        let mut index = LuaSignatureIndex::new();

        for (holder, position) in [(holder_a, 0), (holder_b, 10)] {
            let signature_id = LuaSignatureId::new(holder, position.into());
            let signature = index.get_or_create(signature_id);
            signature
                .return_docs
                .push(crate::db_index::signature::LuaDocReturnInfo {
                    name: None,
                    type_ref: table_ref(source, 0, 1),
                    default_value: None,
                    description: None,
                    attributes: None,
                    return_kind: crate::db_index::signature::ReturnTypeKind::Reference,
                });
            index.snapshot_payload_refs(holder);
        }

        assert_eq!(
            index
                .payload_holders_by_source
                .get(&source)
                .cloned()
                .unwrap_or_default(),
            HashSet::from_iter([holder_a, holder_b]),
        );

        index.remove(holder_a);

        assert!(!index.payload_sources_by_holder.contains_key(&holder_a));
        assert_eq!(
            index
                .payload_holders_by_source
                .get(&source)
                .cloned()
                .unwrap_or_default(),
            HashSet::from_iter([holder_b]),
        );

        // The removed source held no payloads of its own; foreign holders that
        // still name it must stay so a later remap can report the loss.
        index.remove(source);

        assert_eq!(
            index
                .payload_holders_by_source
                .get(&source)
                .cloned()
                .unwrap_or_default(),
            HashSet::from_iter([holder_b]),
        );
        assert_eq!(
            index
                .payload_sources_by_holder
                .get(&holder_b)
                .cloned()
                .unwrap_or_default(),
            HashSet::from_iter([source]),
        );

        index.clear();
        assert!(index.payload_sources_by_holder.is_empty());
        assert!(index.payload_holders_by_source.is_empty());
    }
}
