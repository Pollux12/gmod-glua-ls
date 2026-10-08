mod global_id;

use crate::db_index::cow_shards::CowShards;
use std::collections::BTreeMap;
use std::sync::Arc;

pub use global_id::GlobalId;

use crate::FileId;
use crate::db_index::derivation::{self, FactKey, Fingerprint};
use smol_str::SmolStr;

use super::{LuaDeclId, LuaIndex, LuaModuleIndex, WorkspaceId};

#[derive(Debug, Clone)]
pub struct LuaGlobalIndex {
    global_decl: CowShards<GlobalId, Vec<LuaDeclId>>,
    /// Each declaring file's `Vfs::file_order_key`, which orders its
    /// declarations among the other files'.
    file_order: CowShards<FileId, Arc<str>>,
    /// Multiset fingerprint of every `(name, decl id, file order key)`, for
    /// `GlobalScan`: the order key decides where a scan lists the declaration.
    scan_fingerprint: Fingerprint,
}

fn scan_element(
    file_order: &CowShards<FileId, Arc<str>>,
    name: &str,
    decl_id: LuaDeclId,
) -> Fingerprint {
    derivation::fingerprint_of(&(name, decl_id, decl_sort_key(file_order, decl_id).0))
}

fn note_global_write(name: &str) {
    derivation::note_write(|| FactKey::Global(SmolStr::new(name)));
    derivation::note_write(|| FactKey::GlobalScan);
}

/// Canonical order for a global's declarations: source position, with the
/// file's path as the outer key. File ids follow load history (a file created
/// mid-session, or a workspace loaded in batches, numbers differently from a
/// cold load), and the first declaration in this order is the one a global
/// resolves to, so the path decides.
fn decl_sort_key(file_order: &CowShards<FileId, Arc<str>>, decl_id: LuaDeclId) -> (&str, u32, u32) {
    (
        file_order.get(&decl_id.file_id).map_or("", |key| key),
        u32::from(decl_id.position),
        decl_id.file_id.id,
    )
}

impl Default for LuaGlobalIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaGlobalIndex {
    pub fn new() -> Self {
        Self {
            global_decl: Default::default(),
            file_order: Default::default(),
            scan_fingerprint: derivation::ABSENT,
        }
    }

    /// Registers a declaration of `name`, keeping the global's declaration
    /// list in canonical source order. `file_order_key` is the declaring
    /// file's `Vfs::file_order_key`.
    pub fn add_global_decl(&mut self, name: &str, decl_id: LuaDeclId, file_order_key: Arc<str>) {
        let id = GlobalId::new(name);
        self.file_order
            .entry(decl_id.file_id)
            .or_insert(file_order_key);
        let file_order = &self.file_order;
        let decl_ids = self.global_decl.entry(id).or_default();
        if let Err(insert_at) = decl_ids.binary_search_by(|existing| {
            decl_sort_key(file_order, *existing).cmp(&decl_sort_key(file_order, decl_id))
        }) {
            decl_ids.insert(insert_at, decl_id);
            self.scan_fingerprint = self
                .scan_fingerprint
                .wrapping_add(scan_element(file_order, name, decl_id));
            note_global_write(name);
        }
    }

