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

    fn signature_ids(&self) -> impl Iterator<Item = &LuaSignatureId> {
        self.declared
            .iter()
            .chain(self.inferred.iter())
            .chain(self.unique.iter())
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
    /// File -> the holders whose targets name a signature in it.
    referencing: CowShards<FileId, FxHashSet<FileId>>,
}

impl LuaCallTargetIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `file_id`'s call targets.
    pub fn set_file(&mut self, file_id: FileId, targets: FileCallTargets) {
        derivation::note_write(|| FactKey::CallTargets(file_id));
        let named = signature_files(&targets);
        let previous = self.files.insert(file_id, Arc::new(targets));
        self.relink(file_id, previous.as_deref(), named);
    }

    /// Re-files `holder` under the files its targets name now, given the
    /// targets it held before.
    fn relink(
        &mut self,
        holder: FileId,
        previous: Option<&FileCallTargets>,
        named: FxHashSet<FileId>,
    ) {
        let linked = previous.map(signature_files).unwrap_or_default();
        for file_id in linked.difference(&named) {
            if let Some(holders) = self.referencing.get_mut(file_id) {
                holders.remove(&holder);
                if holders.is_empty() {
                    self.referencing.remove(file_id);
                }
            }
        }
        for file_id in named.difference(&linked) {
            self.referencing.entry(*file_id).or_default().insert(holder);
        }
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
        let previous = self.files.remove(&file_id);
        self.relink(file_id, previous.as_deref(), FxHashSet::default());
    }

    /// Moves the signatures of `remap`'s file the targets name to their new
    /// place. A holder whose target names a signature the edit destroyed is
    /// reported for re-analysis instead.
    pub fn remap_file_identities(&mut self, remap: &crate::FileRemap) -> FxHashSet<FileId> {
        let mut dirty = FxHashSet::default();
        if remap.map.is_identity() {
            return dirty;
        }
        let mut holders = self
            .referencing
            .get(&remap.file_id)
            .map(|holders| holders.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        holders.sort_unstable();
        for holder in holders {
            let Some(targets) = self.files.get_mut(&holder) else {
                continue;
            };
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
                            dirty.insert(holder);
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
                derivation::note_write(|| FactKey::CallTargets(holder));
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

/// The files the signatures `targets` name lie in.
fn signature_files(targets: &FileCallTargets) -> FxHashSet<FileId> {
    targets
        .values()
        .flat_map(LuaCallTarget::signature_ids)
        .map(LuaSignatureId::get_file_id)
        .collect()
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
        if let Some(previous) = self.files.remove(&file_id) {
            derivation::note_write(|| FactKey::CallTargets(file_id));
            self.relink(file_id, Some(&previous), FxHashSet::default());
        }
    }

    fn clear(&mut self) {
        if derivation::recording() {
            for file_id in self.files.keys() {
                derivation::note_write(|| FactKey::CallTargets(*file_id));
            }
        }
        self.files.clear();
        self.referencing.clear();
    }
}

#[cfg(test)]
mod tests {
    use rowan::{TextRange, TextSize};
    use rustc_hash::{FxHashMap, FxHashSet};

    use super::{LuaCallTarget, LuaCallTargetIndex, signature_files};
    use crate::{FileId, FileRemap, LuaSignatureId, PositionMap, db_index::LuaIndex};

    /// `referencing` names exactly the holders a scan of every file's targets
    /// finds.
    fn assert_referencing_matches_a_scan(index: &LuaCallTargetIndex) {
        let mut expected: FxHashMap<FileId, FxHashSet<FileId>> = FxHashMap::default();
        for (holder, targets) in index.files.iter() {
            for file_id in signature_files(targets) {
                expected.entry(file_id).or_default().insert(*holder);
            }
        }
        let indexed: FxHashMap<_, _> = index
            .referencing
            .iter()
            .map(|(file_id, holders)| (*file_id, holders.clone()))
            .collect();
        assert_eq!(indexed, expected);
    }

    fn declared(file_id: FileId, position: u32) -> LuaCallTarget {
        LuaCallTarget {
            declared: Some(LuaSignatureId::new(file_id, TextSize::new(position))),
            ..LuaCallTarget::default()
        }
    }

    #[test]
    fn referencing_follows_sets_resets_remaps_and_removals() {
        let (callee, holder, other) = (FileId::new(1), FileId::new(2), FileId::new(3));
        let call = TextRange::new(TextSize::new(0), TextSize::new(4));
        let local_call = TextRange::new(TextSize::new(8), TextSize::new(12));
        let mut index = LuaCallTargetIndex::new();
        index.set_file(
            holder,
            FxHashMap::from_iter([
                (call, declared(callee, 10)),
                (local_call, declared(holder, 5)),
            ]),
        );
        index.set_file(other, FxHashMap::from_iter([(call, declared(callee, 20))]));
        assert_referencing_matches_a_scan(&index);

        index.set_file(holder, FxHashMap::from_iter([(call, declared(other, 3))]));
        assert_referencing_matches_a_scan(&index);

        let dirty = index.remap_file_identities(&FileRemap::unvalidated(
            callee,
            PositionMap::new("abc", "\nabc"),
        ));
        assert!(dirty.is_empty());
        assert_eq!(
            index.file(other).expect("targets")[&call].declared,
            Some(LuaSignatureId::new(callee, TextSize::new(21)))
        );
        assert_referencing_matches_a_scan(&index);

        index.reset_file(other);
        assert_referencing_matches_a_scan(&index);
        index.remove(holder);
        assert!(index.referencing.is_empty());
    }
}

crate::db_index::cow_shards::task_fork_fields!(LuaCallTargetIndex {
    files,
    referencing => crate::db_index::cow_shards::merge_set,
});
