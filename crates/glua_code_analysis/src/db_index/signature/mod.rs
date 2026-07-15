mod async_state;
mod gmod_domains;
#[allow(clippy::module_inception)]
mod signature;

use smol_str::SmolStr;
use std::collections::{HashMap, HashSet};

pub use async_state::AsyncState;
pub use gmod_domains::{
    GMOD_ATTR_SELF_CALL_VALID, GMOD_ATTR_SELF_GUARD, GMOD_ATTR_VALID_GUARD,
    GMOD_ATTR_WRITES_GLOBAL, GMOD_CALL_ARG_DOMAINS, GMOD_DOMAIN_CLASS_BASE, GMOD_DOMAIN_COLOR,
    GMOD_DOMAIN_CONCOMMAND, GMOD_DOMAIN_CONVAR, GMOD_DOMAIN_DERMA_SKIN, GMOD_DOMAIN_FILE_FIND,
    GMOD_DOMAIN_GAMEMODE, GMOD_DOMAIN_HOOK, GMOD_DOMAIN_LOAD, GMOD_DOMAIN_MEMBER_GUARD,
    GMOD_DOMAIN_NET_MESSAGE, GMOD_DOMAIN_NET_PAYLOAD, GMOD_DOMAIN_NETWORK_VAR,
    GMOD_DOMAIN_SELF_GUARD, GMOD_DOMAIN_TIMER, GMOD_DOMAIN_VALID_GUARD, GMOD_DOMAIN_VGUI_PANEL,
    GMOD_ROLE_EXISTS, GMOD_ROLE_REFERENCE, GMOD_ROLE_VGUI_PARENT, GMOD_ROLE_VGUI_PARENT_SELF,
    GMOD_SIGNATURE_METADATA_DOMAINS, attribute_use_write_global_root,
    collect_call_arg_roles_for_param, find_best_call_arg_role_for_param,
    find_best_call_arg_role_from_type, find_best_direct_call_arg_role_for_param,
    find_best_direct_call_arg_role_from_type, find_signature_attribute_use,
    rebuild_effective_valid_guard_signatures, signature_attribute_uses,
    signature_is_valid_guard_in_realm, signature_is_valid_guard_or_base_runtime_isvalid_in_realm,
    signature_owner_for, signature_writes_global_roots,
};
pub use signature::{
    CALL_ARG_ATTRIBUTE, CALL_ARG_FIELD_ATTRIBUTE, LuaCallArgRole, LuaDocDefaultValue,
    LuaDocParamInfo, LuaDocReturnInfo, LuaGenericParamInfo, LuaNoDiscard, LuaOutParamInfo,
    LuaOutParamRoot, LuaReturnCorrelation, LuaSignature, LuaSignatureId,
    OVERLOAD_CALL_ARG_ATTRIBUTE, OVERLOAD_CALL_ARG_FIELD_ATTRIBUTE, ReturnTypeKind,
    SignatureReturnStatus, find_call_arg_role_from_type, visit_call_arg_roles_from_type,
};

use crate::{FileId, GmodStateMask, LuaType, db_index::LuaDeclId};

use super::traits::LuaIndex;

#[derive(Debug)]
pub struct LuaSignatureIndex {
    signatures: HashMap<LuaSignatureId, LuaSignature>,
    in_file_signatures: HashMap<FileId, HashSet<LuaSignatureId>>,
    local_func_decls: HashMap<LuaSignatureId, LuaDeclId>,
    effective_valid_guard_signatures: HashMap<LuaSignatureId, GmodStateMask>,
    inferred_positive_guards: HashMap<LuaSignatureId, LuaInferredPositiveGuard>,
    inferred_guard_owners: HashMap<LuaSignatureId, LuaInferredGuardOwner>,
    inferred_guard_facts: HashMap<LuaInferredGuardOwner, LuaInferredPositiveGuard>,
    inferred_guard_owners_by_file: HashMap<FileId, HashSet<LuaInferredGuardOwner>>,
    inferred_guard_consumers: HashMap<LuaInferredGuardOwner, HashSet<FileId>>,
    inferred_guard_dependencies: HashMap<FileId, HashSet<LuaInferredGuardOwner>>,
    inferred_positive_guards_changed: bool,
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
    pub fn new() -> Self {
        Self {
            signatures: HashMap::new(),
            in_file_signatures: HashMap::new(),
            local_func_decls: HashMap::new(),
            effective_valid_guard_signatures: HashMap::new(),
            inferred_positive_guards: HashMap::new(),
            inferred_guard_owners: HashMap::new(),
            inferred_guard_facts: HashMap::new(),
            inferred_guard_owners_by_file: HashMap::new(),
            inferred_guard_consumers: HashMap::new(),
            inferred_guard_dependencies: HashMap::new(),
            inferred_positive_guards_changed: false,
            receiver_out_param_member_names: HashMap::new(),
            in_file_receiver_out_param_member_names: HashMap::new(),
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
        self.signatures.get(signature_id)
    }

    pub fn get_mut(&mut self, signature_id: &LuaSignatureId) -> Option<&mut LuaSignature> {
        self.signatures.get_mut(signature_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&LuaSignatureId, &LuaSignature)> {
        self.signatures.iter()
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
}

impl LuaIndex for LuaSignatureIndex {
    fn remove(&mut self, file_id: FileId) {
        self.clear_inferred_guard_dependencies(file_id);
        if let Some(signature_ids) = self.in_file_signatures.remove(&file_id) {
            for signature_id in signature_ids {
                self.signatures.remove(&signature_id);
                self.local_func_decls.remove(&signature_id);
            }
        }
        self.effective_valid_guard_signatures
            .retain(|signature_id, _| signature_id.get_file_id() != file_id);
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
        self.effective_valid_guard_signatures.clear();
        self.inferred_positive_guards.clear();
        self.inferred_guard_owners.clear();
        self.inferred_guard_facts.clear();
        self.inferred_guard_owners_by_file.clear();
        self.inferred_guard_consumers.clear();
        self.inferred_guard_dependencies.clear();
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
        index.set_inferred_guard_dependencies(consumer_file_id, HashSet::from([owner.clone()]));

        index.remove(consumer_file_id);

        assert!(index.inferred_guard_dependencies.is_empty());
        assert!(index.inferred_guard_consumers.is_empty());
        assert!(
            index
                .inferred_guard_consumers_for_files(&HashSet::from([source_file_id]))
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
        index.set_inferred_guard_dependencies(consumer_file_id, HashSet::from([owner]));

        index.remove(source_file_id);

        assert!(index.inferred_guard_facts.is_empty());
        assert!(index.inferred_guard_owners_by_file.is_empty());
        assert!(index.inferred_guard_dependencies.is_empty());
        assert!(index.inferred_guard_consumers.is_empty());
    }
}
