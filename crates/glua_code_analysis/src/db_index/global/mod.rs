mod global_id;

use std::collections::{BTreeMap, HashMap};

pub use global_id::GlobalId;

use crate::FileId;

use super::{LuaDeclId, LuaIndex, LuaModuleIndex, WorkspaceId};

#[derive(Debug)]
pub struct LuaGlobalIndex {
    global_decl: HashMap<GlobalId, Vec<LuaDeclId>>,
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

    pub fn add_global_decl(&mut self, name: &str, decl_id: LuaDeclId) {
        let id = GlobalId::new(name);
        self.global_decl.entry(id).or_default().push(decl_id);
    }

    pub fn get_all_global_decl_ids(&self) -> Vec<LuaDeclId> {
        let mut decls = Vec::new();
        for v in self.global_decl.values() {
            decls.extend(v);
        }

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

    fn clear(&mut self) {
        self.global_decl.clear();
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
