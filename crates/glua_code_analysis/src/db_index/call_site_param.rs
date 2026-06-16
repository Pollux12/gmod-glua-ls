use std::collections::HashMap;

use super::traits::LuaIndex;
use crate::{FileId, LuaSignatureId, LuaType};

#[derive(Debug, Clone)]
struct CallSiteParamContribution {
    signature_id: LuaSignatureId,
    param_idx: usize,
    param_type: LuaType,
}

#[derive(Debug, Default)]
pub struct CallSiteParamIndex {
    /// file → source function access paths declared by that file.
    file_source_signatures: HashMap<FileId, Vec<(String, LuaSignatureId)>>,
    /// access path → current source function signature candidates.
    source_signatures_by_path: HashMap<String, Vec<LuaSignatureId>>,
    /// file → observed call-site param evidence contributed by calls in that file.
    file_contributions: HashMap<FileId, Vec<CallSiteParamContribution>>,
    /// signature → param index → union of all observed types from current file contributions.
    inferred_params: HashMap<LuaSignatureId, HashMap<usize, LuaType>>,
}

impl CallSiteParamIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_files_source_signatures(
        &mut self,
        updates: Vec<(FileId, Vec<(String, LuaSignatureId)>)>,
    ) {
        for (file_id, signatures) in updates {
            self.file_source_signatures.insert(file_id, signatures);
        }
        self.rebuild_source_signatures();
    }

    pub fn get_source_signature_for_file(
        &self,
        path: &str,
        file_id: FileId,
    ) -> Option<LuaSignatureId> {
        self.source_signatures_by_path
            .get(path)?
            .iter()
            .rev()
            .copied()
            .find(|signature_id| signature_id.get_file_id() == file_id)
    }

    pub fn set_files_contributions(
        &mut self,
        updates: Vec<(FileId, Vec<(LuaSignatureId, usize, LuaType)>)>,
    ) {
        for (file_id, contributions) in updates {
            self.file_contributions.insert(
                file_id,
                contributions
                    .into_iter()
                    .map(
                        |(signature_id, param_idx, param_type)| CallSiteParamContribution {
                            signature_id,
                            param_idx,
                            param_type,
                        },
                    )
                    .collect(),
            );
        }
        self.rebuild_derived_state();
    }

    pub fn get_inferred_param(
        &self,
        signature_id: &LuaSignatureId,
        param_idx: usize,
    ) -> Option<&LuaType> {
        self.inferred_params
            .get(signature_id)
            .and_then(|params| params.get(&param_idx))
    }

    fn rebuild_derived_state(&mut self) {
        self.inferred_params.clear();

        for contribution in self.file_contributions.values().flatten() {
            self.inferred_params
                .entry(contribution.signature_id)
                .or_default()
                .entry(contribution.param_idx)
                .and_modify(|current| {
                    *current =
                        LuaType::from_vec(vec![current.clone(), contribution.param_type.clone()])
                })
                .or_insert_with(|| contribution.param_type.clone());
        }
    }

    fn rebuild_source_signatures(&mut self) {
        self.source_signatures_by_path.clear();

        let mut file_ids = self
            .file_source_signatures
            .keys()
            .copied()
            .collect::<Vec<_>>();
        file_ids.sort_by_key(|file_id| file_id.id);
        for file_id in file_ids {
            let Some(signatures) = self.file_source_signatures.get(&file_id) else {
                continue;
            };
            for (path, signature_id) in signatures {
                self.source_signatures_by_path
                    .entry(path.clone())
                    .or_default()
                    .push(*signature_id);
            }
        }
    }
}

impl LuaIndex for CallSiteParamIndex {
    fn remove(&mut self, file_id: FileId) {
        self.file_source_signatures.remove(&file_id);
        self.file_contributions.remove(&file_id);

        self.rebuild_derived_state();
        self.rebuild_source_signatures();
    }

    fn clear(&mut self) {
        self.file_source_signatures.clear();
        self.source_signatures_by_path.clear();
        self.file_contributions.clear();
        self.inferred_params.clear();
    }
}
