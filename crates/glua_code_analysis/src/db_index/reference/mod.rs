mod batch;
mod file_reference;
mod string_reference;

use crate::db_index::cow_shards::CowCell;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

pub use batch::FileReferenceBatch;
pub use file_reference::{DeclReference, DeclReferenceCell, FileReference};
use glua_parser::LuaSyntaxId;
use rowan::{TextRange, TextSize};
use smol_str::SmolStr;
pub use string_reference::StringReference;

use super::{LuaDeclId, LuaMemberKey, LuaTypeDeclId, traits::LuaIndex};
use crate::{
    FileId, InFiled,
    db_index::derivation::{self, DerivationId, FactKey, Fingerprint},
};

#[derive(Debug, Clone)]
pub struct LuaReferenceIndex {
    file_references: CowCell<HashMap<FileId, FileReference>>,
    /// Member-key reference sites: key → file → syntax ids in source order.
    index_reference: CowCell<HashMap<LuaMemberKey, HashMap<FileId, Vec<LuaSyntaxId>>>>,
    /// Global-name reference sites: name → file → syntax ids in source order.
    global_references: CowCell<HashMap<SmolStr, HashMap<FileId, Vec<LuaSyntaxId>>>>,
    string_references: CowCell<HashMap<FileId, StringReference>>,
    /// Doc type-name reference sites: file → type decl → ranges in source order.
    type_references: CowCell<HashMap<FileId, HashMap<LuaTypeDeclId, Vec<TextRange>>>>,
    /// Where each of a file's locals is first reassigned, computed once when the
    /// file is indexed.
    local_reassignments: CowCell<HashMap<FileId, HashMap<LuaDeclId, TextSize>>>,
    /// Bumped whenever a file's references change, so a memo over facts derived
    /// from one file's references can tell a hit from a stale entry. Values come
    /// from a counter that never restarts, so a revision is never reused after a
    /// removal.
    file_revision: CowCell<HashMap<FileId, u64>>,
    next_revision: u64,
    /// Incremental multiset fingerprint of each file's reference entries.
    file_fingerprints: CowCell<HashMap<FileId, Fingerprint>>,
    /// Incremental multiset fingerprint of the name-keyed entries (global,
    /// member-key, string and type references) under each name.
    name_fingerprints: CowCell<HashMap<SmolStr, Fingerprint>>,
    /// Member-key references derivations found once a key was inferred, with
    /// how many derivations hold each; kept apart from what the source layer
    /// indexed so a re-run retracts exactly its own.
    derived_index_reference: CowCell<HashMap<LuaMemberKey, HashMap<(FileId, LuaSyntaxId), usize>>>,
    /// Incremental multiset fingerprint of the derived references in each file.
    derived_file_fingerprints: CowCell<HashMap<FileId, Fingerprint>>,
    /// The member-key references each derivation added.
    derived_index_reference_holders:
        CowCell<HashMap<DerivationId, Vec<(LuaMemberKey, FileId, LuaSyntaxId)>>>,
}

impl Default for LuaReferenceIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaReferenceIndex {
    pub fn new() -> Self {
        Self {
            file_references: Default::default(),
            index_reference: Default::default(),
            global_references: Default::default(),
            string_references: Default::default(),
            type_references: Default::default(),
            local_reassignments: Default::default(),
            file_revision: Default::default(),
            next_revision: 1,
            file_fingerprints: Default::default(),
            name_fingerprints: Default::default(),
            derived_index_reference: Default::default(),
            derived_file_fingerprints: Default::default(),
            derived_index_reference_holders: Default::default(),
        }
    }

    /// Withdraws the member-key references derivation `id` added, before it
    /// runs again.
    pub(crate) fn retract_index_references(&mut self, id: &DerivationId) {
        for (key, file_id, syntax_id) in self
            .derived_index_reference_holders
            .remove(id)
            .unwrap_or_default()
        {
            let Some(sites) = self.derived_index_reference.get_mut(&key) else {
                continue;
            };
            let Some(count) = sites.get_mut(&(file_id, syntax_id)) else {
                continue;
            };
            *count -= 1;
            if *count > 0 {
                continue;
            }
            sites.remove(&(file_id, syntax_id));
            if sites.is_empty() {
                self.derived_index_reference.remove(&key);
            }
            let name = member_key_name(&key);
            let fingerprint = index_reference_fingerprint(file_id, &key, &syntax_id);
            Self::sub_name_fingerprint(&mut self.name_fingerprints, &name, fingerprint);
            if let Some(sum) = self.derived_file_fingerprints.get_mut(&file_id) {
                *sum = sum.wrapping_sub(fingerprint);
                if *sum == derivation::ABSENT {
                    self.derived_file_fingerprints.remove(&file_id);
                }
            }
        }
    }

    /// Revision of `file_id`'s references. Zero until the file records one.
    pub fn file_reference_revision(&self, file_id: FileId) -> u64 {
        derivation::note_file_fact_read(derivation::FileFact::References, file_id);
        self.file_revision.get(&file_id).copied().unwrap_or(0)
    }

    /// Reserves capacity in the workspace-wide maps for one file's worth of
    /// reference writes, and creates the file's own entries.
    ///
    /// `entries` is a cheap upper-bound hint (see
    /// `crate::compilation::analyzer::common::estimate_file_index_entries`),
    /// not a count the index trusts: maps keep their exact contents, so the
    /// hint only changes rehashing, never facts. Distinct member keys are a
    /// fraction of reference writes, hence the scaled outer-map reservations.
    /// The file's own maps grow to what they hold: sized from an upper bound,
    /// they would keep the slack for as long as the file is indexed.
    /// `file_revision` is intentionally untouched: a revision is minted on the
    /// first real write, and reserving must not consume one.
    pub fn reserve_for_file(&mut self, file_id: FileId, entries: usize) {
        derivation::note_write(|| FactKey::References(file_id));
        self.file_references.entry(file_id).or_default();
        self.index_reference.reserve(entries / 4 + 1);
        self.global_references.reserve(entries / 4 + 1);
        self.string_references
            .entry(file_id)
            .or_insert_with(StringReference::new);
        self.type_references.entry(file_id).or_default();
    }