    /// Files each of `order_keys`' files under its new order key: its
    /// declarations take the place in each global's list and in the scan that
    /// the key gives them.
    pub(crate) fn rekey_files(&mut self, order_keys: &[(FileId, Arc<str>)]) {
        let rekeyed = order_keys
            .iter()
            .filter(|(file_id, key)| self.file_order.get(file_id).is_some_and(|held| held != key))
            .cloned()
            .collect::<Vec<_>>();
        if rekeyed.is_empty() {
            return;
        }
        let files = rekeyed
            .iter()
            .map(|(file_id, _)| *file_id)
            .collect::<rustc_hash::FxHashSet<_>>();
        let names = self
            .global_decl
            .iter()
            .filter(|(_, decl_ids)| {
                decl_ids
                    .iter()
                    .any(|decl_id| files.contains(&decl_id.file_id))
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let scan_share = |index: &Self| {
            let mut share: Fingerprint = 0;
            for id in &names {
                for decl_id in index.global_decl.get(id).into_iter().flatten() {
                    if files.contains(&decl_id.file_id) {
                        share = share.wrapping_add(scan_element(
                            &index.file_order,
                            id.get_name(),
                            *decl_id,
                        ));
                    }
                }
            }
            share
        };
        let before = scan_share(self);
        for (file_id, key) in rekeyed {
            self.file_order.insert(file_id, key);
        }
        let file_order = &self.file_order;
        for id in &names {
            if let Some(decl_ids) = self.global_decl.get_mut(id) {
                decl_ids.sort_by(|left, right| {
                    decl_sort_key(file_order, *left).cmp(&decl_sort_key(file_order, *right))
                });
            }
            note_global_write(id.get_name());
        }
        let after = scan_share(self);
        self.scan_fingerprint = self
            .scan_fingerprint
            .wrapping_sub(before)
            .wrapping_add(after);
    }

    /// Drops one declaration of `name`.
    pub(crate) fn remove_global_decl(&mut self, name: &str, decl_id: LuaDeclId) {
        let id = GlobalId::new(name);
        let Some(decl_ids) = self.global_decl.get_mut(&id) else {
            return;
        };
        let Some(at) = decl_ids.iter().position(|existing| *existing == decl_id) else {
            return;
        };
        decl_ids.remove(at);
        if decl_ids.is_empty() {
            self.global_decl.remove(&id);
        }
        self.scan_fingerprint =
            self.scan_fingerprint
                .wrapping_sub(scan_element(&self.file_order, name, decl_id));
        note_global_write(name);
    }

    pub fn get_all_global_decl_ids(&self) -> Vec<LuaDeclId> {
        self.get_global_decl_ids_matching(|_| true)
    }

    /// The declarations of the globals whose names `matches` accepts, in the
    /// order of [`Self::get_all_global_decl_ids`].
    pub fn get_global_decl_ids_matching(&self, matches: impl Fn(&str) -> bool) -> Vec<LuaDeclId> {
        derivation::note_read(|| FactKey::GlobalScan);
        // `global_decl` is a `FxHashMap`, so its iteration order is not stable
        // across index states; sort so callers see the same sequence whatever
        // order the globals were discovered in.
        let mut decls = self
            .global_decl
            .iter()
            .filter(|(name, _)| matches(name.get_name()))
            .flat_map(|(_, decl_ids)| decl_ids.iter().copied())
            .collect::<Vec<_>>();
        decls.sort_unstable_by(|a, b| {
            decl_sort_key(&self.file_order, *a).cmp(&decl_sort_key(&self.file_order, *b))
        });
        decls
    }

    pub fn get_global_decl_ids(&self, name: &str) -> Option<&Vec<LuaDeclId>> {
        // Borrowed lookup: `GlobalId` hashes by name and borrows as `str`,
        // so this finds the same bucket without interning a new id per call.
        derivation::note_read(|| FactKey::Global(SmolStr::new(name)));
        self.global_decl.get(name)
    }

    pub fn get_global_decl_ids_in_workspace(
        &self,
        name: &str,
        module_index: &LuaModuleIndex,
        current_workspace_id: WorkspaceId,
    ) -> Option<Vec<LuaDeclId>> {
        let mut priority_tiers =
            self.get_global_decl_id_priority_tiers(name, module_index, current_workspace_id)?;
        priority_tiers
            .drain(..)
            .next()
            .map(|(_, decl_ids)| decl_ids)
    }

    pub fn get_global_decl_id_priority_tiers(
        &self,
        name: &str,
        module_index: &LuaModuleIndex,
        current_workspace_id: WorkspaceId,
    ) -> Option<Vec<(u8, Vec<LuaDeclId>)>> {
        let decl_ids = self.get_global_decl_ids(name)?;
        let mut priority_tiers: BTreeMap<u8, Vec<LuaDeclId>> = BTreeMap::new();

        for decl_id in decl_ids {
            let candidate_workspace_id = module_index
                .get_workspace_id(decl_id.file_id)
                .unwrap_or(WorkspaceId::MAIN);
            let Some(priority) = module_index
                .workspace_resolution_priority(current_workspace_id, candidate_workspace_id)
            else {
                continue;
            };

            priority_tiers.entry(priority).or_default().push(*decl_id);
        }

        if priority_tiers.is_empty() {
            None
        } else {
            Some(priority_tiers.into_iter().collect())
        }
    }

    pub fn is_exist_global_decl(&self, name: &str) -> bool {
        derivation::note_read(|| FactKey::Global(SmolStr::new(name)));
        self.global_decl.contains_key(name)
    }

    pub fn is_exist_global_decl_in_workspace(
        &self,
        name: &str,
        module_index: &LuaModuleIndex,
        current_workspace_id: WorkspaceId,
    ) -> bool {
        self.get_global_decl_ids_in_workspace(name, module_index, current_workspace_id)
            .is_some_and(|decl_ids| !decl_ids.is_empty())
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::Global(name) => {
                derivation::fingerprint_opt(self.global_decl.get(name.as_str()))
            }
            FactKey::GlobalScan if derivation::mapping_back() => self
                .global_decl
                .iter()
                .flat_map(|(id, decl_ids)| {
                    decl_ids
                        .iter()
                        .map(|decl_id| scan_element(&self.file_order, id.get_name(), *decl_id))
                })
                .fold(0, Fingerprint::wrapping_add),
            FactKey::GlobalScan => self.scan_fingerprint,
            _ => {
                debug_assert!(false, "{key:?} is not a global fact");
                derivation::ABSENT
            }
        }
    }

    /// Drops the declarations whose file matches `removed`, keeping the scan
    /// fingerprint and fact notes in step. Calls `on_emptied` for each global
    /// left with no declaration.
    fn retain_decls(&mut self, removed: impl Fn(FileId) -> bool, on_emptied: impl Fn(&GlobalId)) {
        let mut scan_fingerprint = self.scan_fingerprint;
        let file_order = &self.file_order;
        self.global_decl.retain(|global_id, decl_ids| {
            let before_len = decl_ids.len();
            decl_ids.retain(|decl_id| {
                if !removed(decl_id.file_id) {
                    return true;
                }
                scan_fingerprint = scan_fingerprint.wrapping_sub(scan_element(
                    file_order,
                    global_id.get_name(),
                    *decl_id,
                ));
                false
            });
            if decl_ids.len() != before_len {
                note_global_write(global_id.get_name());
            }
            if decl_ids.is_empty() && before_len > 0 {
                on_emptied(global_id);
            }
            !decl_ids.is_empty()
        });
        self.file_order.retain(|file_id, _| !removed(*file_id));
        self.scan_fingerprint = scan_fingerprint;
    }
}

impl LuaIndex for LuaGlobalIndex {
    fn remove(&mut self, file_id: FileId) {
        self.retain_decls(
            |decl_file_id| decl_file_id == file_id,
            |global_id| {
                log::info!(
                    "global_index: global '{}' fully removed (file_id={:?})",
                    global_id.get_name(),
                    file_id,
                );
            },
        );
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        let removed_file_ids = file_ids
            .iter()
            .copied()
            .collect::<rustc_hash::FxHashSet<_>>();
        self.retain_decls(|file_id| removed_file_ids.contains(&file_id), |_| {});
    }

