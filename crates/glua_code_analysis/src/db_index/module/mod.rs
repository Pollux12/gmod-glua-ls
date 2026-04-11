mod legacy_module_env;
mod module_info;
mod module_node;
mod test;
mod workspace;

use glua_parser::LuaVersionCondition;
pub use legacy_module_env::LegacyModuleEnv;
use log::{error, info};
pub use module_info::ModuleInfo;
pub use module_node::{ModuleNode, ModuleNodeId};
use regex::Regex;
use rowan::TextSize;
pub use workspace::{Workspace, WorkspaceId, WorkspaceKind};

use super::traits::LuaIndex;
use crate::{Emmyrc, FileId};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug)]
pub struct LuaModuleIndex {
    module_patterns: Vec<Regex>,
    module_root_id: ModuleNodeId,
    module_nodes: HashMap<ModuleNodeId, ModuleNode>,
    file_module_map: HashMap<FileId, ModuleInfo>,
    module_name_to_file_ids: HashMap<String, Vec<FileId>>,
    legacy_module_envs: HashMap<FileId, Vec<LegacyModuleEnv>>,
    workspaces: Vec<Workspace>,
    workspace_kind_map: HashMap<WorkspaceId, WorkspaceKind>,
    id_counter: u32,
    fuzzy_search: bool,
    module_replace_vec: Vec<(Regex, String)>,
    workspace_isolation_enabled: bool,
}

impl Default for LuaModuleIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaModuleIndex {
    pub fn new() -> Self {
        let mut index = Self {
            module_patterns: Vec::new(),
            module_root_id: ModuleNodeId { id: 0 },
            module_nodes: HashMap::new(),
            file_module_map: HashMap::new(),
            module_name_to_file_ids: HashMap::new(),
            legacy_module_envs: HashMap::new(),
            workspaces: Vec::new(),
            workspace_kind_map: HashMap::new(),
            id_counter: 1,
            fuzzy_search: false,
            module_replace_vec: Vec::new(),
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

        let (module_path, workspace_id) = self.extract_module_path(path)?;
        let mut module_path = module_path.replace(['\\', '/'], ".");
        if !self.module_replace_vec.is_empty() {
            module_path = self.replace_module_path(&module_path);
        }

        self.add_module_by_module_path(file_id, module_path, workspace_id);
        Some(workspace_id)
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
                    children: HashMap::new(),
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
        let module_name = match module_parts.last() {
            Some(name) => name.to_string(),
            None => return None,
        };
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

        self.file_module_map.insert(file_id, module_info);
        if self.fuzzy_search {
            self.module_name_to_file_ids
                .entry(module_name)
                .or_default()
                .push(file_id);
        }

        Some(())
    }

    pub fn get_module(&self, file_id: FileId) -> Option<&ModuleInfo> {
        self.file_module_map.get(&file_id)
    }

    pub fn get_module_mut(&mut self, file_id: FileId) -> Option<&mut ModuleInfo> {
        self.file_module_map.get_mut(&file_id)
    }

    pub fn set_legacy_module_env(&mut self, file_id: FileId, env: LegacyModuleEnv) {
        let envs = self.legacy_module_envs.entry(file_id).or_default();
        envs.push(env);
        envs.sort_by_key(|env| env.activation_position);
    }

    pub fn get_legacy_module_env(&self, file_id: FileId) -> Option<&LegacyModuleEnv> {
        self.legacy_module_envs.get(&file_id)?.last()
    }

    pub fn get_legacy_module_env_at(
        &self,
        file_id: FileId,
        position: TextSize,
    ) -> Option<&LegacyModuleEnv> {
        self.legacy_module_envs
            .get(&file_id)?
            .iter()
            .rev()
            .find(|env| position > env.activation_position)
    }

    pub fn has_legacy_module_namespace(&self, name: &str) -> bool {
        self.legacy_module_envs.values().flatten().any(|env| {
            env.module_path == name
                || env
                    .module_path
                    .strip_prefix(name)
                    .is_some_and(|rest| rest.starts_with('.'))
        })
    }

    pub fn has_legacy_module_namespace_for_file(&self, file_id: FileId, name: &str) -> bool {
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
        }
    }

    pub fn set_module_version_conds(
        &mut self,
        file_id: FileId,
        version_conds: Vec<LuaVersionCondition>,
    ) {
        if let Some(module_info) = self.file_module_map.get_mut(&file_id) {
            module_info.version_conds = Some(Box::new(version_conds));
        }
    }

    pub fn find_module(&self, module_path: &str) -> Option<&ModuleInfo> {
        let module_path = module_path.replace(['\\', '/'], ".");
        let module_parts: Vec<&str> = module_path.split('.').collect();
        if module_parts.is_empty() {
            return None;
        }

        let result = self.exact_find_module(&module_parts);
        if result.is_some() {
            return result;
        }

        if self.fuzzy_search {
            let last_name = module_parts.last()?;

            return self.fuzzy_find_module(&module_path, last_name);
        }

        None
    }

