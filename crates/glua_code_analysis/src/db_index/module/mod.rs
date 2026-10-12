mod legacy_module_env;
mod module_info;
mod module_node;
mod test;
mod workspace;

use crate::db_index::cow_shards::{CowCell, CowShards};
use glua_parser::LuaVersionCondition;
pub use legacy_module_env::LegacyModuleEnv;
use log::{error, info};
pub use module_info::ModuleInfo;
pub use module_node::{ModuleNode, ModuleNodeId};
use regex::Regex;
use rowan::{TextRange, TextSize};
use rustc_hash::{FxHashMap, FxHashSet};
use smol_str::SmolStr;
use std::collections::HashMap;
pub(crate) use workspace::WorkspaceResolutionKey;
pub use workspace::{Workspace, WorkspaceId, WorkspaceKind};

use super::traits::LuaIndex;
use crate::db_index::derivation::{self, FactKey, Fingerprint};
use crate::{Emmyrc, FileId, LuaSemanticDeclId, LuaType};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Clone)]
pub struct LuaModuleIndex {
    module_patterns: CowCell<Vec<Regex>>,
    module_root_id: ModuleNodeId,
    module_nodes: CowCell<FxHashMap<ModuleNodeId, ModuleNode>>,
    file_module_map: CowShards<FileId, ModuleInfo>,
    /// File -> the other files whose module export names a position in it.
    export_refs: CowShards<FileId, FxHashSet<FileId>>,
    /// The `(file, module name, workspace)` of every module in `file_module_map`.
    module_files: derivation::SetFingerprint,
    file_module_paths: CowCell<FxHashMap<FileId, String>>,
    module_name_to_file_ids: CowCell<FxHashMap<String, Vec<FileId>>>,
    legacy_module_envs: CowCell<FxHashMap<FileId, Vec<LegacyModuleEnv>>>,
    workspaces: CowCell<Vec<Workspace>>,
    workspace_kind_map: CowCell<FxHashMap<WorkspaceId, WorkspaceKind>>,
    id_counter: u32,
    fuzzy_search: bool,
    module_replace_vec: CowCell<Vec<(Regex, String)>>,
    workspace_isolation_enabled: bool,
}

fn note_module_fact_write(file_id: FileId) {
    derivation::note_write(|| FactKey::Module(file_id));
    derivation::note_write(|| FactKey::ModuleMeta(file_id));
}

/// The first segment of a dotted module path: the namespace key.
fn namespace_root(module_path: &str) -> SmolStr {
    SmolStr::new(module_path.split('.').next().unwrap_or(module_path))
}

/// The last segment of a module path as lookups split it: the name key.
fn lookup_name(module_path: &str) -> SmolStr {
    SmolStr::new(
        module_path
            .rsplit(['.', '/', '\\'])
            .next()
            .unwrap_or(module_path),
    )
}

/// Deterministic precedence of one declaration site for a shared `---`
/// payload (alias origin, generic params, ...).
///
/// The ordering reuses the global module-resolution semantics: workspace kind
/// (Main before Library before Std before Remote), then the workspace's
/// registration order so earlier configured libraries win, then the
/// normalized VFS path, then the declaration range inside the file. File id
/// is only a totality tiebreak for two files sharing a path; arrival order
/// never participates, so the winner is a function of the settled
/// declaration set rather than ingestion order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LuaDeclSourceKey {
    /// `workspace_kind_priority`: Main=0, Library=1, Std=2, Remote=3.
    pub workspace_kind: u8,
    /// Position of the file's workspace in registration order.
    pub workspace_order: usize,
    /// Tiebreak inside `workspace_order_priority` (`WorkspaceId::id`).
    pub workspace_id: u32,
    /// `Vfs::file_order_key` — normalized path, empty when unknown.
    pub file_order_key: Arc<str>,
    /// Totality tiebreak for files sharing an order key (`FileId::id`).
    pub file_id: u32,
    /// The declaration site's range inside its file.
    pub range: (TextSize, TextSize),
}