    fn clear(&mut self) {
        for global_id in self.global_decl.keys() {
            note_global_write(global_id.get_name());
        }
        self.global_decl.clear();
        self.file_order.clear();
        self.scan_fingerprint = derivation::ABSENT;
    }
}

#[cfg(test)]
mod order_tests {
    use rowan::TextSize;

    use super::*;
    use crate::LuaIndex;

    fn decl(file: u32, position: u32) -> LuaDeclId {
        LuaDeclId::new(FileId::new(file), TextSize::new(position))
    }

    fn path_key(decl_id: LuaDeclId) -> Arc<str> {
        Arc::from(format!("f{}.lua", decl_id.file_id.id))
    }

    /// Re-indexing one file must not reorder a global's declaration list: the
    /// list is what `resolve_global_decl_id` picks the canonical declaration
    /// from, and that identity keys flow narrowing for the whole global path.
    #[test]
    fn declaration_order_survives_reindexing_a_file() {
        let mut cold = LuaGlobalIndex::new();
        for decl_id in [decl(1, 0), decl(2, 10), decl(3, 20)] {
            cold.add_global_decl("cityrp", decl_id, path_key(decl_id));
        }

        let mut reindexed = LuaGlobalIndex::new();
        for decl_id in [decl(1, 0), decl(2, 10), decl(3, 20)] {
            reindexed.add_global_decl("cityrp", decl_id, path_key(decl_id));
        }
        reindexed.remove(FileId::new(1));
        reindexed.add_global_decl("cityrp", decl(1, 0), path_key(decl(1, 0)));

        assert_eq!(
            cold.get_global_decl_ids("cityrp"),
            reindexed.get_global_decl_ids("cityrp"),
        );
    }

