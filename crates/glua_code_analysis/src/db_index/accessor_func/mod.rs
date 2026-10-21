use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::db_index::cow_shards::{CowCell, CowShards};
use glua_parser::LuaSyntaxId;
use smol_str::SmolStr;

use super::traits::LuaIndex;
use crate::{
    FileId, LuaSignatureId, LuaTypeDeclId,
    db_index::derivation::{self, FactKey, Fingerprint},
};

#[derive(Debug, Clone, Hash)]
pub struct AccessorFuncAnnotation {
    pub name_param_index: usize,
    pub file_id: FileId,
    pub accessors: Vec<AccessorTemplate>,
    /// The function the generator is, which a local name reaching it holds.
    pub signature_id: Option<LuaSignatureId>,
    /// Read off the function's body rather than declared by `@accessorfunc`, so the
    /// shape of the accessors it makes is not known.
    pub inferred: bool,
}

/// Whose method an accessor generator makes.
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum AccessorOwner {
    /// The table a call passes first, or the one it is called on.
    Receiver,
    /// The class a call to the global `callee` names by its first argument, `class_name`.
    NamedClass {
        callee: SmolStr,
        class_name: SmolStr,
    },
}

/// What an accessor generator puts before the name it is given.
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum AccessorPrefix {
    Literal(SmolStr),
    /// The string a call passes at this parameter, or the default when it passes none.
    ParamOr(usize, SmolStr),
}

/// One method an accessor generator makes for each name it is given.
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct AccessorTemplate {
    pub owner: AccessorOwner,
    pub prefix: AccessorPrefix,
}

impl AccessorTemplate {
    /// The `Get`/`Set` pair `@accessorfunc` makes on its receiver.
    pub fn receiver_get_set_pair() -> Vec<Self> {
        ["Get", "Set"]
            .into_iter()
            .map(|prefix| Self {
                owner: AccessorOwner::Receiver,
                prefix: AccessorPrefix::Literal(prefix.into()),
            })
            .collect()
    }
}

#[derive(Debug, Default, Clone)]
pub struct AccessorFuncAnnotationIndex {
    /// Each name's generators in canonical order: a call takes the first.
    by_name: CowCell<FxHashMap<SmolStr, Vec<AccessorFuncAnnotation>>>,
    by_file: CowCell<FxHashMap<FileId, Vec<SmolStr>>>,
    /// Each declaring file's `Vfs::file_order_key`, which orders its
    /// generators among those of the same name.
    file_order: CowCell<FxHashMap<FileId, Arc<str>>>,
}

impl AccessorFuncAnnotationIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a generator under `name`. `file_order_key` is the declaring
    /// file's `Vfs::file_order_key`.
    pub fn register(
        &mut self,
        name: SmolStr,
        annotation: AccessorFuncAnnotation,
        file_order_key: Arc<str>,
    ) {
        derivation::note_write(|| FactKey::AccessorFunc(name.clone()));
        let file_id = annotation.file_id;
        self.file_order.entry(file_id).or_insert(file_order_key);
        let file_order = &self.file_order;
        let annotations = self.by_name.entry(name.clone()).or_default();
        let at = annotations.partition_point(|existing| {
            annotation_sort_key(file_order, existing)
                <= annotation_sort_key(file_order, &annotation)
        });
        annotations.insert(at, annotation);
        self.by_file.entry(file_id).or_default().push(name);
    }

    /// Files each of `order_keys`' files under its new order key: its
    /// generators take the place among their names' that the key gives them.
    pub(crate) fn rekey_files(&mut self, order_keys: &[(FileId, Arc<str>)]) {
        for (file_id, order_key) in order_keys {
            if self
                .file_order
                .get(file_id)
                .is_none_or(|held| held == order_key)
            {
                continue;
            }
            self.file_order.insert(*file_id, order_key.clone());
            let mut names = self.by_file.get(file_id).cloned().unwrap_or_default();
            names.sort();
            names.dedup();
            let file_order = &self.file_order;
            for name in names {
                if let Some(annotations) = self.by_name.get_mut(&name) {
                    annotations.sort_by(|left, right| {
                        annotation_sort_key(file_order, left)
                            .cmp(&annotation_sort_key(file_order, right))
                    });
                }
                derivation::note_write(|| FactKey::AccessorFunc(name.clone()));
            }
        }
    }

    /// The `@accessorfunc` annotations this file declares, as
    /// `(function name, name parameter index)`.
    ///
    /// The index is consulted by name while analysing calls in *any* file, and
    /// decides which argument names the accessor - so which `Get*`/`Set*`
    /// members get synthesized on the owner.
    #[cfg(test)]
    pub fn annotations_in_file(&self, file_id: FileId) -> Vec<(&SmolStr, usize)> {
        let Some(names) = self.by_file.get(&file_id) else {
            return Vec::new();
        };
        names
            .iter()
            .filter_map(|name| {
                let annotation = self
                    .by_name
                    .get(name)?
                    .iter()
                    .find(|annotation| annotation.file_id == file_id)?;
                Some((name, annotation.name_param_index))
            })
            .collect()
    }

    pub fn contains_name(&self, name: &str) -> bool {
        derivation::note_read(|| FactKey::AccessorFunc(SmolStr::new(name)));
        self.by_name.contains_key(name)
    }

    pub fn get_annotations(&self, name: &str) -> Option<&Vec<AccessorFuncAnnotation>> {
        derivation::note_read(|| FactKey::AccessorFunc(SmolStr::new(name)));
        self.by_name.get(name)
    }
    pub fn annotation_entries(
        &self,
    ) -> impl Iterator<Item = (&SmolStr, &Vec<AccessorFuncAnnotation>)> {
        self.by_name.iter().inspect(|(name, _)| {
            derivation::note_read(|| FactKey::AccessorFunc((*name).clone()));
        })
    }

    pub fn annotation_files(&self) -> impl Iterator<Item = (&FileId, &Vec<SmolStr>)> {
        self.by_file.iter().inspect(|(_, names)| {
            for name in names.iter() {
                derivation::note_read(|| FactKey::AccessorFunc(name.clone()));
            }
        })
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::AccessorFunc(name) => derivation::fingerprint_opt(self.by_name.get(name)),
            _ => {
                debug_assert!(false, "{key:?} is not an accessor-func annotation fact");
                derivation::ABSENT
            }
        }
    }
}

/// Canonical order for a name's generators: the declaring file's path, then
/// the generator's position in it.
fn annotation_sort_key<'a>(
    file_order: &'a FxHashMap<FileId, Arc<str>>,
    annotation: &AccessorFuncAnnotation,
) -> (&'a str, Option<u32>, u32) {
    (
        file_order.get(&annotation.file_id).map_or("", |key| key),
        annotation
            .signature_id
            .map(|signature_id| u32::from(signature_id.get_position())),
        annotation.file_id.id,
    )
}

impl LuaIndex for AccessorFuncAnnotationIndex {
    fn remove(&mut self, file_id: FileId) {
        self.file_order.remove(&file_id);
        if let Some(names) = self.by_file.remove(&file_id) {
            for name in names {
                derivation::note_write(|| FactKey::AccessorFunc(name.clone()));
                if let Some(annotations) = self.by_name.get_mut(&name) {
                    annotations.retain(|annotation| annotation.file_id != file_id);
                    if annotations.is_empty() {
                        self.by_name.remove(&name);
                    }
                }
            }
        }
    }

    fn clear(&mut self) {
        if derivation::recording() {
            for name in self.by_name.keys() {
                derivation::note_write(|| FactKey::AccessorFunc(name.clone()));
            }
        }
        self.by_name.clear();
        self.by_file.clear();
        self.file_order.clear();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccessorFuncCallMetadata {
    pub syntax_id: LuaSyntaxId,
    pub members: Vec<AccessorMember>,
}

/// Positions hash as ones of the file whose calls are fingerprinted, so a
/// fingerprint taken with edits mapped back sees them where they stood.
impl std::hash::Hash for AccessorFuncCallMetadata {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        derivation::hash_local(&self.syntax_id, state);
        self.members.hash(state);
    }
}

/// A method an accessor generator call makes, declared at `syntax_id` of the call in
/// `slot`, which tells apart the members one node declares.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessorMember {
    pub owner_type_id: LuaTypeDeclId,
    pub name: SmolStr,
    pub signature: AccessorSignature,
    pub syntax_id: LuaSyntaxId,
    pub slot: u32,
}

