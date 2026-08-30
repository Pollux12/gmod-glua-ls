mod global_id;

use std::collections::{BTreeMap, HashMap};

pub use global_id::GlobalId;

use crate::FileId;

use super::{LuaDeclId, LuaIndex, LuaModuleIndex, WorkspaceId};

#[derive(Debug)]
pub struct LuaGlobalIndex {
    global_decl: HashMap<GlobalId, Vec<LuaDeclId>>,
}

/// Canonical order for a global's declarations: source position, with the file
/// as the outer key. `FileId`s are handed out in workspace-collection order and
/// never reused within a session, so this is stable for as long as the index is.
fn decl_sort_key(decl_id: LuaDeclId) -> (u32, u32) {
    (decl_id.file_id.id, u32::from(decl_id.position))
}

impl Default for LuaGlobalIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaGlobalIndex {
    pub fn new() -> Self {
        Self {
            global_decl: HashMap::new(),
        }
    }

    /// Registers a declaration of `name`, keeping the global's declaration
    /// list in canonical source order.
    pub fn add_global_decl(&mut self, name: &str, decl_id: LuaDeclId) {
        let id = GlobalId::new(name);
        let decl_ids = self.global_decl.entry(id).or_default();
        if let Err(insert_at) = decl_ids
            .binary_search_by_key(&decl_sort_key(decl_id), |existing| decl_sort_key(*existing))
        {
            decl_ids.insert(insert_at, decl_id);
        }
    }

    pub fn get_all_global_decl_ids(&self) -> Vec<LuaDeclId> {
        // `global_decl` is a `HashMap`, so its iteration order is not stable
        // across index states; sort so callers see the same sequence whatever
        // order the globals were discovered in.
        let mut decls = self
            .global_decl
            .values()
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        decls.sort_unstable_by_key(|decl_id| decl_sort_key(*decl_id));
        decls
    }

    pub fn get_global_decl_ids(&self, name: &str) -> Option<&Vec<LuaDeclId>> {
        let id = GlobalId::new(name);
        self.global_decl.get(&id)
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
        let id = GlobalId::new(name);
        self.global_decl.contains_key(&id)
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
}

impl LuaIndex for LuaGlobalIndex {
    fn remove(&mut self, file_id: FileId) {
        self.global_decl.retain(|global_id, v| {
            let before_len = v.len();
            v.retain(|decl_id| decl_id.file_id != file_id);
            // Log when a global is completely removed (last declaration gone)
            if v.is_empty() && before_len > 0 {
                log::info!(
                    "global_index: global '{}' fully removed (file_id={:?})",
                    global_id.get_name(),
                    file_id,
                );
            }
            !v.is_empty()
        });
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        let removed_file_ids = file_ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        self.global_decl.retain(|_, decl_ids| {
            decl_ids.retain(|decl_id| !removed_file_ids.contains(&decl_id.file_id));
            !decl_ids.is_empty()
        });
    }

    fn clear(&mut self) {
        self.global_decl.clear();
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

    /// Re-indexing one file must not reorder a global's declaration list: the
    /// list is what `resolve_global_decl_id` picks the canonical declaration
    /// from, and that identity keys flow narrowing for the whole global path.
    #[test]
    fn declaration_order_survives_reindexing_a_file() {
        let mut cold = LuaGlobalIndex::new();
        for decl_id in [decl(1, 0), decl(2, 10), decl(3, 20)] {
            cold.add_global_decl("cityrp", decl_id);
        }

        let mut reindexed = LuaGlobalIndex::new();
        for decl_id in [decl(1, 0), decl(2, 10), decl(3, 20)] {
            reindexed.add_global_decl("cityrp", decl_id);
        }
        reindexed.remove(FileId::new(1));
        reindexed.add_global_decl("cityrp", decl(1, 0));

        assert_eq!(
            cold.get_global_decl_ids("cityrp"),
            reindexed.get_global_decl_ids("cityrp"),
        );
    }

    #[test]
    fn declarations_are_ordered_by_file_then_position() {
        let mut index = LuaGlobalIndex::new();
        for decl_id in [decl(3, 5), decl(1, 40), decl(1, 2)] {
            index.add_global_decl("cityrp", decl_id);
        }

        assert_eq!(
            index.get_global_decl_ids("cityrp"),
            Some(&vec![decl(1, 2), decl(1, 40), decl(3, 5)]),
        );
    }

    #[test]
    fn adding_the_same_declaration_twice_does_not_duplicate_it() {
        let mut index = LuaGlobalIndex::new();
        index.add_global_decl("cityrp", decl(1, 0));
        index.add_global_decl("cityrp", decl(1, 0));

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
        global_index.add_global_decl("SharedGlobal", removed_decl);
        global_index.add_global_decl("SharedGlobal", surviving_decl);

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
        global_index.add_global_decl("SharedGlobal", decl_a);

        let file_b = FileId { id: 2 };
        module_index.add_module_by_path(file_b, "C:/Users/username/ProjectB/shared.lua");
        let decl_b = LuaDeclId::new(file_b, TextSize::new(0));
        global_index.add_global_decl("SharedGlobal", decl_b);

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
        global_index.add_global_decl("FromLibrary", lib_decl);

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
        global_index.add_global_decl("SharedGlobal", decl_a);

        let file_b = FileId { id: 41 };
        module_index.add_module_by_path(file_b, "C:/Users/username/ProjectB/shared.lua");
        let decl_b = LuaDeclId::new(file_b, TextSize::new(0));
        global_index.add_global_decl("SharedGlobal", decl_b);

        let scoped_a = global_index
            .get_global_decl_ids_in_workspace("SharedGlobal", &module_index, workspace_a)
            .unwrap();
        assert_eq!(scoped_a.len(), 2);
        assert!(scoped_a.contains(&decl_a));
        assert!(scoped_a.contains(&decl_b));
    }
}
