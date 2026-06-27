mod async_state;
mod gmod_domains;
#[allow(clippy::module_inception)]
mod signature;

use std::collections::{HashMap, HashSet};

pub use async_state::AsyncState;
pub use gmod_domains::{
    GMOD_ATTR_SELF_CALL_VALID, GMOD_ATTR_SELF_GUARD, GMOD_ATTR_VALID_GUARD, GMOD_CALL_ARG_DOMAINS,
    GMOD_DOMAIN_CLASS_BASE, GMOD_DOMAIN_COLOR, GMOD_DOMAIN_CONCOMMAND, GMOD_DOMAIN_CONVAR,
    GMOD_DOMAIN_DERMA_SKIN, GMOD_DOMAIN_FILE_FIND, GMOD_DOMAIN_GAMEMODE, GMOD_DOMAIN_HOOK,
    GMOD_DOMAIN_LOAD, GMOD_DOMAIN_MEMBER_GUARD, GMOD_DOMAIN_NET_MESSAGE, GMOD_DOMAIN_NET_PAYLOAD,
    GMOD_DOMAIN_NETWORK_VAR, GMOD_DOMAIN_SELF_GUARD, GMOD_DOMAIN_TIMER, GMOD_DOMAIN_VALID_GUARD,
    GMOD_DOMAIN_VGUI_PANEL, GMOD_ROLE_EXISTS, GMOD_ROLE_REFERENCE, GMOD_SIGNATURE_METADATA_DOMAINS,
    collect_call_arg_roles_for_param, find_best_call_arg_role_for_param,
    find_best_call_arg_role_from_type, find_signature_attribute_use,
    semantic_decl_signature_has_valid_guard_metadata, semantic_decl_signature_is_valid_guard,
    signature_attribute_uses, signature_is_valid_guard,
    signature_is_valid_guard_or_base_runtime_isvalid, signature_owner_for,
};
pub use signature::{
    CALL_ARG_ATTRIBUTE, CALL_ARG_FIELD_ATTRIBUTE, LuaCallArgRole, LuaDocDefaultValue,
    LuaDocParamInfo, LuaDocReturnInfo, LuaGenericParamInfo, LuaNoDiscard, LuaOutParamInfo,
    LuaOutParamRoot, LuaReturnCorrelation, LuaSignature, LuaSignatureId,
    OVERLOAD_CALL_ARG_ATTRIBUTE, OVERLOAD_CALL_ARG_FIELD_ATTRIBUTE, ReturnTypeKind,
    SignatureReturnStatus, find_call_arg_role_from_type, visit_call_arg_roles_from_type,
};

use crate::{FileId, db_index::LuaDeclId};

use super::traits::LuaIndex;

#[derive(Debug)]
pub struct LuaSignatureIndex {
    signatures: HashMap<LuaSignatureId, LuaSignature>,
    in_file_signatures: HashMap<FileId, HashSet<LuaSignatureId>>,
    local_func_decls: HashMap<LuaSignatureId, LuaDeclId>,
    receiver_out_param_member_names: HashMap<String, usize>,
    in_file_receiver_out_param_member_names: HashMap<FileId, HashSet<String>>,
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
        if let Some(signature_ids) = self.in_file_signatures.remove(&file_id) {
            for signature_id in signature_ids {
                self.signatures.remove(&signature_id);
                self.local_func_decls.remove(&signature_id);
            }
        }

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
        self.receiver_out_param_member_names.clear();
        self.in_file_receiver_out_param_member_names.clear();
    }
}
