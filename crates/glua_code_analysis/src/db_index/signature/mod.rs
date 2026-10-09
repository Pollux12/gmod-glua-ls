mod async_state;
mod gmod_domains;
#[allow(clippy::module_inception)]
mod signature;
mod signature_cast;

use crate::db_index::cow_shards::{CowCell, CowShards};
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
    collect_call_arg_roles_for_param, compute_valid_guard_signatures,
    find_best_call_arg_role_for_param, find_best_call_arg_role_from_type,
    find_best_direct_call_arg_role_for_param, find_best_direct_call_arg_role_from_type,
    find_signature_attribute_use, rebuild_effective_valid_guard_signatures,
    signature_attribute_uses, signature_has_net_op_metadata, signature_is_valid_guard_in_realm,
    signature_is_valid_guard_or_base_runtime_isvalid_in_realm, signature_net_bits_param_idx,
    signature_net_callback_param_idx, signature_net_payload, signature_net_send,
    signature_owner_for, signature_writes_global_roots, store_valid_guard_signatures,
    type_is_net_read_op,
};
pub(crate) use signature::callee_signature_ids;
pub use signature::{
    CALL_ARG_ATTRIBUTE, CALL_ARG_FIELD_ATTRIBUTE, LuaCallArgRole, LuaClassNameReturnAlias,
    LuaDocDefaultValue, LuaDocParamInfo, LuaDocReturnInfo, LuaGenericParamInfo, LuaNoDiscard,
    LuaOutParamInfo, LuaOutParamRoot, LuaReturnCorrelation, LuaSignature, LuaSignatureId,
    OVERLOAD_CALL_ARG_ATTRIBUTE, OVERLOAD_CALL_ARG_FIELD_ATTRIBUTE, ReturnTypeKind,
    SignaturePayloadRemap, SignatureReturnStatus, find_call_arg_role_from_type,
    visit_call_arg_roles_from_type,
};
pub use signature_cast::{LuaCastAction, LuaCastOp, LuaCastTarget, LuaSignatureCast};

use crate::db_index::derivation::{self, FactKey, Fingerprint};
use crate::{FileId, GmodStateMask, LuaType, db_index::LuaDeclId};

use super::traits::LuaIndex;

#[derive(Debug, Clone)]
pub struct LuaSignatureIndex {
    signatures: CowShards<LuaSignatureId, LuaSignature>,
    /// Each signature a derivation has changed since the last reset, as the
    /// source layer left it (`None`: the source layer has no such signature).
    /// A reset restores it.
    source_signatures: CowShards<LuaSignatureId, Option<LuaSignature>>,
    in_file_signatures: CowShards<FileId, HashSet<LuaSignatureId>>,
    local_func_decls: CowCell<HashMap<LuaSignatureId, LuaDeclId>>,
    effective_valid_guard_signatures: CowCell<HashMap<LuaSignatureId, GmodStateMask>>,
    /// Per-file valid-guard contributions feeding the effective set: file →
    /// the `(signature, mask)` pairs derived from that file's members. A file
    /// can contribute to another file's signature (a source function shadowing
    /// a metadata-bearing annotation), so removal drops these entries
    /// explicitly instead of filtering the effective map by signature file.
    valid_guard_file_contributions: CowShards<FileId, Vec<(LuaSignatureId, GmodStateMask)>>,
    /// The set of files in `valid_guard_file_contributions`.
    valid_guard_files: derivation::SetFingerprint,
    /// The names of the definitions each file's docs mark `valid_guard`.
    valid_guard_names: CowCell<HashMap<FileId, Vec<SmolStr>>>,
    /// How many marked definitions carry each name, over every file.
    valid_guard_name_counts: CowCell<HashMap<SmolStr, usize>>,
    inferred_positive_guards: CowShards<LuaSignatureId, LuaInferredPositiveGuard>,
    inferred_guard_owners: CowCell<HashMap<LuaSignatureId, LuaInferredGuardOwner>>,
    inferred_guard_facts: CowCell<HashMap<LuaInferredGuardOwner, LuaInferredPositiveGuard>>,
    inferred_guard_owners_by_file: CowCell<HashMap<FileId, HashSet<LuaInferredGuardOwner>>>,
    inferred_guard_consumers: CowCell<HashMap<LuaInferredGuardOwner, HashSet<FileId>>>,
    inferred_guard_dependencies: CowCell<HashMap<FileId, HashSet<LuaInferredGuardOwner>>>,
    inferred_return_dependencies: CowCell<HashMap<LuaSignatureId, HashSet<FileId>>>,
    inferred_return_consumers: CowCell<HashMap<FileId, HashSet<LuaSignatureId>>>,
    payload_sources_by_holder: CowCell<HashMap<FileId, HashSet<FileId>>>,
    payload_holders_by_source: CowCell<HashMap<FileId, HashSet<FileId>>>,
    inferred_positive_guards_changed: bool,
    receiver_out_param_member_names: CowCell<HashMap<String, usize>>,
    in_file_receiver_out_param_member_names: CowCell<HashMap<FileId, HashSet<String>>>,
}