    pub fn find_module_in_workspace(
        &self,
        module_path: &str,
        workspace_id: WorkspaceId,
    ) -> Option<&ModuleInfo> {
        let module_path = module_path.replace(['\\', '/'], ".");
        let module_parts: Vec<&str> = module_path.split('.').collect();
        if module_parts.is_empty() {
            return None;
        }

        let result = self.exact_find_module_in_workspace(&module_parts, workspace_id);
        if result.is_some() {
            return result;
        }

        if self.fuzzy_search {
            let last_name = module_parts.last()?;
            return self.fuzzy_find_module_in_workspace(&module_path, last_name, workspace_id);
        }

        None
    }

    pub fn find_module_by_path(&self, path: &Path) -> Option<&ModuleInfo> {
        let path = path.to_str()?;
        let (module_path, _) = self.extract_module_path(path)?;
        self.find_module(&module_path)
    }

    pub fn find_module_for_file(&self, module_path: &str, file_id: FileId) -> Option<&ModuleInfo> {
        if let Some(workspace_id) = self.get_workspace_id(file_id) {
            return self.find_module_in_workspace(module_path, workspace_id);
        }

        self.find_module(module_path)
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
            let child_id = match parent_node.children.get(*part) {
                Some(id) => *id,
                None => return None,
            };
            parent_node_id = child_id;
        }