    fn bump_file_revision(&mut self, file_id: FileId) {
        let revision = self.next_revision;
        self.next_revision += 1;
        self.file_revision.insert(file_id, revision);
    }

    fn add_file_fingerprint(&mut self, file_id: FileId, fingerprint: Fingerprint) {
        let slot = self.file_fingerprints.entry(file_id).or_default();
        *slot = slot.wrapping_add(fingerprint);
    }

    fn add_name_fingerprint(&mut self, name: SmolStr, fingerprint: Fingerprint) {
        let slot = self.name_fingerprints.entry(name).or_default();
        *slot = slot.wrapping_add(fingerprint);
    }

    fn sub_name_fingerprint(
        name_fingerprints: &mut HashMap<SmolStr, Fingerprint>,
        name: &str,
        fingerprint: Fingerprint,
    ) {
        derivation::note_write(|| FactKey::ReferencesByName(SmolStr::new(name)));
        if let Some(slot) = name_fingerprints.get_mut(name) {
            *slot = slot.wrapping_sub(fingerprint);
            if *slot == 0 {
                name_fingerprints.remove(name);
            }
        }
    }

    fn remove_file_name_fingerprints(
        name_fingerprints: &mut HashMap<SmolStr, Fingerprint>,
        file_id: FileId,
        string_references: Option<StringReference>,
        type_references: Option<HashMap<LuaTypeDeclId, Vec<TextRange>>>,
    ) {
        for (string, ranges) in string_references
            .iter()
            .flat_map(StringReference::iter_string_references)
        {
            for range in ranges {
                Self::sub_name_fingerprint(
                    name_fingerprints,
                    string,
                    string_reference_fingerprint(file_id, string, range),
                );
            }
        }
        for (type_decl_id, ranges) in type_references.iter().flatten() {
            for range in ranges {
                Self::sub_name_fingerprint(
                    name_fingerprints,
                    type_decl_id.get_name(),
                    type_reference_fingerprint(file_id, type_decl_id, range),
                );
            }
        }
    }

    /// `file_id`'s entry fingerprints summed afresh, which is what the
    /// incremental `file_fingerprints` sum holds.
    fn recomputed_file_fingerprint(&self, file_id: FileId) -> Fingerprint {
        let decls = self
            .file_references
            .get(&file_id)
            .into_iter()
            .flat_map(|file| file.get_decl_references_map())
            .flat_map(|(decl_id, reference)| {
                reference.cells.iter().map(move |cell| {
                    decl_reference_fingerprint(file_id, *decl_id, cell.range, cell.is_write)
                })
            });
        let globals = self.global_references.iter().flat_map(|(name, by_file)| {
            by_file
                .get(&file_id)
                .into_iter()
                .flatten()
                .map(move |syntax_id| global_reference_fingerprint(file_id, name, syntax_id))
        });
        let indexes = self.index_reference.iter().flat_map(|(key, by_file)| {
            by_file
                .get(&file_id)
                .into_iter()
                .flatten()
                .map(move |syntax_id| index_reference_fingerprint(file_id, key, syntax_id))
        });
        let strings = self
            .string_references
            .get(&file_id)
            .into_iter()
            .flat_map(StringReference::iter_string_references)
            .flat_map(|(string, ranges)| {
                ranges
                    .iter()
                    .map(move |range| string_reference_fingerprint(file_id, string, range))
            });
        let types = self
            .type_references
            .get(&file_id)
            .into_iter()
            .flatten()
            .flat_map(|(type_decl_id, ranges)| {
                ranges
                    .iter()
                    .map(move |range| type_reference_fingerprint(file_id, type_decl_id, range))
            });
        let reassignments = self
            .local_reassignments
            .get(&file_id)
            .map(reassignments_fingerprint);
        decls
            .chain(globals)
            .chain(indexes)
            .chain(strings)
            .chain(types)
            .chain(reassignments)
            .fold(0, Fingerprint::wrapping_add)
    }

