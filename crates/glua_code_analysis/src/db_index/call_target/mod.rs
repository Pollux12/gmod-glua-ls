//! What each call's callee resolves to, so the passes that read it do not
//! resolve every callee again.

use crate::db_index::cow_shards::CowShards;
use std::sync::Arc;

use rowan::TextRange;
use rustc_hash::{FxHashMap, FxHashSet};

use super::derivation::{self, FactKey, Fingerprint};
use super::traits::LuaIndex;
use crate::{FileId, InFiled, LuaSignatureId, LuaType};

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
    /// For a call that emits a hook by a literal name, the callbacks `hook.Add`
    /// registers under that name, sorted.
    pub hook_callbacks: Vec<LuaSignatureId>,
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
            hook_callbacks: Vec::new(),
        }
    }

    /// The target with the hook callbacks the call emits to.
    pub fn with_hook_callbacks(mut self, mut hook_callbacks: Vec<LuaSignatureId>) -> Self {
        hook_callbacks.sort_unstable_by_key(|signature_id| {
            (signature_id.get_file_id().id, signature_id.get_position())
        });
        hook_callbacks.dedup();
        self.hook_callbacks = hook_callbacks;
        self
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
            .chain(self.hook_callbacks.iter())
    }

    fn signature_ids_mut(&mut self) -> impl Iterator<Item = &mut LuaSignatureId> {
        self.declared
            .iter_mut()
            .chain(self.inferred.iter_mut())
            .chain(self.unique.iter_mut())
            .chain(self.hook_callbacks.iter_mut())
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

/// One file's call targets, by call range, and how many signature ids they
/// name in each file.
#[derive(Debug, Clone, Default, PartialEq)]
struct FileCallTargets {
    targets: FxHashMap<TextRange, LuaCallTarget>,
    named: FxHashMap<FileId, usize>,
}

impl FileCallTargets {
    /// Counts the signatures `target` names, adding to `linked` each file
    /// named for the first time.
    fn name(&mut self, target: &LuaCallTarget, linked: &mut Vec<FileId>) {
        for signature_id in target.signature_ids() {
            let count = self.named.entry(signature_id.get_file_id()).or_default();
            *count += 1;
            if *count == 1 {
                linked.push(signature_id.get_file_id());
            }
        }
    }

    /// Uncounts the signatures `target` names, adding to `unlinked` each file
    /// no target names any more.
    fn unname(&mut self, target: &LuaCallTarget, unlinked: &mut Vec<FileId>) {
        for signature_id in target.signature_ids() {
            let file_id = signature_id.get_file_id();
            let Some(count) = self.named.get_mut(&file_id) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                self.named.remove(&file_id);
                unlinked.push(file_id);
            }
        }
    }
}

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

    /// Stores what the call at `range` of `file_id` targets.
    pub fn set_target(&mut self, file_id: FileId, range: TextRange, target: LuaCallTarget) {
        derivation::note_write(|| FactKey::CallTarget(InFiled::new(file_id, range)));
        let held = Arc::make_mut(self.files.entry(file_id).or_default());
        let (mut linked, mut unlinked) = (Vec::new(), Vec::new());
        held.name(&target, &mut linked);
        if let Some(previous) = held.targets.insert(range, target) {
            held.unname(&previous, &mut unlinked);
        }
        self.relink(file_id, linked, unlinked);
    }

    /// Re-files `holder` under the files its targets came to name and no
    /// longer name.
    fn relink(&mut self, holder: FileId, linked: Vec<FileId>, unlinked: Vec<FileId>) {
        for file_id in unlinked {
            if let Some(holders) = self.referencing.get_mut(&file_id) {
                holders.remove(&holder);
                if holders.is_empty() {
                    self.referencing.remove(&file_id);
                }
            }
        }
        for file_id in linked {
            self.referencing.entry(file_id).or_default().insert(holder);
        }
    }

    /// What the call at `range` of `file_id` targets.
    pub fn target(&self, file_id: FileId, range: TextRange) -> Option<&LuaCallTarget> {
        derivation::note_read(|| FactKey::CallTarget(InFiled::new(file_id, range)));
        self.files.get(&file_id)?.targets.get(&range)
    }

    /// Drops what the call at `range` of `file_id` targets, before the
    /// derivation that resolved it runs again.
    pub(crate) fn reset_target(&mut self, file_id: FileId, range: TextRange) {
        derivation::note_write(|| FactKey::CallTarget(InFiled::new(file_id, range)));
        let Some(held) = self.files.get_mut(&file_id) else {
            return;
        };
        if !held.targets.contains_key(&range) {
            return;
        }
        let held = Arc::make_mut(held);
        let mut unlinked = Vec::new();
        if let Some(previous) = held.targets.remove(&range) {
            held.unname(&previous, &mut unlinked);
        }
        if held.targets.is_empty() {
            self.files.remove(&file_id);
        }
        self.relink(file_id, Vec::new(), unlinked);
    }

    /// Moves the signatures of `remap`'s file the targets name to their new
    /// place. A holder whose target names a signature the edit destroyed has
    /// its targets reported to be resolved again instead.
    pub fn remap_file_identities(&mut self, remap: &crate::FileRemap) -> Vec<crate::LostFact> {
        let mut lost = Vec::new();
        if remap.map.is_identity() {
            return lost;
        }
        let mut holders = self
            .referencing
            .get(&remap.file_id)
            .map(|holders| holders.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        holders.sort_unstable();
        for holder in holders {
            let Some(held) = self.files.get(&holder) else {
                continue;
            };
            let mut moved = Vec::new();
            for (range, target) in &held.targets {
                let mut remapped = target.clone();
                let mut changed = false;
                let mut named_lost = false;
                for signature_id in remapped.signature_ids_mut() {
                    match remap.signature_id(*signature_id) {
                        crate::Remap::Moved(new) if new != *signature_id => {
                            *signature_id = new;
                            changed = true;
                        }
                        crate::Remap::Lost => named_lost = true,
                        crate::Remap::Moved(_) | crate::Remap::Unrelated => {}
                    }
                }
                if named_lost {
                    lost.push(crate::LostFact::new(
                        holder,
                        FactKey::CallTarget(InFiled::new(holder, *range)),
                    ));
                }
                if changed {
                    moved.push((*range, remapped));
                }
            }
            moved.sort_unstable_by_key(|(range, _)| (range.start(), range.end()));
            for (range, remapped) in moved {
                self.set_target(holder, range, remapped);
            }
        }
        lost
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::CallTarget(site) => self
                .files
                .get(&site.file_id)
                .and_then(|held| held.targets.get(&site.value))
                .map_or(derivation::ABSENT, |target| {
                    derivation::in_file(Some(site.file_id), || derivation::fingerprint_of(target))
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
            .map(|(file_id, held)| (*file_id, targets_fingerprint(*file_id, &held.targets)))
    }
}

fn targets_fingerprint(
    file_id: FileId,
    targets: &FxHashMap<TextRange, LuaCallTarget>,
) -> Fingerprint {
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
        if let Some(held) = self.files.remove(&file_id) {
            for range in held.targets.keys() {
                derivation::note_write(|| FactKey::CallTarget(InFiled::new(file_id, *range)));
            }
            self.relink(file_id, Vec::new(), held.named.keys().copied().collect());
        }
    }

    fn clear(&mut self) {
        if derivation::recording() {
            for (file_id, held) in self.files.iter() {
                for range in held.targets.keys() {
                    derivation::note_write(|| FactKey::CallTarget(InFiled::new(*file_id, *range)));
                }
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

    use super::{LuaCallTarget, LuaCallTargetIndex};
    use crate::{FileId, FileRemap, LuaSignatureId, PositionMap, db_index::LuaIndex};

    /// `referencing` names exactly the holders a scan of every file's targets
    /// finds.
    fn assert_referencing_matches_a_scan(index: &LuaCallTargetIndex) {
        let mut expected: FxHashMap<FileId, FxHashSet<FileId>> = FxHashMap::default();
        for (holder, held) in index.files.iter() {
            for signature_id in held.targets.values().flat_map(LuaCallTarget::signature_ids) {
                expected
                    .entry(signature_id.get_file_id())
                    .or_default()
                    .insert(*holder);
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
        index.set_target(holder, call, declared(callee, 10));
        index.set_target(holder, local_call, declared(holder, 5));
        index.set_target(other, call, declared(callee, 20));
        assert_referencing_matches_a_scan(&index);

        index.set_target(holder, call, declared(other, 3));
        assert_referencing_matches_a_scan(&index);

        let dirty = index.remap_file_identities(&FileRemap::unvalidated(
            callee,
            PositionMap::new("abc", "\nabc"),
        ));
        assert!(dirty.is_empty());
        assert_eq!(
            index.target(other, call).expect("target").declared,
            Some(LuaSignatureId::new(callee, TextSize::new(21)))
        );
        assert_referencing_matches_a_scan(&index);

        index.reset_target(other, call);
        assert!(index.target(other, call).is_none());
        assert_referencing_matches_a_scan(&index);
        index.remove(holder);
        assert!(index.referencing.is_empty());
    }
}

crate::db_index::cow_shards::task_fork_fields!(LuaCallTargetIndex {
    files,
    referencing => crate::db_index::cow_shards::merge_set,
});