impl Default for LuaModuleIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// The files other than its own whose positions `info`'s export names.
fn export_files(info: &ModuleInfo) -> FxHashSet<FileId> {
    let mut files = info
        .export_type
        .as_ref()
        .map(crate::db_index::r#type::type_referenced_files)
        .unwrap_or_default();
    files.extend(
        info.semantic_id
            .as_ref()
            .and_then(LuaSemanticDeclId::get_file_id),
    );
    files.remove(&info.file_id);
    files
}

/// One module's contribution to [`FactKey::ModuleFiles`].
fn module_files_entry(info: &ModuleInfo) -> (FileId, &str, WorkspaceId) {
    (
        info.file_id,
        info.full_module_name.as_str(),
        info.workspace_id,
    )
}

impl LuaModuleIndex {
    pub fn new() -> Self {
        let mut index = Self {
            module_patterns: Default::default(),
            module_root_id: ModuleNodeId { id: 0 },
            module_nodes: Default::default(),
            file_module_map: Default::default(),
            export_refs: Default::default(),
            module_files: derivation::SetFingerprint::default(),
            file_module_paths: Default::default(),
            module_name_to_file_ids: Default::default(),
            legacy_module_envs: Default::default(),
            workspaces: Default::default(),
            workspace_kind_map: Default::default(),
            id_counter: 1,
            fuzzy_search: false,
            module_replace_vec: Default::default(),
            workspace_isolation_enabled: true,
        };

        let root_node = ModuleNode::default();
        index.module_nodes.insert(index.module_root_id, root_node);

        index
    }

    // patterns like "?.lua" and "?/init.lua"
    pub fn set_module_extract_patterns(&mut self, patterns: Vec<String>) {
        let mut patterns = patterns;
        patterns.sort_by_key(|b| std::cmp::Reverse(b.len()));
        patterns.dedup();
        self.module_patterns.clear();
        for item in patterns {
            let regex_str = format!(
                "^{}$",
                regex::escape(&item.replace('\\', "/")).replace("\\?", "(.*)")
            );
            match Regex::new(&regex_str) {
                Ok(re) => self.module_patterns.push(re),
                Err(e) => {
                    error!("Invalid module pattern: {}, error: {}", item, e);
                    return;
                }
            };
        }

        info!("update module pattern: {:?}", self.module_patterns);
    }

    pub fn set_module_replace_patterns(&mut self, patterns: HashMap<String, String>) {
        self.module_replace_vec.clear();
        for (key, value) in patterns {
            let key_pattern = match Regex::new(&key) {
                Ok(re) => re,
                Err(e) => {
                    error!("Invalid module replace pattern: {}, error: {}", key, e);
                    continue;
                }
            };

            self.module_replace_vec.push((key_pattern, value));
        }

        info!(
            "update module replace pattern: {:?}",
            self.module_replace_vec
        );
    }

    pub fn add_module_by_path(&mut self, file_id: FileId, path: &str) -> Option<WorkspaceId> {
        if self.file_module_map.contains_key(&file_id) {
            self.remove(file_id);
        }

        let normalized_path = path.replace('\\', "/");
        let (module_path, workspace_id) = self.module_path_at(&normalized_path)?;

        self.add_module_by_module_path(file_id, module_path, workspace_id);
        self.file_module_paths.insert(file_id, normalized_path);
        note_module_fact_write(file_id);
        Some(workspace_id)
    }

    /// The dotted module path, and workspace, a file at `path` registers as.
    pub fn module_path_at(&self, path: &str) -> Option<(String, WorkspaceId)> {
        let (module_path, workspace_id) = self.extract_module_path(path)?;
        let module_path = module_path.replace(['\\', '/'], ".");
        let module_path = if self.module_replace_vec.is_empty() {
            module_path
        } else {
            self.replace_module_path(&module_path)
        };
        Some((module_path, workspace_id))
    }

    pub fn add_module_by_module_path(
        &mut self,
        file_id: FileId,
        module_path: String,
        workspace_id: WorkspaceId,
    ) -> Option<()> {
        if self.file_module_map.contains_key(&file_id) {
            self.remove(file_id);
        }

        let module_parts: Vec<&str> = module_path.split('.').collect();
        if module_parts.is_empty() {
            return None;
        }

        let mut parent_node_id = self.module_root_id;
        for part in &module_parts {
            // I had to struggle with Rust's ownership rules, making the code look like this.
            let child_id = {
                let parent_node = self.module_nodes.get_mut(&parent_node_id)?;
                let node_id = parent_node.children.get(*part);
                match node_id {
                    Some(id) => *id,
                    None => {
                        let new_id = ModuleNodeId {
                            id: self.id_counter,
                        };
                        parent_node.children.insert(part.to_string(), new_id);
                        new_id
                    }
                }
            };
            if let std::collections::hash_map::Entry::Vacant(e) = self.module_nodes.entry(child_id)
            {
                let new_node = ModuleNode {
                    children: HashMap::default(),
                    file_ids: Vec::new(),
                    parent: Some(parent_node_id),
                };

                e.insert(new_node);
                self.id_counter += 1;
            }

            parent_node_id = child_id;
        }

        let node = self.module_nodes.get_mut(&parent_node_id)?;

        node.file_ids.push(file_id);
        let module_name = module_parts.last()?.to_string();
        let module_info = ModuleInfo {
            file_id,
            full_module_name: module_parts.join("."),
            name: module_name.clone(),
            module_id: parent_node_id,
            visible: true,
            export_type: None,
            version_conds: None,
            workspace_id,
            semantic_id: None,
            is_meta: false,
        };

        self.module_files.insert(&module_files_entry(&module_info));
        self.file_module_map.insert(file_id, module_info);
        derivation::note_write(|| FactKey::ModuleFiles);
        self.module_name_to_file_ids
            .entry(module_name)
            .or_default()
            .push(file_id);
        self.note_lookup_keys(file_id);

        Some(())
    }

    /// The path `file_id`'s module was registered from, `/`-separated.
    pub fn registered_path(&self, file_id: FileId) -> Option<&str> {
        self.file_module_paths.get(&file_id).map(String::as_str)
    }

    /// Whether `file_id` at `path` belongs where it does now: a module in the
    /// same workspace, or no module. Its module path may differ.
    pub(crate) fn belongs_alike_at(&self, file_id: FileId, path: &str) -> bool {
        let workspace_id = self
            .extract_module_path(&path.replace('\\', "/"))
            .map(|(_, workspace_id)| workspace_id);
        workspace_id
            == self
                .file_module_map
                .get(&file_id)
                .map(|info| info.workspace_id)
    }

    /// Re-files `file_id`'s module, registered from its path, under `path`,
    /// keeping what its source and module return declared of it. A module a
    /// `---@module` annotation names stays as it is.
    pub(crate) fn repath_module(&mut self, file_id: FileId, path: &str) {
        if !self.file_module_paths.contains_key(&file_id) {
            return;
        }
        let Some(declared) = self.file_module_map.get(&file_id).cloned() else {
            return;
        };
        let legacy_envs = self.legacy_module_envs.get(&file_id).cloned();
        self.add_module_by_path(file_id, path);
        if let Some(info) = self.file_module_map.get_mut(&file_id) {
            info.visible = declared.visible;
            info.version_conds = declared.version_conds;
            info.is_meta = declared.is_meta;
            info.export_type = declared.export_type;
            info.semantic_id = declared.semantic_id;
        }
        self.relink_export(file_id, FxHashSet::default());
        if let Some(envs) = legacy_envs {
            self.legacy_module_envs.insert(file_id, envs);
        }
        self.note_lookup_keys(file_id);
    }

    pub fn get_module(&self, file_id: FileId) -> Option<&ModuleInfo> {
        derivation::note_read(|| FactKey::Module(file_id));
        let module = self.file_module_map.get(&file_id)?;
        Some(module)
    }

    /// Records a module-export change. Callers note only when the write
    /// added a novel fact.
    pub(crate) fn note_module_write(&mut self, file_id: FileId) {
        note_module_fact_write(file_id);
    }

    /// Clears the export a file's module return derived, back to what the
    /// source layer registered: none.
    pub(crate) fn reset_export(&mut self, file_id: FileId) {
        if let Some(info) = self.file_module_map.get_mut(&file_id) {
            let linked = export_files(info);
            info.export_type = None;
            info.semantic_id = None;
            note_module_fact_write(file_id);
            self.relink_export(file_id, linked);
        }
    }

    /// Sets `file_id`'s module export. Returns whether it changed, or `None`
    /// when the file has no module.
    pub fn set_export(
        &mut self,
        file_id: FileId,
        export_type: LuaType,
        semantic_id: Option<LuaSemanticDeclId>,
    ) -> Option<bool> {
        derivation::note_read(|| FactKey::Module(file_id));
        self.note_lookup_keys(file_id);
        let info = self.file_module_map.get_mut(&file_id)?;
        if info.export_type.as_ref() == Some(&export_type) && info.semantic_id == semantic_id {
            return Some(false);
        }
        let linked = export_files(info);
        info.export_type = Some(export_type);
        info.semantic_id = semantic_id;
        self.relink_export(file_id, linked);
        Some(true)
    }

    /// Re-files `file_id` under the files its export names now, given those
    /// `linked` it named before.
    fn relink_export(&mut self, file_id: FileId, linked: FxHashSet<FileId>) {
        let named = self
            .file_module_map
            .get(&file_id)
            .map(export_files)
            .unwrap_or_default();
        for named_file in linked.difference(&named) {
            if let Some(files) = self.export_refs.get_mut(named_file) {
                files.remove(&file_id);
                if files.is_empty() {
                    self.export_refs.remove(named_file);
                }
            }
        }
        for named_file in named.difference(&linked) {
            self.export_refs
                .entry(*named_file)
                .or_default()
                .insert(file_id);
        }
    }

    /// Rewrites the exports of other files' modules that name positions of
    /// `remap.file_id`. Returns the modules whose export named a position the
    /// edit destroyed: they are computed again instead.
    pub fn remap_file_exports(&mut self, remap: &crate::FileRemap) -> Vec<crate::LostFact> {
        let mut lost = Vec::new();
        let mut file_ids = self
            .export_refs
            .get(&remap.file_id)
            .map(|files| files.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        file_ids.sort_unstable();
        for file_id in &file_ids {
            let Some(info) = self.file_module_map.get_mut(file_id) else {
                continue;
            };
            let export = info
                .export_type
                .as_ref()
                .map(|typ| crate::remap_identities_in_type(typ, remap));
            let semantic_id = info
                .semantic_id
                .as_ref()
                .map(|id| remap.semantic_decl_id(id));
            if export.as_ref().is_some_and(|export| export.lost)
                || semantic_id.as_ref().is_some_and(crate::Remap::is_lost)
            {
                lost.push(crate::LostFact::new(*file_id, FactKey::Module(*file_id)));
                continue;
            }
            let mut moved = false;
            if let Some(typ) = export.and_then(|export| export.typ) {
                info.export_type = Some(typ);
                moved = true;
            }
            if let Some(crate::Remap::Moved(id)) = semantic_id
                && info.semantic_id.as_ref() != Some(&id)
            {
                info.semantic_id = Some(id);
                moved = true;
            }
            if moved {
                note_module_fact_write(*file_id);
            }
        }
        lost
    }

    /// What module lookups (`find_module*`, module nodes, workspace file
    /// lists, legacy namespaces) observe of one file; `ABSENT` when the file
    /// contributes nothing.
    /// Notes a change to what lookups by `file_id`'s module name and legacy
    /// namespaces observe.
    fn note_lookup_keys(&self, file_id: FileId) {
        if !derivation::recording() {
            return;
        }
        note_module_fact_write(file_id);
        if let Some(info) = self.file_module_map.get(&file_id) {
            derivation::note_write(|| FactKey::ModuleName(SmolStr::new(&info.name)));
            derivation::note_write(|| FactKey::ModulePath(SmolStr::new(&info.full_module_name)));
        }
        for env in self.legacy_module_envs.get(&file_id).into_iter().flatten() {
            derivation::note_write(|| FactKey::ModuleNamespace(namespace_root(&env.module_path)));
        }
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::Module(file_id) => {
                let module_info = self.file_module_map.get(file_id);
                let legacy_envs = self.legacy_module_envs.get(file_id);
                if module_info.is_none() && legacy_envs.is_none() {
                    return derivation::ABSENT;
                }
                derivation::fingerprint_of(&(
                    module_info.map(|info| {
                        (
                            info.full_module_name.as_str(),
                            info.name.as_str(),
                            info.visible,
                            &info.export_type,
                            &info.version_conds,
                            info.workspace_id,
                            &info.semantic_id,
                            info.is_meta,
                        )
                    }),
                    self.file_module_paths.get(file_id),
                    legacy_envs,
                ))
            }
            FactKey::ModuleMeta(file_id) => {
                let module_info = self.file_module_map.get(file_id);
                let legacy_envs = self.legacy_module_envs.get(file_id);
                if module_info.is_none() && legacy_envs.is_none() {
                    return derivation::ABSENT;
                }
                derivation::fingerprint_of(&(
                    module_info.map(|info| (info.workspace_id, info.is_meta)),
                    legacy_envs,
                ))
            }
            FactKey::ModuleName(name) => self.named_modules_fingerprint(name, |_| true),
            FactKey::ModulePath(path) => self
                .named_modules_fingerprint(&lookup_name(path), |info| {
                    info.full_module_name == path.as_str()
                }),
            FactKey::ModuleFiles => self.module_files.get(),
            FactKey::ModuleNamespace(root) => {
                let mut envs = self
                    .legacy_module_envs
                    .iter()
                    .flat_map(|(file_id, envs)| envs.iter().map(move |env| (*file_id, env)))
                    .filter(|(_, env)| namespace_root(&env.module_path) == *root)
                    .map(|(file_id, env)| (file_id, env.module_path.as_str()))
                    .collect::<Vec<_>>();
                if envs.is_empty() {
                    return derivation::ABSENT;
                }
                envs.sort();
                derivation::fingerprint_of(&envs)
            }
            _ => {
                debug_assert!(false, "{key:?} is not a module fact");
                derivation::ABSENT
            }
        }
    }

    /// The fingerprint of the modules named `name` that `filter` keeps.
    fn named_modules_fingerprint(
        &self,
        name: &str,
        filter: impl Fn(&ModuleInfo) -> bool,
    ) -> Fingerprint {
        let Some(file_ids) = self.module_name_to_file_ids.get(name) else {
            return derivation::ABSENT;
        };
        let mut modules = file_ids
            .iter()
            .filter_map(|file_id| self.file_module_map.get(file_id))
            .filter(|info| filter(info))
            .map(|info| {
                (
                    info.file_id,
                    info.full_module_name.as_str(),
                    info.visible,
                    info.workspace_id,
                    &info.version_conds,
                )
            })
            .collect::<Vec<_>>();
        if modules.is_empty() {
            return derivation::ABSENT;
        }
        modules.sort_by_key(|module| module.0);
        derivation::fingerprint_of(&modules)
    }

    pub fn set_legacy_module_env(&mut self, file_id: FileId, env: LegacyModuleEnv) {
        let envs = self.legacy_module_envs.entry(file_id).or_default();
        envs.push(env);
        envs.sort_by_key(|env| env.activation_position);
        self.note_lookup_keys(file_id);
    }

    pub fn get_legacy_module_env(&self, file_id: FileId) -> Option<&LegacyModuleEnv> {
        derivation::note_file_fact_read(derivation::FileFact::ModuleMeta, file_id);
        self.legacy_module_envs.get(&file_id)?.last()
    }

    pub fn get_legacy_module_env_at(
        &self,
        file_id: FileId,
        position: TextSize,
    ) -> Option<&LegacyModuleEnv> {
        derivation::note_file_fact_read(derivation::FileFact::ModuleMeta, file_id);
        self.legacy_module_envs
            .get(&file_id)?
            .iter()
            .rev()
            .find(|env| position > env.activation_position)
    }

    pub fn has_legacy_module_namespace(&self, name: &str) -> bool {
        derivation::note_read(|| FactKey::ModuleNamespace(namespace_root(name)));
        self.legacy_module_envs.values().flatten().any(|env| {
            env.module_path == name
                || env
                    .module_path
                    .strip_prefix(name)
                    .is_some_and(|rest| rest.starts_with('.'))
        })
    }

    pub fn has_legacy_module_namespace_for_file(&self, file_id: FileId, name: &str) -> bool {
        derivation::note_read(|| FactKey::ModuleNamespace(namespace_root(name)));
        let Some(current_workspace_id) = self.get_workspace_id(file_id) else {
            return self.has_legacy_module_namespace(name);
        };

        self.legacy_module_envs
            .iter()
            .any(|(candidate_file_id, envs)| {
                let Some(candidate_workspace_id) = self.get_workspace_id(*candidate_file_id) else {
                    return false;
                };
                self.workspace_resolution_priority(current_workspace_id, candidate_workspace_id)
                    .is_some()
                    && envs.iter().any(|env| {
                        env.module_path == name
                            || env
                                .module_path
                                .strip_prefix(name)
                                .is_some_and(|rest| rest.starts_with('.'))
                    })
            })
    }

    pub fn set_module_visibility(&mut self, file_id: FileId, visible: bool) {
        if let Some(module_info) = self.file_module_map.get_mut(&file_id) {
            module_info.visible = visible;
            self.note_lookup_keys(file_id);
        }
    }

    pub fn set_module_version_conds(
        &mut self,
        file_id: FileId,
        version_conds: Vec<LuaVersionCondition>,
    ) {
        if let Some(module_info) = self.file_module_map.get_mut(&file_id) {
            module_info.version_conds = Some(Box::new(version_conds));
            self.note_lookup_keys(file_id);
        }
    }

    pub fn find_module(&self, module_path: &str) -> Option<&ModuleInfo> {
        self.lookup(module_path, None, true)
    }

    pub fn find_module_in_workspace(
        &self,
        module_path: &str,
        workspace_id: WorkspaceId,
    ) -> Option<&ModuleInfo> {
        self.lookup(module_path, Some(workspace_id), true)
    }

    /// The file of the module [`Self::find_module_for_file`] finds, for a
    /// reader that needs only which file it is: records the lookup, not the
    /// module's contents.
    pub fn find_module_file_for_file(&self, module_path: &str, file_id: FileId) -> Option<FileId> {
        self.lookup(module_path, self.get_workspace_id(file_id), false)
            .map(|module| module.file_id)
    }

    /// [`Self::find_module_file_for_file`] by a file path.
    pub fn find_module_file_by_path_for_file(
        &self,
        path: &Path,
        file_id: FileId,
    ) -> Option<FileId> {
        let (module_path, _) = self.extract_module_path(path.to_str()?)?;
        self.find_module_file_for_file(&module_path, file_id)
    }

    /// Looks `module_path` up, within `workspace_id` when given, recording the
    /// modules at its path or name and, when `contents` is set, the module
    /// found.
    fn lookup(
        &self,
        module_path: &str,
        workspace_id: Option<WorkspaceId>,
        contents: bool,
    ) -> Option<&ModuleInfo> {
        let module_path = module_path.replace(['\\', '/'], ".");
        let module_parts: Vec<&str> = module_path.split('.').collect();
        if module_parts.is_empty() {
            return None;
        }

        let exact = match workspace_id {
            Some(workspace_id) => self.exact_find_module_in_workspace(&module_parts, workspace_id),
            None => self.exact_find_module(&module_parts),
        };
        if let Some(module) = exact {
            derivation::note_read(|| FactKey::ModulePath(SmolStr::new(&module_path)));
            if contents {
                derivation::note_read(|| FactKey::Module(module.file_id));
            }
            return Some(module);
        }
        let result = if self.fuzzy_search {
            let name = module_parts.last()?;
            match workspace_id {
                Some(workspace_id) => {
                    self.fuzzy_find_module_in_workspace(&module_path, name, workspace_id)
                }
                None => self.fuzzy_find_module(&module_path, name),
            }
        } else {
            None
        };
        derivation::note_read(|| FactKey::ModuleName(lookup_name(&module_path)));
        if contents && let Some(module) = result {
            derivation::note_read(|| FactKey::Module(module.file_id));
        }
        result
    }

    pub fn find_module_by_path(&self, path: &Path) -> Option<&ModuleInfo> {
        let path = path.to_str()?;
        let (module_path, _) = self.extract_module_path(path)?;
        self.find_module(&module_path)
    }

    pub fn find_module_for_file(&self, module_path: &str, file_id: FileId) -> Option<&ModuleInfo> {
        if let Some(workspace_id) = self.get_workspace_id(file_id) {
            self.find_module_in_workspace(module_path, workspace_id)
        } else {
            self.find_module(module_path)
        }
    }

    pub fn find_module_by_path_in_workspace(
        &self,
        path: &Path,
        workspace_id: WorkspaceId,
    ) -> Option<&ModuleInfo> {
        let path = path.to_str()?;
        let (module_path, _) = self.extract_module_path(path)?;
        self.find_module_in_workspace(&module_path, workspace_id)
    }

    pub fn find_module_by_path_for_file(
        &self,
        path: &Path,
        file_id: FileId,
    ) -> Option<&ModuleInfo> {
        if let Some(workspace_id) = self.get_workspace_id(file_id) {
            return self.find_module_by_path_in_workspace(path, workspace_id);
        }

        self.find_module_by_path(path)
    }

    fn exact_find_module(&self, module_parts: &Vec<&str>) -> Option<&ModuleInfo> {
        let mut parent_node_id = self.module_root_id;
        for part in module_parts {
            let parent_node = self.module_nodes.get(&parent_node_id)?;
            let child_id = *parent_node.children.get(*part)?;
            parent_node_id = child_id;
        }

        let node = self.module_nodes.get(&parent_node_id)?;
        self.select_module_globally(&node.file_ids)
    }

    fn exact_find_module_in_workspace(
        &self,
        module_parts: &Vec<&str>,
        workspace_id: WorkspaceId,
    ) -> Option<&ModuleInfo> {
        let mut parent_node_id = self.module_root_id;
        for part in module_parts {
            let parent_node = self.module_nodes.get(&parent_node_id)?;
            let child_id = *parent_node.children.get(*part)?;
            parent_node_id = child_id;
        }

        let node = self.module_nodes.get(&parent_node_id)?;
        self.select_module_for_workspace(&node.file_ids, workspace_id)
    }

    /// Find a module by suffix when exact lookup fails.
    ///
    /// Candidates must either exactly equal `module_path` or end with `.{module_path}`.
    /// Among matches, prefer the one with the fewest leading path segments before the suffix,
    /// then use lexicographic `full_module_name` ordering as a stable tie-break, and finally
    /// fall back to the same deterministic ordering used by global module resolution.
    fn fuzzy_find_module(&self, module_path: &str, last_name: &str) -> Option<&ModuleInfo> {
        let file_ids = self.module_name_to_file_ids.get(last_name)?;
        let suffix_with_boundary = format!(".{}", module_path);
        let mut best: Option<(&ModuleInfo, usize)> = None;
        for file_id in file_ids {
            let Some(module_info) = self.file_module_map.get(file_id) else {
                continue;
            };

            let full_module_name = module_info.full_module_name.as_str();
            let Some(leading_segment_count) = (if full_module_name == module_path {
                Some(0)
            } else {
                full_module_name
                    .strip_suffix(&suffix_with_boundary)
                    .map(|prefix| {
                        prefix
                            .split('.')
                            .filter(|segment| !segment.is_empty())
                            .count()
                    })
            }) else {
                continue;
            };

            match best {
                Some((best_module, best_leading_count))
                    if !self.is_candidate_better_for_fuzzy_global(
                        module_info,
                        leading_segment_count,
                        best_module,
                        best_leading_count,
                    ) => {}
                _ => best = Some((module_info, leading_segment_count)),
            }
        }

        best.map(|(module_info, _)| module_info)
    }

    fn fuzzy_find_module_in_workspace(
        &self,
        module_path: &str,
        last_name: &str,
        workspace_id: WorkspaceId,
    ) -> Option<&ModuleInfo> {
        let file_ids = self.module_name_to_file_ids.get(last_name)?;
        let suffix_with_boundary = format!(".{module_path}");
        let mut best: Option<(&ModuleInfo, u8)> = None;

        for file_id in file_ids {
            let Some(module_info) = self.file_module_map.get(file_id) else {
                continue;
            };
            let full_module_name = module_info.full_module_name.as_str();
            if full_module_name != module_path && !full_module_name.ends_with(&suffix_with_boundary)
            {
                continue;
            }

            let Some(priority) =
                self.workspace_resolution_priority(workspace_id, module_info.workspace_id)
            else {
                continue;
            };

            match best {
                Some((best_module, best_priority))
                    if priority > best_priority
                        || (priority == best_priority
                            && !self.is_candidate_better_for_workspace(
                                module_info,
                                best_module,
                                workspace_id,
                            )) => {}
                _ => best = Some((module_info, priority)),
            }
        }

        best.map(|(module_info, _)| module_info)
    }

    fn select_module_for_workspace(
        &self,
        file_ids: &[FileId],
        workspace_id: WorkspaceId,
    ) -> Option<&ModuleInfo> {
        let mut best: Option<(&ModuleInfo, u8)> = None;
        for file_id in file_ids {
            let Some(module_info) = self.file_module_map.get(file_id) else {
                continue;
            };
            let Some(priority) =
                self.workspace_resolution_priority(workspace_id, module_info.workspace_id)
            else {
                continue;
            };

            match best {
                Some((best_module, best_priority))
                    if priority > best_priority
                        || (priority == best_priority
                            && !self.is_candidate_better_for_workspace(
                                module_info,
                                best_module,
                                workspace_id,
                            )) => {}
                _ => best = Some((module_info, priority)),
            }
        }

        best.map(|(module_info, _)| module_info)
    }

    fn select_module_globally(&self, file_ids: &[FileId]) -> Option<&ModuleInfo> {
        let mut best: Option<&ModuleInfo> = None;
        for file_id in file_ids {
            let Some(module_info) = self.file_module_map.get(file_id) else {
                continue;
            };
            match best {
                Some(best_module) if !self.is_candidate_better_global(module_info, best_module) => {
                }
                _ => best = Some(module_info),
            }
        }

        best
    }

    fn is_candidate_better_for_workspace(
        &self,
        candidate: &ModuleInfo,
        current_best: &ModuleInfo,
        current_workspace_id: WorkspaceId,
    ) -> bool {
        (
            candidate.workspace_id != current_workspace_id,
            self.workspace_order_priority(candidate.workspace_id),
            self.module_path_key(candidate.file_id).is_none(),
            self.module_path_key(candidate.file_id),
            candidate.full_module_name.as_str(),
            candidate.file_id,
        ) < (
            current_best.workspace_id != current_workspace_id,
            self.workspace_order_priority(current_best.workspace_id),
            self.module_path_key(current_best.file_id).is_none(),
            self.module_path_key(current_best.file_id),
            current_best.full_module_name.as_str(),
            current_best.file_id,
        )
    }

    fn is_candidate_better_global(
        &self,
        candidate: &ModuleInfo,
        current_best: &ModuleInfo,
    ) -> bool {
        (
            self.workspace_kind_priority(candidate.workspace_id),
            self.workspace_order_priority(candidate.workspace_id),
            self.module_path_key(candidate.file_id).is_none(),
            self.module_path_key(candidate.file_id),
            candidate.full_module_name.as_str(),
            candidate.file_id,
        ) < (
            self.workspace_kind_priority(current_best.workspace_id),
            self.workspace_order_priority(current_best.workspace_id),
            self.module_path_key(current_best.file_id).is_none(),
            self.module_path_key(current_best.file_id),
            current_best.full_module_name.as_str(),
            current_best.file_id,
        )
    }

    fn is_candidate_better_for_fuzzy_global(
        &self,
        candidate: &ModuleInfo,
        candidate_leading_segment_count: usize,
        current_best: &ModuleInfo,
        current_best_leading_segment_count: usize,
    ) -> bool {
        (
            candidate_leading_segment_count,
            candidate.full_module_name.as_str(),
        ) < (
            current_best_leading_segment_count,
            current_best.full_module_name.as_str(),
        ) || (candidate_leading_segment_count == current_best_leading_segment_count
            && candidate.full_module_name == current_best.full_module_name
            && self.is_candidate_better_global(candidate, current_best))
    }

    fn workspace_order_priority(&self, workspace_id: WorkspaceId) -> (usize, WorkspaceId) {
        (
            self.workspaces
                .iter()
                .position(|workspace| workspace.id == workspace_id)
                .unwrap_or(usize::MAX),
            workspace_id,
        )
    }

    /// The precedence key one declaration site carries for a shared `---`
    /// payload. See [`LuaDeclSourceKey`]; a file with no module info is
    /// treated as a main-workspace file, matching the loose-file default.
    pub(crate) fn decl_source_key(
        &self,
        file_id: FileId,
        file_order_key: Arc<str>,
        range: TextRange,
    ) -> LuaDeclSourceKey {
        let workspace_id = self.get_workspace_id(file_id).unwrap_or(WorkspaceId::MAIN);
        let (workspace_order, _) = self.workspace_order_priority(workspace_id);
        LuaDeclSourceKey {
            workspace_kind: self.workspace_kind_priority(workspace_id),
            workspace_order,
            workspace_id: workspace_id.id,
            file_order_key,
            file_id: file_id.id,
            range: (range.start(), range.end()),
        }
    }

    fn workspace_kind_priority(&self, workspace_id: WorkspaceId) -> u8 {
        match self.get_workspace_kind(workspace_id) {
            WorkspaceKind::Main => 0,
            WorkspaceKind::Library => 1,
            WorkspaceKind::Std => 2,
            WorkspaceKind::Remote => 3,
        }
    }

    fn module_path_key(&self, file_id: FileId) -> Option<&str> {
        self.file_module_paths
            .get(&file_id)
            .map(|path| path.as_str())
    }

    /// Find a module node by module path.
    /// The module path is a string separated by dots.
    /// For example, "a.b.c" represents the module "c" in the module "b" in the module "a".
    pub fn find_module_node(&self, module_path: &str) -> Option<&ModuleNode> {
        derivation::note_read(|| FactKey::ModuleFiles);
        if module_path.is_empty() {
            return self.module_nodes.get(&self.module_root_id);
        }

        let module_path = module_path.replace(['\\', '/'], ".");
        let module_parts: Vec<&str> = module_path.split('.').collect();
        if module_parts.is_empty() {
            return None;
        }

        let mut parent_node_id = self.module_root_id;
        for part in &module_parts {
            let parent_node = self.module_nodes.get(&parent_node_id)?;
            let child_id = parent_node.children.get(*part)?;
            parent_node_id = *child_id;
        }

        self.module_nodes.get(&parent_node_id)
    }

    pub fn get_module_node(&self, module_id: &ModuleNodeId) -> Option<&ModuleNode> {
        derivation::note_read(|| FactKey::ModuleFiles);
        self.module_nodes.get(module_id)
    }

    /// Sorted by file id: the result reaches completion output, so hash order
    /// would let two runs of the same workspace disagree.
    pub fn get_module_infos(&self) -> Vec<&ModuleInfo> {
        derivation::note_read(|| FactKey::ModuleFiles);
        let mut module_infos: Vec<&ModuleInfo> = self.file_module_map.values().collect();
        if derivation::recording() {
            for module_info in &module_infos {
                derivation::note_read(|| FactKey::Module(module_info.file_id));
            }
        }
        module_infos.sort_unstable_by_key(|module_info| module_info.file_id);
        module_infos
    }

    pub fn get_workspace_kind(&self, workspace_id: WorkspaceId) -> WorkspaceKind {
        if let Some(kind) = self.workspace_kind_map.get(&workspace_id) {
            return *kind;
        }

        if workspace_id == WorkspaceId::STD {
            WorkspaceKind::Std
        } else if workspace_id == WorkspaceId::MAIN {
            WorkspaceKind::Main
        } else if workspace_id == WorkspaceId::REMOTE {
            WorkspaceKind::Remote
        } else {
            WorkspaceKind::Library
        }
    }

    pub fn is_main_workspace_id(&self, workspace_id: WorkspaceId) -> bool {
        self.get_workspace_kind(workspace_id) == WorkspaceKind::Main
    }

    pub fn get_main_workspace_ids(&self) -> Vec<WorkspaceId> {
        self.workspaces
            .iter()
            .filter(|w| self.get_workspace_kind(w.id) == WorkspaceKind::Main)
            .map(|w| w.id)
            .collect()
    }

    pub fn is_std_workspace_id(&self, workspace_id: WorkspaceId) -> bool {
        self.get_workspace_kind(workspace_id) == WorkspaceKind::Std
    }

    pub fn is_library_workspace_id(&self, workspace_id: WorkspaceId) -> bool {
        self.get_workspace_kind(workspace_id) == WorkspaceKind::Library
    }

    pub fn is_remote_workspace_id(&self, workspace_id: WorkspaceId) -> bool {
        self.get_workspace_kind(workspace_id) == WorkspaceKind::Remote
    }

    pub fn workspace_isolation_enabled(&self) -> bool {
        self.workspace_isolation_enabled
    }

    pub fn workspace_resolution_priority(
        &self,
        current_workspace_id: WorkspaceId,
        candidate_workspace_id: WorkspaceId,
    ) -> Option<u8> {
        if current_workspace_id == candidate_workspace_id {
            return Some(0);
        }

        let current_kind = self.get_workspace_kind(current_workspace_id);
        let candidate_kind = self.get_workspace_kind(candidate_workspace_id);
        match current_kind {
            WorkspaceKind::Std => {
                if candidate_kind == WorkspaceKind::Std {
                    Some(0)
                } else {
                    None
                }
            }
            WorkspaceKind::Main => match candidate_kind {
                WorkspaceKind::Main if !self.workspace_isolation_enabled => Some(0),
                // Library (e.g. GMod annotations) takes priority over std so user-provided
                // type definitions override the built-in Lua stdlib definitions.
                WorkspaceKind::Library => Some(1),
                WorkspaceKind::Std => Some(2),
                WorkspaceKind::Main | WorkspaceKind::Remote => None,
            },
            WorkspaceKind::Library => match candidate_kind {
                WorkspaceKind::Library => Some(1),
                WorkspaceKind::Std => Some(2),
                WorkspaceKind::Main | WorkspaceKind::Remote => None,
            },
            WorkspaceKind::Remote => match candidate_kind {
                WorkspaceKind::Library => Some(1),
                WorkspaceKind::Std => Some(2),
                WorkspaceKind::Remote => Some(3),
                WorkspaceKind::Main => None,
            },
        }
    }

    /// Extends the broad workspace-kind priority with configured library order.
    ///
    /// For colliding definitions, earlier `workspace.library` entries are
    /// authoritative. The workspace ID is only a stable final fallback.
    pub(crate) fn workspace_resolution_key(
        &self,
        current_workspace_id: WorkspaceId,
        candidate_workspace_id: WorkspaceId,
    ) -> Option<WorkspaceResolutionKey> {
        let tier =
            self.workspace_resolution_priority(current_workspace_id, candidate_workspace_id)?;
        let candidate_kind = self.get_workspace_kind(candidate_workspace_id);
        let library_order = if candidate_kind == WorkspaceKind::Library {
            self.workspace_registration_order(candidate_workspace_id)
                .unwrap_or(usize::MAX)
        } else {
            0
        };
        let stable_workspace_id = if candidate_kind == WorkspaceKind::Library {
            candidate_workspace_id
        } else {
            WorkspaceId { id: 0 }
        };

        Some(WorkspaceResolutionKey::new(
            tier,
            library_order,
            stable_workspace_id,
        ))
    }

    pub(crate) fn workspace_registration_order(&self, workspace_id: WorkspaceId) -> Option<usize> {
        self.workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)
    }

    pub(crate) fn get_workspace_root(&self, workspace_id: WorkspaceId) -> Option<&Path> {
        self.workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .map(|workspace| workspace.root.as_path())
    }

    pub fn extract_module_path(&self, path: &str) -> Option<(String, WorkspaceId)> {
        let normalized_path = path.replace('\\', "/");
        let normalized_path = normalized_path.trim_end_matches('/');
        let mut matched_module_path: Option<(String, WorkspaceId)> = None;
        for workspace in &self.workspaces {
            let workspace_root = workspace.root.to_string_lossy().replace('\\', "/");
            let workspace_root = workspace_root.trim_end_matches('/');
            let relative_path_str = if normalized_path == workspace_root {
                ""
            } else if let Some(relative) =
                normalized_path.strip_prefix(&format!("{workspace_root}/"))
            {
                relative
            } else {
                continue;
            };
            if relative_path_str.is_empty() {
                if let Some(file_name) = workspace.root.file_prefix() {
                    let module_path = file_name.to_string_lossy().to_string();
                    return Some((module_path, workspace.id));
                }
            }

            let module_path = self.match_pattern(relative_path_str);
            if let Some(module_path) = module_path {
                if matched_module_path.is_none() {
                    matched_module_path = Some((module_path, workspace.id));
                } else {
                    let (matched, matched_workspace_id) = match matched_module_path.as_ref() {
                        Some((matched, id)) => (matched, id),
                        None => continue,
                    };
                    if module_path.len() < matched.len() {
                        // Libraries could be in a subdirectory of the main workspace
                        // In case of a conflict, we prioritise the non-main workspace ID
                        let workspace_id = if workspace.kind == WorkspaceKind::Main {
                            *matched_workspace_id
                        } else {
                            workspace.id
                        };
                        matched_module_path = Some((module_path, workspace_id));
                    }
                }
            }
        }

        matched_module_path
    }

    fn replace_module_path(&self, module_path: &str) -> String {
        let mut module_path = module_path.to_owned();
        for (key, value) in &self.module_replace_vec {
            if let std::borrow::Cow::Owned(o) = key.replace_all(&module_path, value) {
                module_path = o;
            }
        }

        module_path
    }

    pub fn match_pattern(&self, path: &str) -> Option<String> {
        for pattern in &self.module_patterns {
            if let Some(captures) = pattern.captures(path)
                && let Some(matched) = captures.get(1)
            {
                return Some(matched.as_str().to_string());
            }
        }

        None
    }

    pub fn add_workspace_root(&mut self, root: PathBuf, workspace_id: WorkspaceId) {
        let workspace_kind = self.get_workspace_kind(workspace_id);
        self.add_workspace_root_with_kind(root, workspace_id, workspace_kind);
    }

    pub fn add_workspace_root_with_kind(
        &mut self,
        root: PathBuf,
        workspace_id: WorkspaceId,
        workspace_kind: WorkspaceKind,
    ) {
        if let Some(existing_workspace) = self.workspaces.iter().find(|w| w.root == root) {
            self.workspace_kind_map
                .insert(existing_workspace.id, existing_workspace.kind);
            return;
        }

        self.workspaces
            .push(Workspace::new(root, workspace_id, workspace_kind));
        self.workspace_kind_map.insert(workspace_id, workspace_kind);
    }

    pub fn next_main_workspace_id(&self) -> u32 {
        let used: FxHashSet<u32> = self.workspaces.iter().map(|w| w.id.id).collect();
        let mut candidate = WorkspaceId::MAIN.id;
        while candidate == WorkspaceId::REMOTE.id || used.contains(&candidate) {
            candidate += 1;
        }
        candidate
    }

    pub fn next_library_workspace_id(&self) -> u32 {
        let used: FxHashSet<u32> = self.workspaces.iter().map(|w| w.id.id).collect();
        let mut candidate = WorkspaceId::REMOTE.id + 1;
        while used.contains(&candidate) {
            candidate += 1;
        }
        candidate
    }

    /// Every workspace root, with its id and kind.
    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    /// Unregisters every main and library workspace root.
    pub fn remove_main_and_library_workspaces(&mut self) {
        let user_root =
            |kind: &WorkspaceKind| matches!(kind, WorkspaceKind::Main | WorkspaceKind::Library);
        self.workspaces
            .retain(|workspace| !user_root(&workspace.kind));
        self.workspace_kind_map.retain(|_, kind| !user_root(kind));
    }

    pub fn update_config(&mut self, config: Arc<Emmyrc>) {
        let mut extension_names = Vec::new();

        for extension in &config.runtime.extensions {
            if let Some(stripped) = extension
                .strip_prefix(".")
                .or_else(|| extension.strip_prefix("*."))
            {
                extension_names.push(stripped.to_string());
            } else {
                extension_names.push(extension.clone());
            }
        }

        if !extension_names.contains(&"lua".to_string()) {
            extension_names.push("lua".to_string());
        }

        let mut patterns = Vec::new();
        for extension in &extension_names {
            patterns.push(format!("?.{}", extension));
        }

        let require_pattern = config.runtime.require_pattern.clone();
        if require_pattern.is_empty() {
            // add default require pattern
            for extension in &extension_names {
                patterns.push(format!("?/init.{}", extension));
            }
        } else {
            patterns.extend(require_pattern);
        }

        self.set_module_extract_patterns(patterns);
        self.set_module_replace_patterns(
            config
                .workspace
                .module_map
                .iter()
                .map(|m| (m.pattern.clone(), m.replace.clone()))
                .collect(),
        );

        self.workspace_isolation_enabled = config.workspace.enable_isolation;
        self.fuzzy_search = !config.strict.require_path;
    }

    pub fn get_std_file_ids(&self) -> Vec<FileId> {
        derivation::note_read(|| FactKey::ModuleFiles);
        let mut file_ids = Vec::new();
        for module_info in self.file_module_map.values() {
            if self.is_std_workspace_id(module_info.workspace_id) {
                file_ids.push(module_info.file_id);
            }
        }

        file_ids.sort_unstable();
        file_ids
    }

    pub fn is_main(&self, file_id: &FileId) -> bool {
        derivation::note_file_fact_read(derivation::FileFact::ModuleMeta, *file_id);
        if let Some(module_info) = self.file_module_map.get(file_id) {
            return self.is_main_workspace_id(module_info.workspace_id);
        }

        false
    }

    pub fn is_std(&self, file_id: &FileId) -> bool {
        derivation::note_file_fact_read(derivation::FileFact::ModuleMeta, *file_id);
        if let Some(module_info) = self.file_module_map.get(file_id) {
            return self.is_std_workspace_id(module_info.workspace_id);
        }

        false
    }

    pub fn is_library(&self, file_id: &FileId) -> bool {
        derivation::note_file_fact_read(derivation::FileFact::ModuleMeta, *file_id);
        if let Some(module_info) = self.file_module_map.get(file_id) {
            return self.is_library_workspace_id(module_info.workspace_id);
        }

        false
    }

    pub fn get_main_workspace_file_ids(&self) -> Vec<FileId> {
        derivation::note_read(|| FactKey::ModuleFiles);
        let mut file_ids = Vec::new();
        for module_info in self.file_module_map.values() {
            if self.is_main_workspace_id(module_info.workspace_id) {
                file_ids.push(module_info.file_id);
            }
        }
        file_ids.sort_unstable();

        file_ids
    }

    pub fn get_lib_file_ids(&self) -> Vec<FileId> {
        derivation::note_read(|| FactKey::ModuleFiles);
        let mut file_ids = Vec::new();
        for module_info in self.file_module_map.values() {
            if self.is_library_workspace_id(module_info.workspace_id) {
                file_ids.push(module_info.file_id);
            }
        }
        file_ids.sort_unstable();

        file_ids
    }

    pub fn set_meta(&mut self, file_id: FileId) {
        if let Some(module_info) = self.file_module_map.get_mut(&file_id) {
            module_info.is_meta = true;
            note_module_fact_write(file_id);
        }
    }

    pub fn is_meta_file(&self, file_id: &FileId) -> bool {
        derivation::note_file_fact_read(derivation::FileFact::ModuleMeta, *file_id);
        if let Some(module_info) = self.file_module_map.get(file_id) {
            return module_info.is_meta;
        }

        false
    }

    pub fn get_workspace_id(&self, file_id: FileId) -> Option<WorkspaceId> {
        derivation::note_file_fact_read(derivation::FileFact::ModuleMeta, file_id);
        if let Some(module_info) = self.file_module_map.get(&file_id) {
            return Some(module_info.workspace_id);
        }

        None
    }

    pub fn get_workspace_id_for_root(&self, root: &Path) -> Option<WorkspaceId> {
        self.workspaces
            .iter()
            .find(|w| w.root == root)
            .map(|w| w.id)
    }

    pub fn get_main_workspace_roots(&self) -> Vec<PathBuf> {
        self.workspaces
            .iter()
            .filter(|workspace| workspace.kind == WorkspaceKind::Main)
            .map(|workspace| workspace.root.clone())
            .collect()
    }
}