    /// `name`'s entry fingerprints summed afresh, which is what the
    /// incremental `name_fingerprints` sum holds.
    fn recomputed_name_fingerprint(&self, name: &SmolStr) -> Fingerprint {
        let globals = self
            .global_references
            .get(name)
            .into_iter()
            .flatten()
            .flat_map(|(file_id, syntax_ids)| {
                syntax_ids
                    .iter()
                    .map(|syntax_id| global_reference_fingerprint(*file_id, name, syntax_id))
            });
        let indexes = self
            .index_reference
            .iter()
            .filter(|(key, _)| member_key_name(key) == *name)
            .flat_map(|(key, by_file)| {
                by_file.iter().flat_map(move |(file_id, syntax_ids)| {
                    syntax_ids
                        .iter()
                        .map(move |syntax_id| index_reference_fingerprint(*file_id, key, syntax_id))
                })
            });
        let derived = self
            .derived_index_reference
            .iter()
            .filter(|(key, _)| member_key_name(key) == *name)
            .flat_map(|(key, sites)| {
                sites.keys().map(move |(file_id, syntax_id)| {
                    index_reference_fingerprint(*file_id, key, syntax_id)
                })
            });
        let strings = self
            .string_references
            .iter()
            .flat_map(|(file_id, strings)| {
                strings
                    .iter_string_references()
                    .filter(|(string, _)| *string == name.as_str())
                    .flat_map(move |(string, ranges)| {
                        ranges
                            .iter()
                            .map(move |range| string_reference_fingerprint(*file_id, string, range))
                    })
            });
        let types = self.type_references.iter().flat_map(|(file_id, by_type)| {
            by_type
                .iter()
                .filter(|(type_decl_id, _)| type_decl_id.get_name() == name.as_str())
                .flat_map(move |(type_decl_id, ranges)| {
                    ranges
                        .iter()
                        .map(move |range| type_reference_fingerprint(*file_id, type_decl_id, range))
                })
        });
        globals
            .chain(indexes)
            .chain(derived)
            .chain(strings)
            .chain(types)
            .fold(derivation::ABSENT, Fingerprint::wrapping_add)
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::References(file_id) => {
                let content = if derivation::mapping_back() {
                    Some(self.recomputed_file_fingerprint(*file_id)).filter(|sum| *sum != 0)
                } else {
                    self.file_fingerprints.get(file_id).copied()
                };
                let present = self.file_references.contains_key(file_id);
                if content.is_none() && !present {
                    derivation::ABSENT
                } else {
                    derivation::fingerprint_of(&(content, present))
                }
            }
            FactKey::DeclReferences(decl_id) => self
                .file_references
                .get(&decl_id.file_id)
                .and_then(|file| file.get_decl_references(decl_id))
                .map_or(derivation::ABSENT, |reference| {
                    let cells = reference
                        .cells
                        .iter()
                        .map(|cell| (InFiled::new(decl_id.file_id, cell.range), cell.is_write))
                        .collect::<Vec<_>>();
                    derivation::fingerprint_of(&(cells, reference.mutable))
                }),
            FactKey::ReferencesByName(name) if derivation::mapping_back() => {
                self.recomputed_name_fingerprint(name)
            }
            FactKey::ReferencesByName(name) => self
                .name_fingerprints
                .get(name)
                .copied()
                .unwrap_or(derivation::ABSENT),
            FactKey::IndexReferenceFiles(name) => {
                derivation::fingerprint_of(&self.collect_index_reference_files(name))
            }
            FactKey::DerivedReferences(file_id) if derivation::mapping_back() => {
                self.recomputed_derived_file_fingerprint(*file_id)
            }
            FactKey::DerivedReferences(file_id) => self
                .derived_file_fingerprints
                .get(file_id)
                .copied()
                .unwrap_or(derivation::ABSENT),
            FactKey::ReferenceContrib(id) => {
                derivation::fingerprint_opt(self.derived_index_reference_holders.get(id))
            }
            _ => {
                debug_assert!(false, "{key:?} is not a reference fact");
                derivation::ABSENT
            }
        }
    }

    pub(crate) fn files_with_index_references(&self, name: &SmolStr) -> Vec<FileId> {
        derivation::note_read(|| FactKey::IndexReferenceFiles(name.clone()));
        self.collect_index_reference_files(name)
    }

    fn collect_index_reference_files(&self, name: &SmolStr) -> Vec<FileId> {
        let member_key = LuaMemberKey::Name(name.clone());
        let mut files = self
            .index_reference
            .get(&member_key)
            .into_iter()
            .flatten()
            .chain(self.global_references.get(name).into_iter().flatten())
            .map(|(file_id, _)| *file_id)
            .chain(
                self.derived_index_reference
                    .get(&member_key)
                    .into_iter()
                    .flat_map(|sites| sites.keys())
                    .map(|(file_id, _)| *file_id),
            )
            .collect::<Vec<_>>();
        files.sort_unstable();
        files.dedup();
        files
    }

    /// Every file that references `name`, from both name-keyed tables. The sites
    /// themselves are not materialized.
    pub fn files_referencing_name(&self, name: &SmolStr) -> HashSet<FileId> {
        derivation::note_read(|| FactKey::ReferencesByName(name.clone()));
        let member_key = LuaMemberKey::Name(name.clone());
        self.index_reference
            .get(&member_key)
            .into_iter()
            .flatten()
            .chain(self.global_references.get(name).into_iter().flatten())
            .map(|(file_id, _)| *file_id)
            .collect()
    }

    /// Every site in `file_id` that references `name`, from both name-keyed
    /// tables.
    pub fn name_references_in_file(&self, name: &SmolStr, file_id: FileId) -> Vec<LuaSyntaxId> {
        derivation::note_read(|| FactKey::ReferencesByName(name.clone()));
        let member_key = LuaMemberKey::Name(name.clone());
        self.index_reference
            .get(&member_key)
            .and_then(|references| references.get(&file_id))
            .into_iter()
            .flatten()
            .chain(
                self.global_references
                    .get(name)
                    .and_then(|references| references.get(&file_id))
                    .into_iter()
                    .flatten(),
            )
            .copied()
            .collect()
    }

    /// Whether `syntax_id` in `file_id` references `name`, the index
    /// references derivations resolved included: [`Self::get_global_references`]
    /// and [`Self::get_index_references`] narrowed to one site, read as that
    /// file's facts alone.
    pub fn is_name_site(&self, name: &SmolStr, file_id: FileId, syntax_id: LuaSyntaxId) -> bool {
        derivation::note_file_fact_read(derivation::FileFact::References, file_id);
        derivation::note_read(|| FactKey::DerivedReferences(file_id));
        let in_file = |sites: Option<&HashMap<FileId, Vec<LuaSyntaxId>>>| {
            sites
                .and_then(|sites| sites.get(&file_id))
                .is_some_and(|ids| ids.contains(&syntax_id))
        };
        let member_key = LuaMemberKey::Name(name.clone());
        in_file(self.global_references.get(name))
            || in_file(self.index_reference.get(&member_key))
            || self
                .derived_index_reference
                .get(&member_key)
                .is_some_and(|derived| derived.contains_key(&(file_id, syntax_id)))
    }

    /// `file_id`'s derived reference fingerprints summed afresh, which is what
    /// the incremental `derived_file_fingerprints` sum holds.
    fn recomputed_derived_file_fingerprint(&self, file_id: FileId) -> Fingerprint {
        self.derived_index_reference
            .iter()
            .flat_map(|(key, sites)| {
                sites
                    .keys()
                    .filter(move |(site_file, _)| *site_file == file_id)
                    .map(move |(_, syntax_id)| index_reference_fingerprint(file_id, key, syntax_id))
            })
            .fold(derivation::ABSENT, Fingerprint::wrapping_add)
    }

    /// Files the references `batch` collected for a file that has none yet.
    pub fn file_batch(&mut self, batch: FileReferenceBatch) {
        let file_id = batch.file_id();
        derivation::note_write(|| FactKey::References(file_id));
        *self.file_references.entry(file_id).or_default() = batch.file_reference;
        for entry in batch.globals.entries {
            derivation::note_write(|| FactKey::ReferencesByName(entry.name.clone()));
            let slot = self
                .global_references
                .entry(entry.name.clone())
                .or_default()
                .entry(file_id)
                .or_default();
            merge_sites(slot, entry.sites);
            self.add_name_fingerprint(entry.name, entry.fingerprint);
        }
        for entry in batch.indexes.entries {
            let name = member_key_name(&entry.name);
            derivation::note_write(|| FactKey::ReferencesByName(name.clone()));
            let slot = self
                .index_reference
                .entry(entry.name)
                .or_default()
                .entry(file_id)
                .or_default();
            merge_sites(slot, entry.sites);
            self.add_name_fingerprint(name, entry.fingerprint);
        }
        for (string, fingerprint) in batch.string_fingerprints {
            derivation::note_write(|| FactKey::ReferencesByName(string.clone()));
            self.add_name_fingerprint(string, fingerprint);
        }
        *self
            .string_references
            .entry(file_id)
            .or_insert_with(StringReference::new) = batch.strings;
        if batch.fingerprinted {
            self.add_file_fingerprint(file_id, batch.fingerprint);
        }
        if batch.revisions > 0 {
            self.next_revision += batch.revisions - 1;
            self.bump_file_revision(file_id);
        }
    }

    pub fn add_decl_reference(
        &mut self,
        decl_id: LuaDeclId,
        file_id: FileId,
        range: TextRange,
        is_write: bool,
    ) {
        derivation::note_write(|| FactKey::References(file_id));
        let file_reference = self.file_references.entry(file_id).or_default();
        if file_reference.get_decl_id(&range).is_none() {
            file_reference.add_decl_reference(decl_id, range, is_write);
            self.add_file_fingerprint(
                file_id,
                decl_reference_fingerprint(file_id, decl_id, range, is_write),
            );
        }
        self.bump_file_revision(file_id);
    }

    pub fn add_global_reference(&mut self, name: &str, file_id: FileId, syntax_id: LuaSyntaxId) {
        let key = SmolStr::new(name);
        derivation::note_write(|| FactKey::References(file_id));
        derivation::note_write(|| FactKey::ReferencesByName(key.clone()));
        let inserted = insert_site(
            self.global_references
                .entry(key.clone())
                .or_default()
                .entry(file_id)
                .or_default(),
            syntax_id,
        );
        if inserted {
            let fingerprint = global_reference_fingerprint(file_id, &key, &syntax_id);
            self.add_file_fingerprint(file_id, fingerprint);
            self.add_name_fingerprint(key, fingerprint);
        }
        self.bump_file_revision(file_id);
    }

    pub fn add_index_reference(
        &mut self,
        key: LuaMemberKey,
        file_id: FileId,
        syntax_id: LuaSyntaxId,
    ) {
        let name = member_key_name(&key);
        let fingerprint = index_reference_fingerprint(file_id, &key, &syntax_id);
        if !derivation::writes_source()
            && let Some(holder) = derivation::current()
        {
            derivation::note_write(|| FactKey::ReferenceContrib(holder));
            self.derived_index_reference_holders
                .entry(holder)
                .or_default()
                .push((key.clone(), file_id, syntax_id));
            let count = self
                .derived_index_reference
                .entry(key)
                .or_default()
                .entry((file_id, syntax_id))
                .or_default();
            *count += 1;
            if *count == 1 {
                self.add_name_fingerprint(name.clone(), fingerprint);
                let sum = self.derived_file_fingerprints.entry(file_id).or_default();
                *sum = sum.wrapping_add(fingerprint);
                derivation::note_write(|| FactKey::ReferencesByName(name));
                derivation::note_write(|| FactKey::DerivedReferences(file_id));
            }
            return;
        }
        derivation::note_write(|| FactKey::References(file_id));
        derivation::note_write(|| FactKey::ReferencesByName(name.clone()));
        let inserted = insert_site(
            self.index_reference
                .entry(key)
                .or_default()
                .entry(file_id)
                .or_default(),
            syntax_id,
        );
        if inserted {
            self.add_file_fingerprint(file_id, fingerprint);
            self.add_name_fingerprint(name, fingerprint);
        }
        self.bump_file_revision(file_id);
    }

    pub fn add_string_reference(&mut self, file_id: FileId, string: &str, range: TextRange) {
        derivation::note_write(|| FactKey::References(file_id));
        derivation::note_write(|| FactKey::ReferencesByName(SmolStr::new(string)));
        let fingerprint = string_reference_fingerprint(file_id, string, &range);
        self.add_file_fingerprint(file_id, fingerprint);
        self.add_name_fingerprint(SmolStr::new(string), fingerprint);
        self.string_references
            .entry(file_id)
            .or_insert_with(StringReference::new)
            .add_string_reference(string, range);
    }

    pub fn add_type_reference(
        &mut self,
        file_id: FileId,
        type_decl_id: LuaTypeDeclId,
        range: TextRange,
    ) {
        let name = SmolStr::new(type_decl_id.get_name());
        derivation::note_write(|| FactKey::References(file_id));
        derivation::note_write(|| FactKey::ReferencesByName(name.clone()));
        let fingerprint = type_reference_fingerprint(file_id, &type_decl_id, &range);
        let ranges = self
            .type_references
            .entry(file_id)
            .or_default()
            .entry(type_decl_id)
            .or_default();
        let inserted = match ranges.binary_search_by_key(&range_key(&range), range_key) {
            Ok(_) => false,
            Err(at) => {
                ranges.insert(at, range);
                true
            }
        };
        if inserted {
            self.add_file_fingerprint(file_id, fingerprint);
            self.add_name_fingerprint(name, fingerprint);
        }
    }

    pub fn set_local_reassignments(
        &mut self,
        file_id: FileId,
        positions: HashMap<LuaDeclId, TextSize>,
    ) {
        derivation::note_write(|| FactKey::References(file_id));
        let added = reassignments_fingerprint(&positions);
        let removed = self
            .local_reassignments
            .insert(file_id, positions)
            .map_or(0, |previous| reassignments_fingerprint(&previous));
        self.add_file_fingerprint(file_id, added.wrapping_sub(removed));
    }

    /// Where `decl_id` is first reassigned within `file_id`, if at all.
    pub fn first_local_reassignment(
        &self,
        file_id: FileId,
        decl_id: &LuaDeclId,
    ) -> Option<TextSize> {
        derivation::note_file_fact_read(derivation::FileFact::References, file_id);
        self.local_reassignments
            .get(&file_id)?
            .get(decl_id)
            .copied()
    }

    pub fn get_local_reference(&self, file_id: &FileId) -> Option<&FileReference> {
        derivation::note_file_fact_read(derivation::FileFact::References, *file_id);
        self.file_references.get(file_id)
    }

    pub fn create_local_reference(&mut self, file_id: FileId) {
        derivation::note_write(|| FactKey::References(file_id));
        self.file_references.entry(file_id).or_default();
    }

    pub fn get_decl_references(
        &self,
        file_id: &FileId,
        decl_id: &LuaDeclId,
    ) -> Option<&DeclReference> {
        derivation::note_file_fact_read(derivation::FileFact::References, *file_id);
        self.file_references
            .get(file_id)?
            .get_decl_references(decl_id)
    }

    pub fn get_var_reference_decl(&self, file_id: &FileId, range: TextRange) -> Option<LuaDeclId> {
        derivation::note_file_fact_read(derivation::FileFact::References, *file_id);
        self.file_references.get(file_id)?.get_decl_id(&range)
    }

    /// Where `decl_id` is read and written, read as that declaration's
    /// references alone.
    pub fn decl_references(&self, decl_id: &LuaDeclId) -> Option<&DeclReference> {
        derivation::note_read(|| FactKey::DeclReferences(*decl_id));
        self.file_references
            .get(&decl_id.file_id)?
            .get_decl_references(decl_id)
    }

    pub fn get_decl_references_map(
        &self,
        file_id: &FileId,
    ) -> Option<&HashMap<LuaDeclId, DeclReference>> {
        derivation::note_file_fact_read(derivation::FileFact::References, *file_id);
        self.file_references
            .get(file_id)
            .map(|file_reference| file_reference.get_decl_references_map())
    }

    pub fn get_global_file_references(
        &self,
        name: &str,
        file_id: FileId,
    ) -> Option<Vec<LuaSyntaxId>> {
        derivation::note_read(|| FactKey::ReferencesByName(SmolStr::new(name)));
        let results = self
            .global_references
            .get(name)?
            .iter()
            .filter_map(|(source_file_id, syntax_ids)| {
                if file_id == *source_file_id {
                    Some(syntax_ids.iter())
                } else {
                    None
                }
            })
            .flatten()
            .copied()
            .collect();

        Some(results)
    }

    pub fn get_global_references(&self, name: &str) -> Option<Vec<InFiled<LuaSyntaxId>>> {
        derivation::note_read(|| FactKey::ReferencesByName(SmolStr::new(name)));
        let results = self
            .global_references
            .get(name)?
            .iter()
            .flat_map(|(file_id, syntax_ids)| {
                syntax_ids
                    .iter()
                    .map(|syntax_id| InFiled::new(*file_id, *syntax_id))
            })
            .collect();

        Some(results)
    }

    pub fn get_index_references(&self, key: &LuaMemberKey) -> Option<Vec<InFiled<LuaSyntaxId>>> {
        derivation::note_read(|| FactKey::ReferencesByName(member_key_name(key)));
        let source = self.index_reference.get(key);
        let derived = self.derived_index_reference.get(key);
        if source.is_none() && derived.is_none() {
            return None;
        }
        let mut results = source
            .into_iter()
            .flatten()
            .flat_map(|(file_id, syntax_ids)| {
                syntax_ids
                    .iter()
                    .map(|syntax_id| InFiled::new(*file_id, *syntax_id))
            })
            .collect::<Vec<_>>();
        for (file_id, syntax_id) in derived.into_iter().flat_map(|sites| sites.keys()) {
            let in_source = source
                .and_then(|by_file| by_file.get(file_id))
                .is_some_and(|sites| sites.binary_search(syntax_id).is_ok());
            if !in_source {
                results.push(InFiled::new(*file_id, *syntax_id));
            }
        }
        Some(results)
    }

    pub fn get_string_references(&self, string_value: &str) -> Vec<InFiled<TextRange>> {
        derivation::note_read(|| FactKey::ReferencesByName(SmolStr::new(string_value)));
        self.string_references
            .iter()
            .flat_map(|(file_id, string_reference)| {
                string_reference
                    .get_string_references(string_value)
                    .into_iter()
                    .map(|range| InFiled::new(*file_id, range))
            })
            .collect()
    }

    pub fn get_type_references(
        &self,
        type_decl_id: &LuaTypeDeclId,
    ) -> Option<Vec<InFiled<TextRange>>> {
        derivation::note_read(|| FactKey::ReferencesByName(SmolStr::new(type_decl_id.get_name())));
        let results = self
            .type_references
            .iter()
            .flat_map(|(file_id, type_references)| {
                type_references
                    .get(type_decl_id)
                    .into_iter()
                    .flatten()
                    .map(|range| InFiled::new(*file_id, *range))
            })
            .collect();

        Some(results)
    }

    /// The declaration ids recorded for doc type names spanning `range` in
    /// `file_id`, including names that resolved to nothing when the file was
    /// analysed (those are stored under the would-be global id).
    pub fn get_type_references_at(&self, file_id: FileId, range: TextRange) -> Vec<LuaTypeDeclId> {
        derivation::note_file_fact_read(derivation::FileFact::References, file_id);
        let Some(type_references) = self.type_references.get(&file_id) else {
            return Vec::new();
        };
        type_references
            .iter()
            .filter(|(_, ranges)| {
                ranges
                    .iter()
                    .any(|stored| stored.intersect(range).is_some())
            })
            .map(|(decl_id, _)| decl_id.clone())
            .collect()
    }

    /// Every expression range mapped to the declaration it refers to — the
    /// stored range → decl inverse map inside each [`FileReference`].
    pub fn iter_reference_ranges(
        &self,
    ) -> impl Iterator<Item = (FileId, TextRange, LuaDeclId)> + '_ {
        self.file_references
            .iter()
            .flat_map(|(file_id, reference)| {
                derivation::note_file_fact_read(derivation::FileFact::References, *file_id);
                reference
                    .iter_references_to_decl()
                    .map(move |(range, decl_id)| (*file_id, *range, *decl_id))
            })
    }

    /// Member-key reference sites: key → file → syntax ids.
    pub fn iter_index_references(
        &self,
    ) -> impl Iterator<Item = (&LuaMemberKey, &HashMap<FileId, Vec<LuaSyntaxId>>)> {
        self.index_reference.iter().inspect(|(key, _)| {
            derivation::note_read(|| FactKey::ReferencesByName(member_key_name(key)));
        })
    }

    /// Global-name reference sites: name → file → syntax ids.
    pub fn iter_global_references(
        &self,
    ) -> impl Iterator<Item = (&SmolStr, &HashMap<FileId, Vec<LuaSyntaxId>>)> {
        self.global_references.iter().inspect(|(name, _)| {
            derivation::note_read(|| FactKey::ReferencesByName((*name).clone()));
        })
    }

    /// Per-file string literal reference sites.
    pub fn iter_string_references(&self) -> impl Iterator<Item = (&FileId, &StringReference)> {
        self.string_references.iter().inspect(|(file_id, _)| {
            derivation::note_file_fact_read(derivation::FileFact::References, **file_id)
        })
    }

    /// Doc type-name reference sites: file → type decl → ranges.
    pub fn iter_type_references(
        &self,
    ) -> impl Iterator<Item = (&FileId, &HashMap<LuaTypeDeclId, Vec<TextRange>>)> {
        self.type_references.iter().inspect(|(file_id, _)| {
            derivation::note_file_fact_read(derivation::FileFact::References, **file_id)
        })
    }

    /// First reassignment position of each local: file → decl → position.
    pub fn iter_local_reassignments(
        &self,
    ) -> impl Iterator<Item = (&FileId, &HashMap<LuaDeclId, TextSize>)> {
        self.local_reassignments.iter().inspect(|(file_id, _)| {
            derivation::note_file_fact_read(derivation::FileFact::References, **file_id)
        })
    }
}