    /// A file created later gets a later id wherever its path sorts; the
    /// declaration order follows the path, as a cold load would number it.
    #[test]
    fn declarations_are_ordered_by_path_then_position() {
        let mut index = LuaGlobalIndex::new();
        let key = |decl_id: LuaDeclId| {
            Arc::from(if decl_id.file_id.id == 3 {
                "a.lua"
            } else {
                "b.lua"
            })
        };
        for decl_id in [decl(3, 5), decl(1, 40), decl(1, 2)] {
            index.add_global_decl("cityrp", decl_id, key(decl_id));
        }

        assert_eq!(
            index.get_global_decl_ids("cityrp"),
            Some(&vec![decl(3, 5), decl(1, 2), decl(1, 40)]),
        );
    }

    #[test]
    fn adding_the_same_declaration_twice_does_not_duplicate_it() {
        let mut index = LuaGlobalIndex::new();
        index.add_global_decl("cityrp", decl(1, 0), path_key(decl(1, 0)));
        index.add_global_decl("cityrp", decl(1, 0), path_key(decl(1, 0)));

        assert_eq!(index.get_global_decl_ids("cityrp"), Some(&vec![decl(1, 0)]));
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rowan::TextSize;

    use std::sync::Arc;

    use crate::{
        Emmyrc, FileId, WorkspaceId,
        db_index::{LuaModuleIndex, WorkspaceKind},
    };

    use super::{LuaDeclId, LuaGlobalIndex};
    use crate::db_index::LuaIndex;

    #[test]
    fn batch_removal_preserves_declarations_from_surviving_files() {
        let removed = FileId::new(1);
        let other_removed = FileId::new(2);
        let surviving = FileId::new(3);
        let mut global_index = LuaGlobalIndex::new();
        let removed_decl = LuaDeclId::new(removed, TextSize::new(0));
        let surviving_decl = LuaDeclId::new(surviving, TextSize::new(1));
        global_index.add_global_decl(
            "SharedGlobal",
            removed_decl,
            Arc::from(format!("f{}.lua", removed_decl.file_id.id)),
        );
        global_index.add_global_decl(
            "SharedGlobal",
            surviving_decl,
            Arc::from(format!("f{}.lua", surviving_decl.file_id.id)),
        );

        global_index.remove_files(&[other_removed, removed, other_removed]);

        assert_eq!(
            global_index.get_global_decl_ids("SharedGlobal").unwrap(),
            &vec![surviving_decl]
        );
    }

    fn create_module_index() -> LuaModuleIndex {
        let mut module_index = LuaModuleIndex::new();
        module_index
            .set_module_extract_patterns(["?.lua".to_string(), "?/init.lua".to_string()].to_vec());
        module_index
    }

    #[test]
    fn test_get_global_decl_ids_in_workspace_isolates_main_roots() {
        let mut global_index = LuaGlobalIndex::new();
        let mut module_index = create_module_index();

        let workspace_a = WorkspaceId::MAIN;
        let workspace_b = WorkspaceId { id: 3 };

        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA").into(),
            workspace_a,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectB").into(),
            workspace_b,
            WorkspaceKind::Main,
        );

        let file_a = FileId { id: 1 };
        module_index.add_module_by_path(file_a, "C:/Users/username/ProjectA/shared.lua");
        let decl_a = LuaDeclId::new(file_a, TextSize::new(0));
        global_index.add_global_decl(
            "SharedGlobal",
            decl_a,
            Arc::from(format!("f{}.lua", decl_a.file_id.id)),
        );

        let file_b = FileId { id: 2 };
        module_index.add_module_by_path(file_b, "C:/Users/username/ProjectB/shared.lua");
        let decl_b = LuaDeclId::new(file_b, TextSize::new(0));
        global_index.add_global_decl(
            "SharedGlobal",
            decl_b,
            Arc::from(format!("f{}.lua", decl_b.file_id.id)),
        );

        let scoped_a = global_index
            .get_global_decl_ids_in_workspace("SharedGlobal", &module_index, workspace_a)
            .unwrap();
        assert_eq!(scoped_a.len(), 1);
        assert_eq!(scoped_a[0], decl_a);

        let scoped_b = global_index
            .get_global_decl_ids_in_workspace("SharedGlobal", &module_index, workspace_b)
            .unwrap();
        assert_eq!(scoped_b.len(), 1);
        assert_eq!(scoped_b[0], decl_b);
    }