/// A conservative true-branch parameter narrowing derived from a function body.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    pub(crate) fn note_return_write(&mut self, signature_id: LuaSignatureId) {
        derivation::note_write(|| FactKey::Signature(signature_id));
    }

    /// Records a parameter-contract change. Callers note only when the write
    /// added a novel fact.
    pub(crate) fn note_param_write(&mut self, signature_id: LuaSignatureId) {
        derivation::note_write(|| FactKey::Signature(signature_id));
    }

    pub fn new() -> Self {
        Self {
            signatures: Default::default(),
            source_signatures: Default::default(),
            in_file_signatures: Default::default(),
            local_func_decls: Default::default(),
            effective_valid_guard_signatures: Default::default(),
            valid_guard_file_contributions: Default::default(),
            valid_guard_files: derivation::SetFingerprint::default(),
            valid_guard_names: Default::default(),
            valid_guard_name_counts: Default::default(),

            inferred_positive_guards: Default::default(),
            inferred_guard_owners: Default::default(),
            inferred_guard_facts: Default::default(),
            inferred_guard_owners_by_file: Default::default(),
            inferred_guard_consumers: Default::default(),
            inferred_guard_dependencies: Default::default(),
            inferred_return_dependencies: Default::default(),
            inferred_return_consumers: Default::default(),
            payload_sources_by_holder: Default::default(),
            payload_holders_by_source: Default::default(),
            inferred_positive_guards_changed: false,
            receiver_out_param_member_names: Default::default(),
            in_file_receiver_out_param_member_names: Default::default(),
        }
    }

    /// Keeps the source layer's version of `signature_id` before a
    /// derivation first changes it.
    fn snapshot_source(&mut self, signature_id: LuaSignatureId) {
        if derivation::writes_source() {
            return;
        }
        if let std::collections::hash_map::Entry::Vacant(entry) =
            self.source_signatures.entry(signature_id)
        {
            entry.insert(self.signatures.get(&signature_id).cloned());
        }
    }

    /// Restores `signature_id` to what the source layer left.
    pub(crate) fn reset_signature(&mut self, signature_id: &LuaSignatureId) {
        let Some(source) = self.source_signatures.remove(signature_id) else {
            return;
        };
        match source {
            Some(signature) => {
                self.signatures.insert(*signature_id, signature);
            }
            None => {
                self.signatures.remove(signature_id);
                if let Some(ids) = self.in_file_signatures.get_mut(&signature_id.get_file_id()) {
                    ids.remove(signature_id);
                }
            }
        }
    }

    pub fn get_or_create(&mut self, signature_id: LuaSignatureId) -> &mut LuaSignature {
        self.snapshot_source(signature_id);
        derivation::note_write(|| FactKey::Signature(signature_id));
        if self
            .in_file_signatures
            .entry(signature_id.get_file_id())
            .or_default()
            .insert(signature_id)
        {
            derivation::note_write(|| FactKey::SignatureFile(signature_id.get_file_id()));
        }
        self.signatures.entry(signature_id).or_default()
    }

    pub fn get(&self, signature_id: &LuaSignatureId) -> Option<&LuaSignature> {
        derivation::note_signature_read(*signature_id);
        self.signatures.get(signature_id)
    }

    /// `signature_id`'s signature for a reader of what it declares only: its
    /// params with their docs, overloads, generic params and colon and vararg
    /// flags. Records a read of those alone, so what inference later resolves
    /// for the signature leaves the reader standing.
    pub fn get_declared(&self, signature_id: &LuaSignatureId) -> Option<&LuaSignature> {
        derivation::note_signature_decl_read(*signature_id);
        self.signatures.get(signature_id)
    }

    /// Whether `signature_id` returns `never`. Records a read of that alone, so
    /// a return type that settles on anything else leaves the reader standing.
    pub fn returns_never(&self, signature_id: &LuaSignatureId) -> bool {
        derivation::note_signature_returns_never_read(*signature_id);
        self.signatures
            .get(signature_id)
            .is_some_and(LuaSignature::returns_never)
    }

    /// Whether `signature_id` takes special-call params. Records a read of
    /// that alone, so what inference later resolves for the signature leaves
    /// the reader standing.
    pub fn has_special_call_params(&self, signature_id: &LuaSignatureId) -> bool {
        derivation::note_special_call_signature_read(*signature_id);
        self.signatures
            .get(signature_id)
            .is_some_and(LuaSignature::has_special_call_params)
    }

    pub fn get_mut(&mut self, signature_id: &LuaSignatureId) -> Option<&mut LuaSignature> {
        self.snapshot_source(*signature_id);
        derivation::note_write(|| FactKey::Signature(*signature_id));
        self.signatures.get_mut(signature_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&LuaSignatureId, &LuaSignature)> {
        self.signatures.iter()
    }

    pub fn local_func_decl_for(&self, signature_id: &LuaSignatureId) -> Option<LuaDeclId> {
        derivation::note_signature_decl_read(*signature_id);
        self.local_func_decls.get(signature_id).copied()
    }

    pub fn bind_local_func_decl(&mut self, signature_id: LuaSignatureId, decl_id: LuaDeclId) {
        if self.local_func_decls.get(&signature_id) != Some(&decl_id) {
            self.local_func_decls.insert(signature_id, decl_id);
            // `local_func_decl_for` records a declaration read: the write
            // lands on that signature's declaration.
            self.note_return_write(signature_id);
        }
    }

    pub fn clear_effective_valid_guard_signatures(&mut self) {
        for signature_id in self.effective_valid_guard_signatures.keys() {
            derivation::note_write(|| FactKey::ValidGuard(*signature_id));
        }
        self.effective_valid_guard_signatures.clear();
    }

    pub fn mark_effective_valid_guard_signature(
        &mut self,
        signature_id: LuaSignatureId,
        state_mask: GmodStateMask,
    ) {
        derivation::note_write(|| FactKey::ValidGuard(signature_id));
        self.effective_valid_guard_signatures
            .entry(signature_id)
            .and_modify(|existing| existing.insert(state_mask))
            .or_insert(state_mask);
    }

    pub fn is_effective_valid_guard_signature(&self, signature_id: &LuaSignatureId) -> bool {
        derivation::note_read(|| FactKey::ValidGuard(*signature_id));
        self.effective_valid_guard_signatures
            .contains_key(signature_id)
    }

    pub fn effective_valid_guard_signature_mask(
        &self,
        signature_id: &LuaSignatureId,
    ) -> Option<GmodStateMask> {
        derivation::note_read(|| FactKey::ValidGuard(*signature_id));
        self.effective_valid_guard_signatures
            .get(signature_id)
            .copied()
    }

    /// Stores one file's valid-guard signatures.
    pub(crate) fn store_valid_guard_file_contributions(
        &mut self,
        file_id: FileId,
        contributions: Vec<(LuaSignatureId, GmodStateMask)>,
    ) {
        derivation::note_write(|| FactKey::ValidGuardFile(file_id));
        if self
            .valid_guard_file_contributions
            .insert(file_id, contributions)
            .is_none()
        {
            self.valid_guard_files.insert(&file_id);
            derivation::note_write(|| FactKey::ValidGuardFiles);
        }
    }

    /// Records that `file_id`'s docs mark a definition named `name` as a
    /// valid guard.
    pub(crate) fn add_valid_guard_name(&mut self, file_id: FileId, name: SmolStr) {
        derivation::note_write(|| FactKey::ValidGuardName(name.clone()));
        *self
            .valid_guard_name_counts
            .entry(name.clone())
            .or_default() += 1;
        self.valid_guard_names
            .entry(file_id)
            .or_default()
            .push(name);
    }

    fn remove_valid_guard_names(&mut self, file_id: FileId) {
        for name in self.valid_guard_names.remove(&file_id).unwrap_or_default() {
            derivation::note_write(|| FactKey::ValidGuardName(name.clone()));
            let Some(count) = self.valid_guard_name_counts.get_mut(&name) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                self.valid_guard_name_counts.remove(&name);
            }
        }
    }

    /// Whether some definition named `name` is marked a valid guard.
    pub(crate) fn is_valid_guard_name(&self, name: &str) -> bool {
        derivation::note_read(|| FactKey::ValidGuardName(SmolStr::new(name)));
        self.valid_guard_name_counts.contains_key(name)
    }

    /// Drops one file's valid-guard signatures.
    pub(crate) fn remove_valid_guard_file_contributions(&mut self, file_id: FileId) {
        if self
            .valid_guard_file_contributions
            .remove(&file_id)
            .is_some()
        {
            self.valid_guard_files.remove(&file_id);
            derivation::note_write(|| FactKey::ValidGuardFile(file_id));
            derivation::note_write(|| FactKey::ValidGuardFiles);
        }
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
        derivation::note_read(|| FactKey::ValidGuardFiles);
        for (file_id, contributions) in &self.valid_guard_file_contributions {
            derivation::note_read(|| FactKey::ValidGuardFile(*file_id));
            for (signature_id, state_mask) in contributions {
                union(&mut effective, *signature_id, *state_mask);
            }
        }
        if derivation::recording() {
            for (signature_id, mask) in &effective {
                if self.effective_valid_guard_signatures.get(signature_id) != Some(mask) {
                    derivation::note_write(|| FactKey::ValidGuard(*signature_id));
                }
            }
            for signature_id in self.effective_valid_guard_signatures.keys() {
                if !effective.contains_key(signature_id) {
                    derivation::note_write(|| FactKey::ValidGuard(*signature_id));
                }
            }
        }
        self.effective_valid_guard_signatures = effective.into();
    }

    pub(crate) fn clear_valid_guard_contributions(&mut self) {
        self.valid_guard_file_contributions.clear();
        self.valid_guard_files = derivation::SetFingerprint::default();
    }

    pub fn set_inferred_positive_guard(
        &mut self,
        signature_id: LuaSignatureId,
        guard: LuaInferredPositiveGuard,
    ) {
        if self.inferred_positive_guards.get(&signature_id) != Some(&guard) {
            self.inferred_positive_guards_changed = true;
        }
        derivation::note_write(|| FactKey::InferredGuard(signature_id));
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
        derivation::note_write(|| FactKey::InferredGuard(signature_id));
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

    /// Drops the inferred guard a derivation published for `signature_id`,
    /// before that derivation runs again.
    pub(crate) fn reset_inferred_guard(&mut self, signature_id: &LuaSignatureId) {
        if self.inferred_positive_guards.remove(signature_id).is_some() {
            self.inferred_positive_guards_changed = true;
        }
        let Some(owner) = self.inferred_guard_owners.remove(signature_id) else {
            return;
        };
        self.inferred_guard_facts.remove(&owner);
        let file_id = owner.source_file_id();
        if let Some(owners) = self.inferred_guard_owners_by_file.get_mut(&file_id) {
            owners.remove(&owner);
            if owners.is_empty() {
                self.inferred_guard_owners_by_file.remove(&file_id);
            }
        }
    }

    pub fn inferred_positive_guard(
        &self,
        signature_id: &LuaSignatureId,
    ) -> Option<&LuaInferredPositiveGuard> {
        derivation::note_read(|| FactKey::InferredGuard(*signature_id));
        self.inferred_positive_guards.get(signature_id)
    }

    pub fn inferred_guard_owner(
        &self,
        signature_id: &LuaSignatureId,
    ) -> Option<&LuaInferredGuardOwner> {
        derivation::note_read(|| FactKey::InferredGuard(*signature_id));
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
    /// leaves that payload untouched and reports its fact instead.
    /// Coordinate-only moves never call `note_return_write`, so export hashing
    /// with the same `FileRemap` stays stable.
    pub(crate) fn remap_payload_types(&mut self, remap: &crate::FileRemap) -> Vec<crate::LostFact> {
        let mut candidates: Vec<FileId> = self
            .payload_holders_by_source
            .get(&remap.file_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        candidates.sort();
        let mut lost = Vec::new();
        for holder in candidates {
            if holder == remap.file_id {
                continue;
            }
            let (signature_ids, fact_owners) = self.holder_payload_keys(holder);
            for signature_id in signature_ids {
                derivation::note_write(|| FactKey::Signature(signature_id));
                derivation::note_write(|| FactKey::InferredGuard(signature_id));
                if let Some(signature) = self.signatures.get_mut(&signature_id) {
                    let outcome = signature.remap_payload_types(remap);
                    if outcome.lost {
                        lost.push(crate::LostFact::new(
                            holder,
                            FactKey::Signature(signature_id),
                        ));
                    }
                }
                if let Some(guard) = self.inferred_positive_guards.get_mut(&signature_id) {
                    let result =
                        crate::db_index::remap_identities_in_type(&guard.narrowed_type, remap);
                    if result.lost {
                        lost.push(crate::LostFact::new(
                            holder,
                            FactKey::InferredGuard(signature_id),
                        ));
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
                        lost.push(crate::LostFact::new(
                            holder,
                            FactKey::InferredGuard(owner.signature_id()),
                        ));
                    } else if let Some(new_ty) = result.typ {
                        guard.narrowed_type = new_ty;
                    }
                }
            }
        }
        lost
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
        let retain = |signature_id: &LuaSignatureId| {
            let keep = signature_id.get_file_id() != file_id;
            if !keep {
                derivation::note_write(|| FactKey::InferredGuard(*signature_id));
            }
            keep
        };
        self.inferred_positive_guards
            .retain(|signature_id, _| retain(signature_id));
        self.inferred_guard_owners
            .retain(|signature_id, _| retain(signature_id));
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

        derivation::note_write(|| FactKey::ReceiverOutParam(member_name.as_str().into()));
        *self
            .receiver_out_param_member_names
            .entry(member_name)
            .or_default() += 1;
    }

    pub fn has_receiver_out_param_member_name(&self, member_name: &str) -> bool {
        derivation::note_read(|| FactKey::ReceiverOutParam(member_name.into()));
        self.receiver_out_param_member_names
            .contains_key(member_name)
    }

    pub fn receiver_out_param_member_names(&self) -> impl Iterator<Item = &str> {
        self.receiver_out_param_member_names
            .keys()
            .map(String::as_str)
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::Signature(signature_id) => {
                let signature = self.signatures.get(signature_id);
                let local_func_decl = self.local_func_decls.get(signature_id);
                if signature.is_none() && local_func_decl.is_none() {
                    return derivation::ABSENT;
                }
                derivation::fingerprint_of(&(signature, local_func_decl))
            }
            FactKey::SpecialCallSignature(signature_id) => self
                .signatures
                .get(signature_id)
                .map_or(derivation::ABSENT, |signature| {
                    derivation::fingerprint_of(&signature.has_special_call_params())
                }),
            FactKey::SignatureReturnsNever(signature_id) => self
                .signatures
                .get(signature_id)
                .map_or(derivation::ABSENT, |signature| {
                    derivation::fingerprint_of(&signature.returns_never())
                }),
            FactKey::SignatureDecl(signature_id) => {
                let signature = self.signatures.get(signature_id);
                let local_func_decl = self.local_func_decls.get(signature_id);
                if signature.is_none() && local_func_decl.is_none() {
                    return derivation::ABSENT;
                }
                derivation::fingerprint_of(&(
                    signature.map(LuaSignature::declaration),
                    local_func_decl,
                ))
            }
            FactKey::ValidGuardFile(file_id) => {
                derivation::fingerprint_opt(self.valid_guard_file_contributions.get(file_id))
            }
            FactKey::ValidGuardFiles => self.valid_guard_files.get(),
            FactKey::ValidGuardName(name) => {
                if self.valid_guard_name_counts.contains_key(name) {
                    derivation::fingerprint_of(name)
                } else {
                    derivation::ABSENT
                }
            }
            FactKey::ValidGuard(signature_id) => {
                derivation::fingerprint_opt(self.effective_valid_guard_signatures.get(signature_id))
            }
            FactKey::InferredGuard(signature_id) => {
                let positive_guard = self.inferred_positive_guards.get(signature_id);
                let guard_owner = self.inferred_guard_owners.get(signature_id);
                if positive_guard.is_none() && guard_owner.is_none() {
                    return derivation::ABSENT;
                }
                derivation::fingerprint_of(&(positive_guard, guard_owner))
            }
            FactKey::SignatureFile(file_id) => {
                self.in_file_signatures
                    .get(file_id)
                    .map_or(derivation::ABSENT, |ids| {
                        let mut ids: Vec<_> = ids.iter().collect();
                        ids.sort_unstable_by_key(|id| u32::from(id.get_position()));
                        derivation::fingerprint_of(&ids)
                    })
            }
            FactKey::ReceiverOutParam(member_name) => {
                if self
                    .receiver_out_param_member_names
                    .contains_key(member_name.as_str())
                {
                    derivation::fingerprint_of(&true)
                } else {
                    derivation::ABSENT
                }
            }
            _ => {
                debug_assert!(false, "{key:?} is not a signature fact");
                derivation::ABSENT
            }
        }
    }

    pub fn get_file_signature_ids(&self, file_id: FileId) -> Option<&HashSet<LuaSignatureId>> {
        derivation::note_read(|| FactKey::SignatureFile(file_id));
        self.in_file_signatures.get(&file_id)
    }
}

impl LuaIndex for LuaSignatureIndex {
    fn remove(&mut self, file_id: FileId) {
        // Drop only the removed file's holder edges. Reverse edges where it is
        // the referenced source stay so a later remap can still report loss.
        self.drop_payload_holder_edges(file_id);
        self.clear_inferred_guard_dependencies(file_id);
        if let Some(signature_ids) = self.in_file_signatures.remove(&file_id) {
            derivation::note_write(|| FactKey::SignatureFile(file_id));
            for signature_id in signature_ids {
                derivation::note_write(|| FactKey::Signature(signature_id));
                self.clear_inferred_return_dependencies(signature_id);
                self.source_signatures.remove(&signature_id);
                if self.signatures.remove(&signature_id).is_some() {
                    self.note_return_write(signature_id);
                    self.note_param_write(signature_id);
                }
                if self.local_func_decls.remove(&signature_id).is_some() {
                    self.note_return_write(signature_id);
                }
            }
        }
        // Drop this file's valid-guard contributions explicitly and re-derive
        // the effective set from what remains: a removed file can contribute
        // to another file's signature (a source function shadowing a
        // metadata-bearing annotation), which a signature-file filter on the
        // effective map would leave stale.
        self.remove_valid_guard_file_contributions(file_id);
        self.remove_valid_guard_names(file_id);
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
                        derivation::note_write(|| {
                            FactKey::ReceiverOutParam(member_name.as_str().into())
                        });
                        self.receiver_out_param_member_names.remove(&member_name);
                    }
                    None => {}
                }
            }
        }

        // Also drop entries whose target decl lived in the removed file, even if
        // the signature key was not tracked in that file's signature set.
        let mut dropped_binds = Vec::new();
        self.local_func_decls.retain(|signature_id, decl_id| {
            let keep = decl_id.file_id != file_id;
            if !keep {
                dropped_binds.push(*signature_id);
            }
            keep
        });
        for signature_id in dropped_binds {
            self.note_return_write(signature_id);
        }
    }

    fn clear(&mut self) {
        if derivation::recording() {
            for file_id in self.in_file_signatures.keys() {
                derivation::note_write(|| FactKey::SignatureFile(*file_id));
            }
            for signature_id in self.signatures.keys().chain(self.local_func_decls.keys()) {
                derivation::note_write(|| FactKey::Signature(*signature_id));
            }
            for signature_id in self.effective_valid_guard_signatures.keys() {
                derivation::note_write(|| FactKey::ValidGuard(*signature_id));
            }
            for signature_id in self
                .inferred_positive_guards
                .keys()
                .chain(self.inferred_guard_owners.keys())
            {
                derivation::note_write(|| FactKey::InferredGuard(*signature_id));
            }
            for member_name in self.receiver_out_param_member_names.keys() {
                derivation::note_write(|| FactKey::ReceiverOutParam(member_name.as_str().into()));
            }
        }
        self.signatures.clear();
        self.source_signatures.clear();
        self.in_file_signatures.clear();
        self.local_func_decls.clear();
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

    fn returning(type_ref: LuaType) -> Vec<LuaDocReturnInfo> {
        vec![LuaDocReturnInfo {
            name: None,
            type_ref,
            default_value: None,
            description: None,
            attributes: None,
            return_kind: Default::default(),
        }]
    }

    /// A reader of whether a signature returns `never` records that alone,
    /// read out of the signature: its returns resolving to anything else hold
    /// it, resolving to `never` moves it.
    #[test]
    fn a_return_that_settles_short_of_never_holds_a_reader_of_never() {
        let file_id = FileId::new(3);
        let signature_id = LuaSignatureId::new(file_id, 8.into());
        let mut index = LuaSignatureIndex::new();
        index.get_or_create(signature_id);
        let _recording = derivation::begin(derivation::DerivationId::workspace(
            derivation::DerivationKind::GmodLoad,
        ));
        assert!(!index.returns_never(&signature_id));
        let reads = derivation::end().into_recorded().2;
        let [read] = reads.as_slice() else {
            panic!("expected one read, got {reads:?}");
        };
        let signature = FactKey::Signature(signature_id);
        assert_eq!(read.base(), Some(signature.clone()));
        let fingerprint = |index: &LuaSignatureIndex, key: &FactKey| {
            derivation::in_file(Some(file_id), || index.fact_fingerprint(key))
        };
        let (signature_seen, read_seen) =
            (fingerprint(&index, &signature), fingerprint(&index, read));

        let resolved = index.get_mut(&signature_id).expect("the signature");
        resolved.resolve_return = SignatureReturnStatus::InferResolve;
        resolved.return_docs = returning(LuaType::Boolean);
        assert_ne!(fingerprint(&index, &signature), signature_seen);
        assert_eq!(fingerprint(&index, read), read_seen);

        index
            .get_mut(&signature_id)
            .expect("the signature")
            .return_docs = returning(LuaType::Never);
        assert_ne!(fingerprint(&index, read), read_seen);
        assert!(index.returns_never(&signature_id));
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

crate::db_index::cow_shards::task_fork_fields!(LuaSignatureIndex {
    signatures,
    source_signatures,
    in_file_signatures,
    local_func_decls,
    effective_valid_guard_signatures,
    valid_guard_file_contributions,
    valid_guard_files,
    valid_guard_names,
    valid_guard_name_counts,
    inferred_positive_guards,
    inferred_guard_owners => |current, frozen, fork| {
        crate::db_index::cow_shards::merge_map(current, frozen, fork, crate::db_index::cow_shards::refuse)
    },
    inferred_guard_facts => |current, frozen, fork| {
        crate::db_index::cow_shards::merge_map(current, frozen, fork, crate::db_index::cow_shards::refuse)
    },
    inferred_guard_owners_by_file => |current, frozen, fork| {
        crate::db_index::cow_shards::merge_map(current, frozen, fork, crate::db_index::cow_shards::merge_set)
    },
    inferred_guard_consumers,
    inferred_guard_dependencies,
    inferred_return_dependencies,
    inferred_return_consumers,
    payload_sources_by_holder,
    payload_holders_by_source,
    inferred_positive_guards_changed,
    receiver_out_param_member_names,
    in_file_receiver_out_param_member_names,
});