/// Inserts `site` into the source-ordered `sites`. Returns whether it is new.
fn insert_site(sites: &mut Vec<LuaSyntaxId>, site: LuaSyntaxId) -> bool {
    match sites.binary_search(&site) {
        Ok(_) => false,
        Err(at) => {
            sites.insert(at, site);
            true
        }
    }
}

/// Files a batch's distinct sites into the source-ordered `slot`.
fn merge_sites(slot: &mut Vec<LuaSyntaxId>, sites: HashSet<LuaSyntaxId>) {
    if slot.is_empty() {
        *slot = sites.into_iter().collect();
        slot.sort_unstable();
    } else {
        slot.extend(sites);
        slot.sort_unstable();
        slot.dedup();
    }
}

fn range_key(range: &TextRange) -> (TextSize, TextSize) {
    (range.start(), range.end())
}

/// The name a member-key reference is fingerprinted and read under.
fn member_key_name(key: &LuaMemberKey) -> SmolStr {
    match key {
        LuaMemberKey::Name(name) => name.clone(),
        LuaMemberKey::Integer(index) => SmolStr::new(format!("[{index}]")),
        LuaMemberKey::None | LuaMemberKey::ExprType(_) => SmolStr::new_static("[]"),
    }
}

fn decl_reference_fingerprint(
    file_id: FileId,
    decl_id: LuaDeclId,
    range: TextRange,
    is_write: bool,
) -> Fingerprint {
    derivation::fingerprint_of(&(0u8, decl_id, InFiled::new(file_id, range), is_write))
}

