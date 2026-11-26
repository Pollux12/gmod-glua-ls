use std::hash::Hash;

use glua_parser::LuaSyntaxId;
use rowan::TextRange;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use super::{
    DeclReferenceCell, FileReference, StringReference, decl_reference_fingerprint,
    global_reference_fingerprint, index_reference_fingerprint, string_reference_fingerprint,
};
use crate::{
    FileId,
    db_index::{LuaDeclId, LuaMemberKey, derivation::Fingerprint, intern::InternedName},
};

/// The sites under one name and their fingerprints' sum.
pub(super) struct NameSites<K> {
    pub(super) name: K,
    pub(super) sites: HashSet<LuaSyntaxId>,
    pub(super) fingerprint: Fingerprint,
}

/// Name-keyed sites in the order each name first appears.
pub(super) struct OrderedNameSites<K> {
    slots: HashMap<K, usize>,
    pub(super) entries: Vec<NameSites<K>>,
}

impl<K> Default for OrderedNameSites<K> {
    fn default() -> Self {
        Self {
            slots: HashMap::default(),
            entries: Vec::new(),
        }
    }
}

impl<K: Clone + Eq + Hash> OrderedNameSites<K> {
    /// Files `syntax_id` under `name`. Returns whether the site is new.
    fn insert(&mut self, name: K, syntax_id: LuaSyntaxId, fingerprint: Fingerprint) -> bool {
        let slot = match self.slots.get(&name) {
            Some(slot) => *slot,
            None => {
                let slot = self.entries.len();
                self.slots.insert(name.clone(), slot);
                self.entries.push(NameSites {
                    name,
                    sites: HashSet::default(),
                    fingerprint: 0,
                });
                slot
            }
        };
        let entry = &mut self.entries[slot];
        let inserted = entry.sites.insert(syntax_id);
        if inserted {
            entry.fingerprint = entry.fingerprint.wrapping_add(fingerprint);
        }
        inserted
    }
}

/// The references one file's source layer indexes, collected without touching
/// the workspace-wide index and filed by `LuaReferenceIndex::file_batch` in
/// one step. Filing it leaves the index exactly as the same calls to
/// `add_decl_reference`, `add_global_reference`, `add_index_reference` and
/// `add_string_reference` would, made in order on a file with no references.
pub struct FileReferenceBatch {
    file_id: FileId,
    decl_references: HashMap<LuaDeclId, (Vec<DeclReferenceCell>, bool)>,
    references_to_decl: HashMap<TextRange, LuaDeclId>,
    pub(super) globals: OrderedNameSites<InternedName>,
    pub(super) indexes: OrderedNameSites<LuaMemberKey>,
    strings: HashMap<InternedName, Vec<TextRange>>,
    pub(super) string_fingerprints: HashMap<InternedName, Fingerprint>,
    pub(super) fingerprint: Fingerprint,
    pub(super) fingerprinted: bool,
    pub(super) revisions: u64,
}

impl FileReferenceBatch {
    pub fn new(file_id: FileId) -> Self {
        Self {
            file_id,
            decl_references: HashMap::default(),
            references_to_decl: HashMap::default(),
            globals: OrderedNameSites::default(),
            indexes: OrderedNameSites::default(),
            strings: HashMap::default(),
            string_fingerprints: HashMap::default(),
            fingerprint: 0,
            fingerprinted: false,
            revisions: 0,
        }
    }

    pub(super) fn file_id(&self) -> FileId {
        self.file_id
    }

    pub fn get_decl_id(&self, range: &TextRange) -> Option<LuaDeclId> {
        self.references_to_decl.get(range).copied()
    }

    pub(super) fn take_file_reference_and_strings(&mut self) -> (FileReference, StringReference) {
        let file_reference = FileReference::from_parts(
            std::mem::take(&mut self.decl_references),
            std::mem::take(&mut self.references_to_decl),
        );
        let strings = StringReference::from_parts(std::mem::take(&mut self.strings));
        (file_reference, strings)
    }

    fn add_fingerprint(&mut self, fingerprint: Fingerprint) {
        self.fingerprint = self.fingerprint.wrapping_add(fingerprint);
        self.fingerprinted = true;
    }

    pub fn add_decl_reference(&mut self, decl_id: LuaDeclId, range: TextRange, is_write: bool) {
        if let std::collections::hash_map::Entry::Vacant(slot) =
            self.references_to_decl.entry(range)
        {
            slot.insert(decl_id);
            let (cells, mutable) = self.decl_references.entry(decl_id).or_default();
            if is_write {
                *mutable = true;
            }
            cells.push(DeclReferenceCell { range, is_write });
            self.add_fingerprint(decl_reference_fingerprint(
                self.file_id,
                decl_id,
                range,
                is_write,
            ));
        }
        self.revisions += 1;
    }

    pub fn add_global_reference(&mut self, name: &str, syntax_id: LuaSyntaxId) {
        let name = InternedName::new(name);
        let fingerprint = global_reference_fingerprint(self.file_id, &name, &syntax_id);
        if self.globals.insert(name, syntax_id, fingerprint) {
            self.add_fingerprint(fingerprint);
        }
        self.revisions += 1;
    }

    pub fn add_index_reference(&mut self, key: LuaMemberKey, syntax_id: LuaSyntaxId) {
        let fingerprint = index_reference_fingerprint(self.file_id, &key, &syntax_id);
        if self.indexes.insert(key, syntax_id, fingerprint) {
            self.add_fingerprint(fingerprint);
        }
        self.revisions += 1;
    }

    pub fn add_string_reference(&mut self, string: &str, range: TextRange) {
        let fingerprint = string_reference_fingerprint(self.file_id, string, &range);
        self.add_fingerprint(fingerprint);
        let key = InternedName::new(string);
        let slot = self.string_fingerprints.entry(key.clone()).or_default();
        *slot = slot.wrapping_add(fingerprint);
        self.strings.entry(key).or_default().push(range);
    }
}
