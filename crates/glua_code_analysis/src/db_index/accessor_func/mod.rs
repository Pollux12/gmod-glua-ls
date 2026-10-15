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
    by_name: CowCell<FxHashMap<SmolStr, Vec<AccessorFuncAnnotation>>>,
    by_file: CowCell<FxHashMap<FileId, Vec<SmolStr>>>,
}

impl AccessorFuncAnnotationIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, name: SmolStr, annotation: AccessorFuncAnnotation) {
        derivation::note_write(|| FactKey::AccessorFunc(name.clone()));
        let file_id = annotation.file_id;
        self.by_name
            .entry(name.clone())
            .or_default()
            .push(annotation);
        self.by_file.entry(file_id).or_default().push(name);
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

impl LuaIndex for AccessorFuncAnnotationIndex {
    fn remove(&mut self, file_id: FileId) {
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
    }
}

#[derive(Debug, Clone, Hash, PartialEq)]
pub struct AccessorFuncCallMetadata {
    pub syntax_id: LuaSyntaxId,
    pub members: Vec<AccessorMember>,
}

/// A method an accessor generator call makes, declared at `syntax_id` of the call in
/// `slot`, which tells apart the members one node declares.
#[derive(Debug, Clone, Hash, PartialEq)]
pub struct AccessorMember {
    pub owner_type_id: LuaTypeDeclId,
    pub name: SmolStr,
    pub signature: AccessorSignature,
    pub syntax_id: LuaSyntaxId,
    pub slot: u32,
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

crate::db_index::cow_shards::task_fork_fields!(AccessorFuncAnnotationIndex { by_name, by_file });

crate::db_index::cow_shards::task_fork_fields!(AccessorFuncCallIndex { calls });