fn global_reference_fingerprint(
    file_id: FileId,
    name: &str,
    syntax_id: &LuaSyntaxId,
) -> Fingerprint {
    derivation::fingerprint_of(&(1u8, name, InFiled::new(file_id, *syntax_id)))
}

fn index_reference_fingerprint(
    file_id: FileId,
    key: &LuaMemberKey,
    syntax_id: &LuaSyntaxId,
) -> Fingerprint {
    derivation::fingerprint_of(&(2u8, key, InFiled::new(file_id, *syntax_id)))
}

fn string_reference_fingerprint(file_id: FileId, string: &str, range: &TextRange) -> Fingerprint {
    derivation::fingerprint_of(&(3u8, string, InFiled::new(file_id, *range)))
}

fn type_reference_fingerprint(
    file_id: FileId,
    type_decl_id: &LuaTypeDeclId,
    range: &TextRange,
) -> Fingerprint {
    derivation::fingerprint_of(&(4u8, type_decl_id, InFiled::new(file_id, *range)))
}

fn reassignments_fingerprint(positions: &HashMap<LuaDeclId, TextSize>) -> Fingerprint {
    positions
        .iter()
        .fold(0, |sum: Fingerprint, (decl_id, position)| {
            sum.wrapping_add(derivation::fingerprint_of(&(
                5u8,
                decl_id,
                InFiled::new(decl_id.file_id, *position),
            )))
        })
}