        let node = self.module_nodes.get(&parent_node_id)?;
        let file_id = node.file_ids.first()?;
        self.file_module_map.get(file_id)
    }

    fn exact_find_module_in_workspace(
        &self,
        module_parts: &Vec<&str>,
        workspace_id: WorkspaceId,
    ) -> Option<&ModuleInfo> {
        let mut parent_node_id = self.module_root_id;
        for part in module_parts {
            let parent_node = self.module_nodes.get(&parent_node_id)?;
            let child_id = match parent_node.children.get(*part) {
                Some(id) => *id,
                None => return None,
            };
            parent_node_id = child_id;
        }

        let node = self.module_nodes.get(&parent_node_id)?;
        self.select_module_for_workspace(&node.file_ids, workspace_id)
    }

    /// Find a module by suffix when exact lookup fails.
    ///
    /// Candidates must either exactly equal `module_path` or end with `.{module_path}`.
    /// Among matches, prefer the one with the fewest leading path segments before the suffix,
    /// then use lexicographic `full_module_name` ordering as a stable tie-break.
    fn fuzzy_find_module(&self, module_path: &str, last_name: &str) -> Option<&ModuleInfo> {
        let file_ids = self.module_name_to_file_ids.get(last_name)?;
        let suffix_with_boundary = format!(".{}", module_path);
        file_ids
            .iter()
            .filter_map(|file_id| {
                let module_info = self.file_module_map.get(file_id)?;
                let full_module_name = module_info.full_module_name.as_str();
                let leading_segment_count = if full_module_name == module_path {
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
                }?;

                Some((leading_segment_count, module_info))
            })
            .min_by(|(left_count, left_info), (right_count, right_info)| {
                left_count
                    .cmp(right_count)
                    .then_with(|| left_info.full_module_name.cmp(&right_info.full_module_name))
            })
            .map(|(_, module_info)| module_info)
    }

    fn fuzzy_find_module_in_workspace(
        &self,
        module_path: &str,
        last_name: &str,
        workspace_id: WorkspaceId,
    ) -> Option<&ModuleInfo> {
        let file_ids = self.module_name_to_file_ids.get(last_name)?;
        let mut best: Option<(&ModuleInfo, u8)> = None;

        for file_id in file_ids {
            let module_info = self.file_module_map.get(file_id)?;
            if !module_info.full_module_name.ends_with(module_path) {
                continue;
            }

            let Some(priority) =
                self.workspace_resolution_priority(workspace_id, module_info.workspace_id)
            else {
                continue;
            };

            match best {
                Some((_best_module, best_priority)) if priority > best_priority => {}
                Some((best_module, best_priority)) if priority == best_priority => {
                    let best_is_current_workspace = best_module.workspace_id == workspace_id;
                    let candidate_is_current_workspace = module_info.workspace_id == workspace_id;
                    if !best_is_current_workspace && candidate_is_current_workspace {
                        best = Some((module_info, priority));
                    }
                }
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
            let module_info = self.file_module_map.get(file_id)?;
            let Some(priority) =
                self.workspace_resolution_priority(workspace_id, module_info.workspace_id)
            else {
                continue;
            };

            match best {
                Some((_best_module, best_priority)) if priority > best_priority => {}
                Some((best_module, best_priority)) if priority == best_priority => {
                    let best_is_current_workspace = best_module.workspace_id == workspace_id;
                    let candidate_is_current_workspace = module_info.workspace_id == workspace_id;
                    if !best_is_current_workspace && candidate_is_current_workspace {
                        best = Some((module_info, priority));
                    }
                }
                _ => best = Some((module_info, priority)),
            }
        }

        best.map(|(module_info, _)| module_info)
    }

    /// Find a module node by module path.
    /// The module path is a string separated by dots.
    /// For example, "a.b.c" represents the module "c" in the module "b" in the module "a".
    pub fn find_module_node(&self, module_path: &str) -> Option<&ModuleNode> {
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
        self.module_nodes.get(module_id)
    }

    pub fn get_module_infos(&self) -> Vec<&ModuleInfo> {
        self.file_module_map.values().collect()
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
        let used: HashSet<u32> = self.workspaces.iter().map(|w| w.id.id).collect();
        let mut candidate = WorkspaceId::MAIN.id;
        while candidate == WorkspaceId::REMOTE.id || used.contains(&candidate) {
            candidate += 1;
        }
        candidate
    }

    pub fn next_library_workspace_id(&self) -> u32 {
        let used: HashSet<u32> = self.workspaces.iter().map(|w| w.id.id).collect();
        let mut candidate = WorkspaceId::REMOTE.id + 1;
        while used.contains(&candidate) {
            candidate += 1;
        }
        candidate
    }

    #[allow(unused)]
    pub fn remove_workspace_root(&mut self, root: &Path) {
        self.workspaces.retain(|r| r.root != root);
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
        let mut file_ids = Vec::new();
        for module_info in self.file_module_map.values() {
            if self.is_std_workspace_id(module_info.workspace_id) {
                file_ids.push(module_info.file_id);
            }
        }

        file_ids
    }

    pub fn is_main(&self, file_id: &FileId) -> bool {
        if let Some(module_info) = self.file_module_map.get(file_id) {
            return self.is_main_workspace_id(module_info.workspace_id);
        }

        false
    }

    pub fn is_std(&self, file_id: &FileId) -> bool {
        if let Some(module_info) = self.file_module_map.get(file_id) {
            return self.is_std_workspace_id(module_info.workspace_id);
        }

        false
    }

    pub fn is_library(&self, file_id: &FileId) -> bool {
        if let Some(module_info) = self.file_module_map.get(file_id) {
            return self.is_library_workspace_id(module_info.workspace_id);
        }

        false
    }

    pub fn get_main_workspace_file_ids(&self) -> Vec<FileId> {
        let mut file_ids = Vec::new();
        for module_info in self.file_module_map.values() {
            if self.is_main_workspace_id(module_info.workspace_id) {
                file_ids.push(module_info.file_id);
            }
        }

        file_ids
    }

    pub fn get_lib_file_ids(&self) -> Vec<FileId> {
        let mut file_ids = Vec::new();
        for module_info in self.file_module_map.values() {
            if self.is_library_workspace_id(module_info.workspace_id) {
                file_ids.push(module_info.file_id);
            }
        }

        file_ids
    }

    pub fn set_meta(&mut self, file_id: FileId) {
        if let Some(module_info) = self.file_module_map.get_mut(&file_id) {
            module_info.is_meta = true;
        }
    }

    pub fn is_meta_file(&self, file_id: &FileId) -> bool {
        if let Some(module_info) = self.file_module_map.get(file_id) {
            return module_info.is_meta;
        }

        false
    }

    pub fn get_workspace_id(&self, file_id: FileId) -> Option<WorkspaceId> {
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
        let (mut parent_id, mut child_id) =
            if let Some(module_info) = self.file_module_map.remove(&file_id) {
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

        if !self.module_name_to_file_ids.is_empty() {
            let mut module_name = String::new();
            for (name, file_ids) in &self.module_name_to_file_ids {
                if file_ids.contains(&file_id) {
                    module_name = name.clone();
                    break;
                }
            }

            if !module_name.is_empty() {
                let file_ids = match self.module_name_to_file_ids.get_mut(&module_name) {
                    Some(ids) => ids,
                    None => return,
                };

                file_ids.retain(|id| *id != file_id);
                if file_ids.is_empty() {
                    self.module_name_to_file_ids.remove(&module_name);
                }
            }
        }
    }

    fn clear(&mut self) {
        self.module_nodes.clear();
        self.file_module_map.clear();
        self.module_name_to_file_ids.clear();
        self.legacy_module_envs.clear();

        let root_node = ModuleNode::default();
        self.module_nodes.insert(self.module_root_id, root_node);
    }
}
