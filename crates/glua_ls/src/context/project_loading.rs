use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use glua_code_analysis::{
    GmodProject, GmodProjectKind, GmodWorkspaceTopology, WorkspaceFolder, WorkspaceImport,
    file_path_to_uri, uri_to_file_path,
};
use lsp_types::Uri;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GmodProjectLoadingOptions {
    #[serde(default)]
    pub interactive_gamemode_selection: bool,
    pub selected_gamemode_uri: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LoadedProjectKind {
    Addon,
    Gamemode,
    Workspace,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GamemodeRole {
    Primary,
    Base,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadedProject {
    pub id: String,
    pub kind: LoadedProjectKind,
    pub name: String,
    pub root_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<GamemodeRole>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GamemodeCandidate {
    pub id: String,
    pub name: String,
    pub root_uri: String,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum GamemodeChoiceReason {
    Initial,
    DocumentOpen,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChooseGamemodeParams {
    pub candidates: Vec<GamemodeCandidate>,
    pub current_gamemode_id: Option<String>,
    pub requested_gamemode_id: Option<String>,
    pub reason: GamemodeChoiceReason,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLoadingState {
    pub candidates: Vec<GamemodeCandidate>,
    pub current_gamemode_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChooseGamemodeResult {
    pub selected_gamemode_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenDocumentSnapshot {
    pub uri: Uri,
    pub text: String,
    pub version: i32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetActiveGamemodeParams {
    pub selected_gamemode_id: String,
    #[serde(default)]
    pub open_documents: Vec<OpenDocumentSnapshot>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetActiveGamemodeResult {
    pub selected_gamemode_id: String,
}

#[derive(Clone, Debug)]
pub struct TrackedOpenDocument {
    pub text: String,
    pub version: i32,
}

#[derive(Clone, Debug)]
pub struct GmodProjectLoading {
    topology: GmodWorkspaceTopology,
    explicit_workspace_folders: Vec<WorkspaceFolder>,
    active_gamemode_id: Option<String>,
    interactive: bool,
    open_documents: HashMap<Uri, TrackedOpenDocument>,
}

impl GmodProjectLoading {
    pub fn new(
        topology: GmodWorkspaceTopology,
        explicit_workspace_folders: Vec<WorkspaceFolder>,
        interactive: bool,
    ) -> Self {
        Self {
            topology,
            explicit_workspace_folders,
            active_gamemode_id: None,
            interactive,
            open_documents: HashMap::new(),
        }
    }

    pub fn interactive(&self) -> bool {
        self.interactive
    }

    pub fn rediscover(&mut self, explicit_workspace_folders: Vec<WorkspaceFolder>) {
        self.topology = GmodWorkspaceTopology::discover(
            &explicit_workspace_folders
                .iter()
                .map(|workspace| workspace.root.clone())
                .collect::<Vec<_>>(),
        );
        self.explicit_workspace_folders = explicit_workspace_folders;
        if self
            .active_gamemode_id
            .as_deref()
            .is_some_and(|id| !self.is_valid_primary_id(id))
        {
            self.active_gamemode_id = self.sole_primary_gamemode_id();
        }
    }

    pub fn set_active_gamemode(&mut self, id: Option<String>) -> bool {
        if id.as_deref() == self.active_gamemode_id.as_deref() {
            return false;
        }
        self.active_gamemode_id = id;
        true
    }

    pub fn is_valid_primary_id(&self, id: &str) -> bool {
        self.topology
            .primary_gamemodes()
            .any(|project| project.id == id)
    }

    pub fn primary_gamemode_count(&self) -> usize {
        self.topology.primary_gamemodes().count()
    }

    pub fn sole_primary_gamemode_id(&self) -> Option<String> {
        let mut candidates = self.topology.primary_gamemodes();
        let candidate = candidates.next()?;
        candidates.next().is_none().then(|| candidate.id.clone())
    }

    pub fn resolve_persisted_gamemode(&self, value: &str) -> Option<String> {
        self.topology
            .primary_gamemodes()
            .find(|project| {
                project.id == value
                    || file_path_to_uri(&project.root).is_some_and(|uri| uri.as_str() == value)
            })
            .map(|project| project.id.clone())
    }

    pub fn gamemode_candidates(&self) -> Vec<GamemodeCandidate> {
        self.topology
            .primary_gamemodes()
            .filter_map(|project| {
                Some(GamemodeCandidate {
                    id: project.id.clone(),
                    name: project.name.clone(),
                    root_uri: file_path_to_uri(&project.root)?.to_string(),
                })
            })
            .collect()
    }

    pub fn choose_params(
        &self,
        requested_gamemode_id: Option<String>,
        reason: GamemodeChoiceReason,
    ) -> ChooseGamemodeParams {
        ChooseGamemodeParams {
            candidates: self.gamemode_candidates(),
            current_gamemode_id: self.active_gamemode_id.clone(),
            requested_gamemode_id,
            reason,
        }
    }

    pub fn state(&self) -> ProjectLoadingState {
        ProjectLoadingState {
            candidates: self.gamemode_candidates(),
            current_gamemode_id: self.active_gamemode_id.clone(),
        }
    }

    pub fn gamemode_for_uri(&self, uri: &Uri) -> Option<&GmodProject> {
        let path = uri_to_file_path(uri)?;
        self.topology
            .project_containing(&path)
            .filter(|project| project.kind == GmodProjectKind::Gamemode)
    }

    pub fn project_id_for_uri(&self, uri: &Uri) -> Option<String> {
        let path = uri_to_file_path(uri)?;
        if let Some(project) = self.topology.project_containing(&path) {
            return self.is_project_loaded(project).then(|| project.id.clone());
        }

        self.explicit_workspace_folders
            .iter()
            .filter(|workspace| self.has_fallback_workspace(workspace))
            .filter(|workspace| path.starts_with(&workspace.root))
            .max_by_key(|workspace| workspace.root.as_os_str().len())
            .and_then(|workspace| workspace_project_id(&workspace.root))
    }

    pub fn is_gamemode_loaded(&self, id: &str) -> bool {
        self.loaded_gamemode_chain()
            .iter()
            .any(|project| project.id == id)
    }

    pub fn loaded_projects(&self) -> Vec<LoadedProject> {
        let mut projects = Vec::new();
        projects.extend(self.topology.addons().filter_map(|project| {
            Some(LoadedProject {
                id: project.id.clone(),
                kind: LoadedProjectKind::Addon,
                name: project.name.clone(),
                root_uri: file_path_to_uri(&project.root)?.to_string(),
                role: None,
            })
        }));
        projects.extend(
            self.loaded_gamemode_chain()
                .into_iter()
                .enumerate()
                .filter_map(|(index, project)| {
                    Some(LoadedProject {
                        id: project.id.clone(),
                        kind: LoadedProjectKind::Gamemode,
                        name: project.name.clone(),
                        root_uri: file_path_to_uri(&project.root)?.to_string(),
                        role: Some(if index == 0 {
                            GamemodeRole::Primary
                        } else {
                            GamemodeRole::Base
                        }),
                    })
                }),
        );
        projects.extend(
            self.explicit_workspace_folders
                .iter()
                .filter(|workspace| self.has_fallback_workspace(workspace))
                .filter_map(|workspace| {
                    Some(LoadedProject {
                        id: workspace_project_id(&workspace.root)?,
                        kind: LoadedProjectKind::Workspace,
                        name: workspace
                            .root
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("Workspace")
                            .to_string(),
                        root_uri: file_path_to_uri(&workspace.root)?.to_string(),
                        role: None,
                    })
                }),
        );
        projects.sort_by(|left, right| {
            loaded_kind_order(left.kind)
                .cmp(&loaded_kind_order(right.kind))
                .then_with(|| gamemode_role_order(left.role).cmp(&gamemode_role_order(right.role)))
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.root_uri.cmp(&right.root_uri))
        });
        projects
    }

    pub fn loaded_workspace_folders(&self) -> Vec<WorkspaceFolder> {
        let loaded_gamemode_roots = self
            .loaded_gamemode_chain()
            .into_iter()
            .map(|project| project.root.clone())
            .collect::<Vec<_>>();
        let loaded_project_roots = self
            .topology
            .addons()
            .map(|project| project.root.clone())
            .chain(loaded_gamemode_roots.iter().cloned())
            .collect::<Vec<_>>();
        let mut folders = self
            .explicit_workspace_folders
            .iter()
            .map(|workspace| {
                let has_logical_projects = self
                    .topology
                    .projects()
                    .iter()
                    .any(|project| project.root.starts_with(&workspace.root));
                if !has_logical_projects {
                    return workspace.clone();
                }

                let mut imported = loaded_project_roots
                    .iter()
                    .filter_map(|root| {
                        root.strip_prefix(&workspace.root)
                            .ok()
                            .map(Path::to_path_buf)
                    })
                    .collect::<Vec<_>>();
                imported.sort();
                imported.dedup();
                if imported.iter().any(|path| path.as_os_str().is_empty()) {
                    workspace.clone()
                } else {
                    WorkspaceFolder {
                        root: workspace.root.clone(),
                        import: WorkspaceImport::SubPaths(imported),
                        is_library: workspace.is_library,
                    }
                }
            })
            .collect::<Vec<_>>();

        for base_root in loaded_gamemode_roots.into_iter().skip(1) {
            if self
                .explicit_workspace_folders
                .iter()
                .any(|workspace| base_root.starts_with(&workspace.root))
            {
                continue;
            }
            folders.push(WorkspaceFolder::new(base_root, true));
        }

        folders
    }

    pub fn loaded_gamemode_roots(&self) -> Vec<PathBuf> {
        self.loaded_gamemode_chain()
            .into_iter()
            .map(|project| project.root.clone())
            .collect()
    }

    pub fn update_open_document(&mut self, uri: Uri, text: String, version: i32) {
        self.open_documents
            .insert(uri, TrackedOpenDocument { text, version });
    }

    pub fn remove_open_document(&mut self, uri: &Uri) {
        self.open_documents.remove(uri);
    }

    pub fn merge_open_document_snapshots(&mut self, snapshots: Vec<OpenDocumentSnapshot>) {
        for snapshot in snapshots {
            self.update_open_document(snapshot.uri, snapshot.text, snapshot.version);
        }
    }

    pub fn open_documents_in_loaded_projects(&self) -> Vec<(Uri, TrackedOpenDocument)> {
        self.open_documents
            .iter()
            .filter(|(uri, _)| {
                self.gamemode_for_uri(uri)
                    .is_none_or(|project| self.is_gamemode_loaded(&project.id))
            })
            .map(|(uri, document)| (uri.clone(), document.clone()))
            .collect()
    }

    fn is_project_loaded(&self, project: &GmodProject) -> bool {
        project.kind == GmodProjectKind::Addon
            || self
                .loaded_gamemode_chain()
                .iter()
                .any(|loaded| loaded.id == project.id)
    }

    fn loaded_gamemode_chain(&self) -> Vec<&GmodProject> {
        let Some(primary_id) = self.active_gamemode_id.as_deref() else {
            return Vec::new();
        };
        self.topology
            .gamemode_chain(primary_id)
            .into_iter()
            .enumerate()
            .filter(|(index, project)| {
                *index == 0 || !is_annotation_backed_builtin_gamemode(&project.name)
            })
            .map(|(_, project)| project)
            .collect()
    }

    fn has_fallback_workspace(&self, workspace: &WorkspaceFolder) -> bool {
        !self
            .topology
            .projects()
            .iter()
            .any(|project| project.root.starts_with(&workspace.root))
    }
}

fn workspace_project_id(root: &Path) -> Option<String> {
    let uri = file_path_to_uri(&root.to_path_buf())?;
    Some(format!("workspace:{}", uri.as_str()))
}

fn is_annotation_backed_builtin_gamemode(name: &str) -> bool {
    name.eq_ignore_ascii_case("base") || name.eq_ignore_ascii_case("sandbox")
}

const fn loaded_kind_order(kind: LoadedProjectKind) -> u8 {
    match kind {
        LoadedProjectKind::Addon => 0,
        LoadedProjectKind::Gamemode => 1,
        LoadedProjectKind::Workspace => 2,
    }
}

const fn gamemode_role_order(role: Option<GamemodeRole>) -> u8 {
    match role {
        Some(GamemodeRole::Primary) => 0,
        Some(GamemodeRole::Base) => 1,
        None => 2,
    }
}

pub fn import_contains(workspace: &WorkspaceFolder, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(&workspace.root) else {
        return false;
    };
    match &workspace.import {
        WorkspaceImport::All => true,
        WorkspaceImport::SubPaths(paths) => {
            paths.iter().any(|sub_path| relative.starts_with(sub_path))
        }
        WorkspaceImport::AllExcept(paths) => {
            !paths.iter().any(|sub_path| relative.starts_with(sub_path))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use glua_code_analysis::GmodWorkspaceTopology;

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be valid")
            .as_nanos();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("gluals_loading_{nanos}_{counter}"));
        fs::create_dir_all(&root).expect("temp root should be created");
        root
    }

    fn create_gamemode(root: &Path, name: &str, base: Option<&str>) -> PathBuf {
        let gamemode = root.join("gamemodes").join(name);
        fs::create_dir_all(&gamemode).expect("gamemode should be created");
        let base_line = base
            .map(|base| format!("\"base\" \"{base}\"\n"))
            .unwrap_or_default();
        fs::write(
            gamemode.join(format!("{name}.txt")),
            format!("\"{name}\"\n{{\n{base_line}}}\n"),
        )
        .expect("manifest should be written");
        gamemode
    }

    #[test]
    fn logical_projects_exclude_unselected_gamemodes_and_unclassified_server_files() {
        let root = temp_dir();
        fs::create_dir_all(root.join("addons").join("example").join("lua"))
            .expect("addon should be created");
        let selected = create_gamemode(&root, "selected", None);
        let unselected = create_gamemode(&root, "unselected", None);
        let explicit = vec![WorkspaceFolder::new(root.clone(), false)];
        let topology = GmodWorkspaceTopology::discover(std::slice::from_ref(&root));
        let selected_id = topology
            .primary_gamemodes()
            .find(|project| project.name == "selected")
            .expect("selected gamemode should exist")
            .id
            .clone();
        let mut loading = GmodProjectLoading::new(topology, explicit, false);
        loading.set_active_gamemode(Some(selected_id));
        let folders = loading.loaded_workspace_folders();
        let workspace = folders
            .iter()
            .find(|workspace| workspace.root == root)
            .expect("explicit root should remain");

        assert!(import_contains(
            workspace,
            &root
                .join("addons")
                .join("example")
                .join("lua")
                .join("init.lua")
        ));
        assert!(import_contains(workspace, &selected.join("init.lua")));
        assert!(!import_contains(workspace, &unselected.join("init.lua")));
        assert!(!import_contains(
            workspace,
            &root.join("cfg").join("server.lua")
        ));
        fs::remove_dir_all(root).expect("temp root should be removed");
    }

    #[test]
    fn custom_base_is_loaded_while_builtin_base_remains_annotation_backed() {
        let root = temp_dir();
        create_gamemode(&root, "base", None);
        create_gamemode(&root, "sandbox", Some("base"));
        create_gamemode(&root, "framework", Some("sandbox"));
        create_gamemode(&root, "roleplay", Some("framework"));
        let explicit = vec![WorkspaceFolder::new(root.clone(), false)];
        let topology = GmodWorkspaceTopology::discover(std::slice::from_ref(&root));
        let roleplay_id = topology
            .primary_gamemodes()
            .find(|project| project.name == "roleplay")
            .expect("roleplay should exist")
            .id
            .clone();
        let mut loading = GmodProjectLoading::new(topology, explicit, false);
        loading.set_active_gamemode(Some(roleplay_id));
        let projects = loading.loaded_projects();
        let names = projects
            .iter()
            .filter(|project| project.kind == LoadedProjectKind::Gamemode)
            .map(|project| (project.name.as_str(), project.role))
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                ("roleplay", Some(GamemodeRole::Primary)),
                ("framework", Some(GamemodeRole::Base)),
            ]
        );
        fs::remove_dir_all(root).expect("temp root should be removed");
    }
}