impl LuaIndex for LuaReferenceIndex {
    fn remove(&mut self, file_id: FileId) {
        derivation::note_write(|| FactKey::References(file_id));
        self.bump_file_revision(file_id);
        self.file_fingerprints.remove(&file_id);
        self.file_references.remove(&file_id);
        Self::remove_file_name_fingerprints(
            &mut self.name_fingerprints,
            file_id,
            self.string_references.remove(&file_id),
            self.type_references.remove(&file_id),
        );
        self.local_reassignments.remove(&file_id);
        let mut to_be_remove = Vec::new();
        for (key, references) in self.index_reference.iter_mut() {
            if let Some(syntax_ids) = references.remove(&file_id) {
                let name = member_key_name(key);
                for syntax_id in &syntax_ids {
                    Self::sub_name_fingerprint(
                        &mut self.name_fingerprints,
                        &name,
                        index_reference_fingerprint(file_id, key, syntax_id),
                    );
                }
            }
            if references.is_empty() {
                to_be_remove.push(key.clone());
            }
        }

        for key in to_be_remove {
            self.index_reference.remove(&key);
        }

        let mut to_be_remove = Vec::new();
        for (key, references) in self.global_references.iter_mut() {
            if let Some(syntax_ids) = references.remove(&file_id) {
                for syntax_id in &syntax_ids {
                    Self::sub_name_fingerprint(
                        &mut self.name_fingerprints,
                        key,
                        global_reference_fingerprint(file_id, key, syntax_id),
                    );
                }
            }
            if references.is_empty() {
                to_be_remove.push(key.clone());
            }
        }

        for key in to_be_remove {
            self.global_references.remove(&key);
        }
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        for &file_id in file_ids {
            derivation::note_write(|| FactKey::References(file_id));
            self.bump_file_revision(file_id);
        }
        let removed_file_ids = file_ids.iter().copied().collect::<HashSet<_>>();
        for file_id in &removed_file_ids {
            self.file_fingerprints.remove(file_id);
            Self::remove_file_name_fingerprints(
                &mut self.name_fingerprints,
                *file_id,
                self.string_references.remove(file_id),
                self.type_references.remove(file_id),
            );
        }
        self.file_references
            .retain(|file_id, _| !removed_file_ids.contains(file_id));
        self.local_reassignments
            .retain(|file_id, _| !removed_file_ids.contains(file_id));

        let name_fingerprints = &mut self.name_fingerprints;
        self.index_reference.retain(|key, references| {
            references.retain(|file_id, syntax_ids| {
                if !removed_file_ids.contains(file_id) {
                    return true;
                }
                let name = member_key_name(key);
                for syntax_id in syntax_ids.iter() {
                    Self::sub_name_fingerprint(
                        name_fingerprints,
                        &name,
                        index_reference_fingerprint(*file_id, key, syntax_id),
                    );
                }
                false
            });
            !references.is_empty()
        });
        self.global_references.retain(|name, references| {
            references.retain(|file_id, syntax_ids| {
                if !removed_file_ids.contains(file_id) {
                    return true;
                }
                for syntax_id in syntax_ids.iter() {
                    Self::sub_name_fingerprint(
                        name_fingerprints,
                        name,
                        global_reference_fingerprint(*file_id, name, syntax_id),
                    );
                }
                false
            });
            !references.is_empty()
        });
    }