impl std::hash::Hash for AccessorMember {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let Self {
            owner_type_id,
            name,
            signature,
            syntax_id,
            slot,
        } = self;
        owner_type_id.hash(state);
        name.hash(state);
        signature.hash(state);
        derivation::hash_local(syntax_id, state);
        slot.hash(state);
    }
}

/// The callable shape of a synthesized accessor.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum AccessorSignature {
    /// `@accessorfunc`'s getter: no arguments, any value.
    Getter,
    /// `@accessorfunc`'s setter: one value.
    Setter,
    /// An inferred generator's function, whose branches may take any arguments.
    Any,
}

#[derive(Debug, Default, Clone)]
pub struct AccessorFuncCallIndex {
    calls: CowShards<FileId, Vec<AccessorFuncCallMetadata>>,
}

impl AccessorFuncCallIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_call(&mut self, file_id: FileId, metadata: AccessorFuncCallMetadata) {
        derivation::note_write(|| FactKey::AccessorCalls(file_id));
        self.calls.entry(file_id).or_default().push(metadata);
    }

    /// Drops `file_id`'s accessor calls before the derivations that found
    /// them run again.
    pub(crate) fn reset_file(&mut self, file_id: FileId) {
        derivation::note_write(|| FactKey::AccessorCalls(file_id));
        self.calls.remove(&file_id);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&FileId, &Vec<AccessorFuncCallMetadata>)> {
        self.calls.iter().inspect(|(file_id, _)| {
            derivation::note_read(|| FactKey::AccessorCalls(**file_id));
        })
    }

    /// The accessor calls found in `file_id`.
    pub fn calls_in(&self, file_id: FileId) -> &[AccessorFuncCallMetadata] {
        derivation::note_read(|| FactKey::AccessorCalls(file_id));
        self.calls.get(&file_id).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::AccessorCalls(file_id) => derivation::fingerprint_opt(self.calls.get(file_id)),
            _ => {
                debug_assert!(false, "{key:?} is not an accessor-func call fact");
                derivation::ABSENT
            }
        }
    }
}

impl LuaIndex for AccessorFuncCallIndex {
    fn remove(&mut self, file_id: FileId) {
        derivation::note_write(|| FactKey::AccessorCalls(file_id));
        self.calls.remove(&file_id);
    }

    fn clear(&mut self) {
        if derivation::recording() {
            for file_id in self.calls.keys() {
                derivation::note_write(|| FactKey::AccessorCalls(*file_id));
            }
        }
        self.calls.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generator(file_id: FileId, name_param_index: usize) -> AccessorFuncAnnotation {
        AccessorFuncAnnotation {
            name_param_index,
            file_id,
            accessors: AccessorTemplate::receiver_get_set_pair(),
            signature_id: Some(LuaSignatureId::new(file_id, 4.into())),
            inferred: false,
        }
    }

    fn first_generator_file(index: &AccessorFuncAnnotationIndex) -> Option<FileId> {
        index
            .get_annotations("AddAccessor")?
            .first()
            .map(|annotation| annotation.file_id)
    }

    /// A name's generators are ordered by their files' paths, whatever order
    /// the files registered in, and follow a file that moves.
    #[test]
    fn a_names_generators_follow_their_files_paths() {
        let (registered_first, registered_later) = (FileId::new(1), FileId::new(2));
        let mut index = AccessorFuncAnnotationIndex::new();
        index.register(
            "AddAccessor".into(),
            generator(registered_first, 1),
            Arc::from("b.lua"),
        );
        index.register(
            "AddAccessor".into(),
            generator(registered_later, 2),
            Arc::from("a.lua"),
        );
        assert_eq!(first_generator_file(&index), Some(registered_later));

        index.rekey_files(&[(registered_later, Arc::from("c.lua"))]);

        assert_eq!(first_generator_file(&index), Some(registered_first));
    }
}

crate::db_index::cow_shards::task_fork_fields!(AccessorFuncAnnotationIndex {
    by_name,
    by_file,
    file_order,
});

crate::db_index::cow_shards::task_fork_fields!(AccessorFuncCallIndex { calls });