    #[test]
    fn test_get_global_decl_ids_in_workspace_includes_library_for_each_main_workspace() {
        let mut global_index = LuaGlobalIndex::new();
        let mut module_index = create_module_index();

        let workspace_a = WorkspaceId::MAIN;
        let workspace_b = WorkspaceId { id: 3 };
        let library_workspace = WorkspaceId { id: 4 };

        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA").into(),
            workspace_a,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectB").into(),
            workspace_b,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectB/lua/lib").into(),
            library_workspace,
            WorkspaceKind::Library,
        );

        let lib_file = FileId { id: 30 };
        module_index.add_module_by_path(
            lib_file,
            "C:/Users/username/ProjectB/lua/lib/shared_lib.lua",
        );
        let lib_decl = LuaDeclId::new(lib_file, TextSize::new(0));
        global_index.add_global_decl(
            "FromLibrary",
            lib_decl,
            Arc::from(format!("f{}.lua", lib_decl.file_id.id)),
        );

        let visible_from_a = global_index
            .get_global_decl_ids_in_workspace("FromLibrary", &module_index, workspace_a)
            .unwrap();
        assert_eq!(visible_from_a.len(), 1);
        assert_eq!(visible_from_a[0], lib_decl);

        let visible_from_b = global_index
            .get_global_decl_ids_in_workspace("FromLibrary", &module_index, workspace_b)
            .unwrap();
        assert_eq!(visible_from_b.len(), 1);
        assert_eq!(visible_from_b[0], lib_decl);
    }

    #[test]
    fn test_get_global_decl_ids_in_workspace_allows_cross_main_when_isolation_disabled() {
        let mut global_index = LuaGlobalIndex::new();
        let mut module_index = create_module_index();

        let workspace_a = WorkspaceId::MAIN;
        let workspace_b = WorkspaceId { id: 3 };

        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA").into(),
            workspace_a,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectB").into(),
            workspace_b,
            WorkspaceKind::Main,
        );

        let mut emmyrc = Emmyrc::default();
        emmyrc.workspace.enable_isolation = false;
        module_index.update_config(Arc::new(emmyrc));

        let file_a = FileId { id: 40 };
        module_index.add_module_by_path(file_a, "C:/Users/username/ProjectA/shared.lua");
        let decl_a = LuaDeclId::new(file_a, TextSize::new(0));
        global_index.add_global_decl(
            "SharedGlobal",
            decl_a,
            Arc::from(format!("f{}.lua", decl_a.file_id.id)),
        );

        let file_b = FileId { id: 41 };
        module_index.add_module_by_path(file_b, "C:/Users/username/ProjectB/shared.lua");
        let decl_b = LuaDeclId::new(file_b, TextSize::new(0));
        global_index.add_global_decl(
            "SharedGlobal",
            decl_b,
            Arc::from(format!("f{}.lua", decl_b.file_id.id)),
        );

        let scoped_a = global_index
            .get_global_decl_ids_in_workspace("SharedGlobal", &module_index, workspace_a)
            .unwrap();
        assert_eq!(scoped_a.len(), 2);
        assert!(scoped_a.contains(&decl_a));
        assert!(scoped_a.contains(&decl_b));
    }
}

crate::db_index::cow_shards::task_fork_fields!(LuaGlobalIndex {
    global_decl,
    file_order,
    scan_fingerprint => crate::db_index::cow_shards::merge_sum,
});