impl LuaIndex for LuaModuleIndex {
    fn remove(&mut self, file_id: FileId) {
        // Dropping the export is a novel fact for export readers, mirroring
        // the signature index's removal tombstones.
        if self.file_module_map.contains_key(&file_id) {
            self.note_module_write(file_id);
            derivation::note_write(|| FactKey::ModuleFiles);
        }
        self.note_lookup_keys(file_id);
        if let Some(name) = self
            .file_module_map
            .get(&file_id)
            .map(|info| info.name.clone())
            && let Some(file_ids) = self.module_name_to_file_ids.get_mut(&name)
        {
            file_ids.retain(|id| *id != file_id);
            if file_ids.is_empty() {
                self.module_name_to_file_ids.remove(&name);
            }
        }
        let (mut parent_id, mut child_id) =
            if let Some(module_info) = self.file_module_map.remove(&file_id) {
                self.relink_export(file_id, export_files(&module_info));
                self.module_files.remove(&module_files_entry(&module_info));
                let module_id = module_info.module_id;
                let node = match self.module_nodes.get_mut(&module_id) {
                    Some(node) => node,
                    None => return,
                };
                node.file_ids.retain(|id| *id != file_id);
                if node.file_ids.is_empty() && node.children.is_empty() {
                    (node.parent, Some(module_id))
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };

        self.legacy_module_envs.remove(&file_id);
        self.file_module_paths.remove(&file_id);

        if parent_id.is_none() || child_id.is_none() {
            return;
        }

        while let Some(id) = parent_id {
            let child_module_id = match child_id {
                Some(id) => id,
                None => break,
            };
            let node = match self.module_nodes.get_mut(&id) {
                Some(node) => node,
                None => break,
            };
            node.children
                .retain(|_, node_child_idid| *node_child_idid != child_module_id);

            if id == self.module_root_id {
                return;
            }

            if node.file_ids.is_empty() && node.children.is_empty() {
                child_id = Some(id);
                parent_id = node.parent;
                self.module_nodes.remove(&id);
            } else {
                break;
            }
        }
    }

    fn clear(&mut self) {
        for file_id in self
            .file_module_map
            .keys()
            .chain(self.legacy_module_envs.keys())
        {
            self.note_lookup_keys(*file_id);
        }
        derivation::note_write(|| FactKey::ModuleFiles);
        self.module_nodes.clear();
        self.file_module_map.clear();
        self.export_refs.clear();
        self.module_files = derivation::SetFingerprint::default();
        self.file_module_paths.clear();
        self.module_name_to_file_ids.clear();
        self.legacy_module_envs.clear();

        let root_node = ModuleNode::default();
        self.module_nodes.insert(self.module_root_id, root_node);
    }
}

crate::db_index::cow_shards::task_fork_fields!(LuaModuleIndex {
    module_patterns,
    module_root_id,
    module_nodes,
    file_module_map,
    export_refs => crate::db_index::cow_shards::merge_set,
    module_files,
    file_module_paths,
    module_name_to_file_ids,
    legacy_module_envs,
    workspaces,
    workspace_kind_map,
    id_counter,
    fuzzy_search,
    module_replace_vec,
    workspace_isolation_enabled,
});