    fn clear(&mut self) {
        if derivation::recording() {
            for file_id in self
                .file_fingerprints
                .keys()
                .chain(self.file_references.keys())
            {
                derivation::note_write(|| FactKey::References(*file_id));
            }
            for name in self.name_fingerprints.keys() {
                derivation::note_write(|| FactKey::ReferencesByName(name.clone()));
            }
        }
        self.file_fingerprints.clear();
        self.name_fingerprints.clear();
        self.file_revision.clear();
        self.file_references.clear();
        self.string_references.clear();
        self.type_references.clear();
        self.local_reassignments.clear();
        self.index_reference.clear();
        self.derived_index_reference.clear();
        self.derived_file_fingerprints.clear();
        self.derived_index_reference_holders.clear();
        self.global_references.clear();
    }
}

#[cfg(test)]
mod tests {
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    use super::{LuaIndex, LuaReferenceIndex};
    use crate::{FileId, LuaMemberKey, db_index::LuaTypeDeclId};

    #[test]
    fn index_reference_files_ignore_strings_types_and_site_positions() {
        use crate::LuaTypeDeclId;
        use crate::db_index::derivation::FactKey;

        let mut index = LuaReferenceIndex::new();
        let file = FileId::new(1);
        let other = FileId::new(2);
        let name = smol_str::SmolStr::new("Relay");
        let key = FactKey::IndexReferenceFiles(name.clone());
        let range = TextRange::new(TextSize::new(0), TextSize::new(1));
        let later = TextRange::new(TextSize::new(4), TextSize::new(5));
        let site = LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range);
        let empty = index.fact_fingerprint(&key);
        index.add_string_reference(file, "Relay", range);
        index.add_type_reference(file, LuaTypeDeclId::global("Relay"), range);
        assert_eq!(index.fact_fingerprint(&key), empty);
        assert!(index.files_with_index_references(&name).is_empty());
        index.add_index_reference(LuaMemberKey::Name(name.clone()), file, site);
        let present = index.fact_fingerprint(&key);
        assert_ne!(present, empty);
        index.add_global_reference(&name, file, site);
        index.add_index_reference(
            LuaMemberKey::Name(name.clone()),
            file,
            LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), later),
        );
        assert_eq!(index.fact_fingerprint(&key), present);
        assert_eq!(index.files_with_index_references(&name), vec![file]);
        index.add_global_reference(&name, other, site);
        assert_eq!(index.files_with_index_references(&name), vec![file, other]);
        index.remove(file);
        assert_eq!(index.files_with_index_references(&name), vec![other]);
        index.remove(other);
        assert_eq!(index.fact_fingerprint(&key), empty);
    }

    #[test]
    fn name_lookups_union_both_tables_and_a_write_moves_the_file_revision() {
        let first = FileId::new(1);
        let second = FileId::new(2);
        let range = TextRange::new(TextSize::new(0), TextSize::new(1));
        let other_range = TextRange::new(TextSize::new(4), TextSize::new(5));
        let mut index = LuaReferenceIndex::new();
        let global_site = LuaSyntaxId::new(LuaSyntaxKind::NameExpr.into(), range);
        let index_site = LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), other_range);
        let name: smol_str::SmolStr = "Send".into();

        index.add_global_reference(&name, first, global_site);
        index.add_index_reference(LuaMemberKey::Name(name.clone()), first, index_site);
        index.add_global_reference(&name, second, global_site);
        let first_revision = index.file_reference_revision(first);

        assert_eq!(
            index.files_referencing_name(&name),
            [first, second].into_iter().collect()
        );
        let mut sites = index.name_references_in_file(&name, first);
        sites.sort_by_key(|site| site.get_range().start());
        assert_eq!(sites, vec![global_site, index_site]);
        assert_eq!(
            index.name_references_in_file(&name, second),
            vec![global_site]
        );

        index.add_decl_reference(
            crate::db_index::LuaDeclId::new(first, TextSize::new(0)),
            first,
            range,
            false,
        );
        assert_ne!(index.file_reference_revision(first), first_revision);

        let before_removal = index.file_reference_revision(second);
        index.remove(second);
        assert!(index.files_referencing_name(&name).contains(&first));
        assert!(!index.files_referencing_name(&name).contains(&second));
        assert_ne!(index.file_reference_revision(second), before_removal);
    }

    #[test]
    fn batch_removal_keeps_references_from_surviving_files() {
        let first = FileId::new(1);
        let second = FileId::new(2);
        let surviving = FileId::new(3);
        let range = TextRange::new(TextSize::new(0), TextSize::new(1));
        let mut index = LuaReferenceIndex::new();

        index.create_local_reference(first);
        index.create_local_reference(surviving);
        index.add_string_reference(first, "removed", range);
        index.add_string_reference(surviving, "surviving", range);
        let first_type = LuaTypeDeclId::local(first, "First");
        let surviving_type = LuaTypeDeclId::local(surviving, "Surviving");
        index.add_type_reference(first, first_type.clone(), range);
        index.add_type_reference(surviving, surviving_type.clone(), range);
        let syntax_id = LuaSyntaxId::new(LuaSyntaxKind::NameExpr.into(), range);
        let member_key = LuaMemberKey::Name("field".into());
        index.add_global_reference("GLOBAL", first, syntax_id);
        index.add_global_reference("GLOBAL", surviving, syntax_id);
        index.add_index_reference(member_key.clone(), first, syntax_id);
        index.add_index_reference(member_key.clone(), surviving, syntax_id);

        index.remove_files(&[second, first, second]);

        assert!(index.get_local_reference(&first).is_none());
        assert!(index.get_local_reference(&surviving).is_some());
        assert!(index.get_string_references("removed").is_empty());
        assert_eq!(index.get_string_references("surviving").len(), 1);
        assert!(
            index
                .get_type_references(&first_type)
                .is_none_or(|references| references.is_empty())
        );
        assert_eq!(index.get_type_references(&surviving_type).unwrap().len(), 1);
        assert_eq!(index.get_global_references("GLOBAL").unwrap().len(), 1);
        assert_eq!(index.get_index_references(&member_key).unwrap().len(), 1);

        index.clear();
        assert!(
            index
                .get_type_references(&surviving_type)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn reserve_for_file_leaves_reference_contents_and_revisions_unchanged() {
        let first = FileId::new(1);
        let range = TextRange::new(TextSize::new(0), TextSize::new(1));
        let mut index = LuaReferenceIndex::new();
        index.reserve_for_file(first, 64);

        assert_eq!(index.file_reference_revision(first), 0);
        assert!(
            index
                .get_local_reference(&first)
                .is_none_or(|references| references.get_decl_references_map().is_empty())
        );

        let syntax_id = LuaSyntaxId::new(LuaSyntaxKind::NameExpr.into(), range);
        let member_key = LuaMemberKey::Name("field".into());
        index.add_index_reference(member_key.clone(), first, syntax_id);
        index.add_global_reference("GLOBAL", first, syntax_id);

        assert_eq!(index.get_index_references(&member_key).unwrap().len(), 1);
        assert_eq!(index.get_global_references("GLOBAL").unwrap().len(), 1);
    }
}

crate::db_index::cow_shards::task_fork_fields!(LuaReferenceIndex {
    file_references,
    index_reference,
    global_references,
    string_references,
    type_references,
    local_reassignments,
    file_revision,
    next_revision,
    file_fingerprints,
    name_fingerprints,
    derived_index_reference,
    derived_file_fingerprints,
    derived_index_reference_holders,
});
