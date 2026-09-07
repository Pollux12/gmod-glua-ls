mod file_reference;
mod string_reference;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

pub use file_reference::{DeclReference, DeclReferenceCell, FileReference};
use glua_parser::LuaSyntaxId;
use rowan::{TextRange, TextSize};
use smol_str::SmolStr;
use string_reference::StringReference;

use super::{LuaDeclId, LuaMemberKey, LuaTypeDeclId, traits::LuaIndex};
use crate::{FileId, InFiled};

#[derive(Debug)]
pub struct LuaReferenceIndex {
    file_references: HashMap<FileId, FileReference>,
    index_reference: HashMap<LuaMemberKey, HashMap<FileId, HashSet<LuaSyntaxId>>>,
    global_references: HashMap<SmolStr, HashMap<FileId, HashSet<LuaSyntaxId>>>,
    string_references: HashMap<FileId, StringReference>,
    type_references: HashMap<FileId, HashMap<LuaTypeDeclId, HashSet<TextRange>>>,
    /// Where each of a file's locals is first reassigned, computed once when the
    /// file is indexed.
    local_reassignments: HashMap<FileId, HashMap<LuaDeclId, TextSize>>,
    /// Bumped whenever a file's references change, so a memo over facts derived
    /// from one file's references can tell a hit from a stale entry. Values come
    /// from a counter that never restarts, so a revision is never reused after a
    /// removal.
    file_revision: HashMap<FileId, u64>,
    next_revision: u64,
}

impl Default for LuaReferenceIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaReferenceIndex {
    pub fn new() -> Self {
        Self {
            file_references: HashMap::default(),
            index_reference: HashMap::default(),
            global_references: HashMap::default(),
            string_references: HashMap::default(),
            type_references: HashMap::default(),
            local_reassignments: HashMap::default(),
            file_revision: HashMap::default(),
            next_revision: 1,
        }
    }

    /// Revision of `file_id`'s references. Zero until the file records one.
    pub fn file_reference_revision(&self, file_id: FileId) -> u64 {
        self.file_revision.get(&file_id).copied().unwrap_or(0)
    }

    /// Reserves capacity for one file's worth of reference writes.
    ///
    /// `entries` is a cheap upper-bound hint (see
    /// `crate::compilation::analyzer::common::estimate_file_index_entries`),
    /// not a count the index trusts: maps keep their exact contents, so the
    /// hint only changes rehashing, never facts. Distinct member keys are a
    /// fraction of reference writes, hence the scaled outer-map reservations.
    /// `file_revision` is intentionally untouched: a revision is minted on the
    /// first real write, and reserving must not consume one.
    pub fn reserve_for_file(&mut self, file_id: FileId, entries: usize) {
        self.file_references
            .entry(file_id)
            .or_default()
            .reserve(entries);
        self.index_reference.reserve(entries / 4 + 1);
        self.global_references.reserve(entries / 4 + 1);
        self.string_references
            .entry(file_id)
            .or_insert_with(StringReference::new)
            .reserve(entries / 8 + 1);
        self.type_references
            .entry(file_id)
            .or_default()
            .reserve(entries / 8 + 1);
    }

    fn bump_file_revision(&mut self, file_id: FileId) {
        let revision = self.next_revision;
        self.next_revision += 1;
        self.file_revision.insert(file_id, revision);
    }

