//! What each call's callee resolves to, one file at a time, so the per-file
//! passes read it instead of resolving every callee again.

use crate::db_index::cow_shards::CowShards;
use std::sync::Arc;

use rowan::TextRange;
use rustc_hash::{FxHashMap, FxHashSet};

use super::derivation::{self, FactKey, Fingerprint};
use super::traits::LuaIndex;
use crate::{FileId, LuaSignatureId, LuaType};

/// A call's callee.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct LuaCallTarget {
    /// The signature the declaration the prefix names is bound to.
    pub declared: Option<LuaSignatureId>,
    /// Every signature the prefix's inferred type names, through instances
    /// and unions, sorted.
    pub inferred: Vec<LuaSignatureId>,
    /// The one signature every callable member of the prefix's inferred type
    /// names, through type guards and `TableOf`.
    pub unique: Option<LuaSignatureId>,
}

impl LuaCallTarget {
    /// The target of a call whose prefix declaration is bound to `declared`
    /// and whose prefix infers as `prefix_type`.
    pub fn new(declared: Option<LuaSignatureId>, prefix_type: Option<&LuaType>) -> Self {
        let mut inferred = Vec::new();
        if let Some(prefix_type) = prefix_type {
            collect_signature_ids(prefix_type, &mut inferred);
        }
        inferred.sort_unstable_by_key(|signature_id| {
            (signature_id.get_file_id().id, signature_id.get_position())
        });
        inferred.dedup();
        Self {
            declared,
            inferred,
            unique: prefix_type.and_then(unique_signature_id),
        }
    }

    /// The one signature the call targets: the declared one, else the one
    /// every callable member of its inferred type names.
    pub fn signature_id(&self) -> Option<LuaSignatureId> {
        self.declared.or(self.unique)
    }

    fn signature_ids_mut(&mut self) -> impl Iterator<Item = &mut LuaSignatureId> {
        self.declared
            .iter_mut()
            .chain(self.inferred.iter_mut())
            .chain(self.unique.iter_mut())
    }
}

fn collect_signature_ids(typ: &LuaType, signature_ids: &mut Vec<LuaSignatureId>) {
    match typ {
        LuaType::Signature(signature_id) => signature_ids.push(*signature_id),
        LuaType::Instance(instance) => collect_signature_ids(instance.get_base(), signature_ids),
        LuaType::Union(union) => {
            for typ in union.types() {
                collect_signature_ids(typ, signature_ids);
            }
        }
        LuaType::MultiLineUnion(union) => {
            for (typ, _) in union.get_unions() {
                collect_signature_ids(typ, signature_ids);
            }
        }
        _ => {}
    }
}

fn unique_signature_id(typ: &LuaType) -> Option<LuaSignatureId> {
    match typ {
        LuaType::Signature(signature_id) => Some(*signature_id),
        LuaType::TypeGuard(inner) => unique_signature_id(inner),
        LuaType::TableOf(inner) => unique_signature_id(inner),
        LuaType::Union(union) => {
            let mut signature = None;
            for member in union.types() {
                let Some(candidate) = unique_signature_id(member) else {
                    continue;
                };
                if signature.is_some_and(|existing| existing != candidate) {
                    return None;
                }
                signature = Some(candidate);
            }
            signature
        }
        _ => None,
    }
}

/// One file's call targets, by call range.
pub type FileCallTargets = FxHashMap<TextRange, LuaCallTarget>;

#[derive(Debug, Clone, Default)]
pub struct LuaCallTargetIndex {
    files: CowShards<FileId, Arc<FileCallTargets>>,
}

impl LuaCallTargetIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `file_id`'s call targets.
    pub fn set_file(&mut self, file_id: FileId, targets: FileCallTargets) {
        derivation::note_write(|| FactKey::CallTargets(file_id));
        self.files.insert(file_id, Arc::new(targets));
    }

    /// `file_id`'s call targets.
    pub fn file(&self, file_id: FileId) -> Option<Arc<FileCallTargets>> {
        derivation::note_read(|| FactKey::CallTargets(file_id));
        self.files.get(&file_id).cloned()
    }

    /// Drops `file_id`'s call targets, before the derivation that resolved
    /// them runs again.
    pub(crate) fn reset_file(&mut self, file_id: FileId) {
        derivation::note_write(|| FactKey::CallTargets(file_id));
        self.files.remove(&file_id);
    }

    /// Moves the signatures of `remap`'s file the targets name to their new
    /// place. A holder whose target names a signature the edit destroyed is
    /// reported for re-analysis instead.
    pub fn remap_file_identities(&mut self, remap: &crate::FileRemap) -> FxHashSet<FileId> {
        let mut dirty = FxHashSet::default();
        if remap.map.is_identity() {
            return dirty;
        }
        for (holder, targets) in self.files.iter_mut() {
            let mut moved = None::<FileCallTargets>;
            for (range, target) in targets.iter() {
                let mut remapped = target.clone();
                let mut changed = false;
                for signature_id in remapped.signature_ids_mut() {
                    match remap.signature_id(*signature_id) {
                        crate::Remap::Moved(new) if new != *signature_id => {
                            *signature_id = new;
                            changed = true;
                        }
                        crate::Remap::Lost => {
                            dirty.insert(*holder);
                        }
                        crate::Remap::Moved(_) | crate::Remap::Unrelated => {}
                    }
                }
                if changed {
                    moved
                        .get_or_insert_with(|| FileCallTargets::clone(targets))
                        .insert(*range, remapped);
                }
            }
            if let Some(moved) = moved {
                *targets = Arc::new(moved);
                derivation::note_write(|| FactKey::CallTargets(*holder));
            }
        }
        dirty
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::CallTargets(file_id) => self
                .files
                .get(file_id)
                .map_or(derivation::ABSENT, |targets| {
                    targets_fingerprint(*file_id, targets)
                }),
            _ => {
                debug_assert!(false, "{key:?} is not a call target fact");
                derivation::ABSENT
            }
        }
    }

    /// Each file's targets as one fingerprint, for comparing two analyses.
    pub fn file_fingerprints(&self) -> impl Iterator<Item = (FileId, Fingerprint)> + '_ {
        self.files
            .iter()
            .map(|(file_id, targets)| (*file_id, targets_fingerprint(*file_id, targets)))
    }
}

fn targets_fingerprint(file_id: FileId, targets: &FileCallTargets) -> Fingerprint {
    derivation::in_file(Some(file_id), || {
        targets
            .iter()
            .map(|(range, target)| derivation::fingerprint_of(&(derivation::Local(range), target)))
            .fold(0u64, u64::wrapping_add)
            .max(1)
    })
}

impl LuaIndex for LuaCallTargetIndex {
    fn remove(&mut self, file_id: FileId) {
        if self.files.remove(&file_id).is_some() {
            derivation::note_write(|| FactKey::CallTargets(file_id));
        }
    }

    fn clear(&mut self) {
        if derivation::recording() {
            for file_id in self.files.keys() {
                derivation::note_write(|| FactKey::CallTargets(*file_id));
            }
        }
        self.files.clear();
    }
}

crate::db_index::cow_shards::task_fork_fields!(LuaCallTargetIndex { files });