    /// Every file that references `name`, from both name-keyed tables. The sites
    /// themselves are not materialized.
    pub fn files_referencing_name(&self, name: &SmolStr) -> HashSet<FileId> {
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

    pub fn add_decl_reference(
        &mut self,
        decl_id: LuaDeclId,
        file_id: FileId,
        range: TextRange,
        is_write: bool,
    ) {
        self.file_references
            .entry(file_id)
            .or_default()
            .add_decl_reference(decl_id, range, is_write);
        self.bump_file_revision(file_id);
    }

    pub fn add_global_reference(&mut self, name: &str, file_id: FileId, syntax_id: LuaSyntaxId) {
        let key = SmolStr::new(name);
        self.global_references
            .entry(key)
            .or_default()
            .entry(file_id)
            .or_default()
            .insert(syntax_id);
        self.bump_file_revision(file_id);
    }

    pub fn add_index_reference(
        &mut self,
        key: LuaMemberKey,
        file_id: FileId,
        syntax_id: LuaSyntaxId,
    ) {
        self.index_reference
            .entry(key)
            .or_default()
            .entry(file_id)
            .or_default()
            .insert(syntax_id);
        self.bump_file_revision(file_id);
    }

    pub fn add_string_reference(&mut self, file_id: FileId, string: &str, range: TextRange) {
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
        self.type_references
            .entry(file_id)
            .or_default()
            .entry(type_decl_id)
            .or_default()
            .insert(range);
    }

    pub fn set_local_reassignments(
        &mut self,
        file_id: FileId,
        positions: HashMap<LuaDeclId, TextSize>,
    ) {
        self.local_reassignments.insert(file_id, positions);
    }

    /// Where `decl_id` is first reassigned within `file_id`, if at all.
    pub fn first_local_reassignment(
        &self,
        file_id: FileId,
        decl_id: &LuaDeclId,
    ) -> Option<TextSize> {
        self.local_reassignments
            .get(&file_id)?
            .get(decl_id)
            .copied()
    }

    pub fn get_local_reference(&self, file_id: &FileId) -> Option<&FileReference> {
        self.file_references.get(file_id)
    }

    pub fn create_local_reference(&mut self, file_id: FileId) {
        self.file_references.entry(file_id).or_default();
    }

    pub fn get_decl_references(
        &self,
        file_id: &FileId,
        decl_id: &LuaDeclId,
    ) -> Option<&DeclReference> {
        self.file_references
            .get(file_id)?
            .get_decl_references(decl_id)
    }

    pub fn get_var_reference_decl(&self, file_id: &FileId, range: TextRange) -> Option<LuaDeclId> {
        self.file_references.get(file_id)?.get_decl_id(&range)
    }

    pub fn get_decl_references_map(
        &self,
        file_id: &FileId,
    ) -> Option<&HashMap<LuaDeclId, DeclReference>> {
        self.file_references
            .get(file_id)
            .map(|file_reference| file_reference.get_decl_references_map())
    }

    pub fn get_global_file_references(
        &self,
        name: &str,
        file_id: FileId,
    ) -> Option<Vec<LuaSyntaxId>> {
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
        let results = self
            .index_reference
            .get(key)?
            .iter()
            .flat_map(|(file_id, syntax_ids)| {
                syntax_ids
                    .iter()
                    .map(|syntax_id| InFiled::new(*file_id, *syntax_id))
            })
            .collect();

        Some(results)
    }

    pub fn get_string_references(&self, string_value: &str) -> Vec<InFiled<TextRange>> {
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
}

impl LuaIndex for LuaReferenceIndex {
    fn remove(&mut self, file_id: FileId) {
        self.bump_file_revision(file_id);
        self.file_references.remove(&file_id);
        self.string_references.remove(&file_id);
        self.type_references.remove(&file_id);
        self.local_reassignments.remove(&file_id);
        let mut to_be_remove = Vec::new();
        for (key, references) in self.index_reference.iter_mut() {
            references.remove(&file_id);
            if references.is_empty() {
                to_be_remove.push(key.clone());
            }
        }

        for key in to_be_remove {
            self.index_reference.remove(&key);
        }

        let mut to_be_remove = Vec::new();
        for (key, references) in self.global_references.iter_mut() {
            references.remove(&file_id);
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
            self.bump_file_revision(file_id);
        }
        let removed_file_ids = file_ids.iter().copied().collect::<HashSet<_>>();
        self.file_references
            .retain(|file_id, _| !removed_file_ids.contains(file_id));
        self.string_references
            .retain(|file_id, _| !removed_file_ids.contains(file_id));
        self.type_references
            .retain(|file_id, _| !removed_file_ids.contains(file_id));
        self.local_reassignments
            .retain(|file_id, _| !removed_file_ids.contains(file_id));

        self.index_reference.retain(|_, references| {
            references.retain(|file_id, _| !removed_file_ids.contains(file_id));
            !references.is_empty()
        });
        self.global_references.retain(|_, references| {
            references.retain(|file_id, _| !removed_file_ids.contains(file_id));
            !references.is_empty()
        });
    }

    fn clear(&mut self) {
        self.file_revision.clear();
        self.file_references.clear();
        self.string_references.clear();
        self.type_references.clear();
        self.local_reassignments.clear();
        self.index_reference.clear();
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
