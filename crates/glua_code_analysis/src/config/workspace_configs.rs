//! The config each workspace root loads with, shared by the language server
//! and `glua_check` so both read a workspace the same way.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::{
    EmmyLibraryItem, Emmyrc, LuaDiagnosticConfig, UnpickedGamemodes, WorkspaceFolder,
    WorkspaceImport, detect_gamemode_base_libraries, is_addons_folder, load_configs,
    scope_workspace_folder, workspace_collection_folders,
};

/// What every root's config is read with besides its own config files.
pub struct ConfigSources<'a> {
    /// Config files read before each root's own, lowest priority first.
    pub global_files: Vec<PathBuf>,
    /// Settings layered over every root's config files.
    pub partial_emmyrcs: Option<Vec<Value>>,
    /// Applied to each config after it is read and before its paths resolve.
    pub prepare: &'a dyn Fn(&mut Emmyrc),
}

/// The merged config of a set of workspace roots and each root's own.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceConfigs {
    /// The roots the configs were loaded for, in workspace order.
    pub roots: Vec<PathBuf>,
    pub emmyrc: Arc<Emmyrc>,
    pub root_emmyrcs: HashMap<PathBuf, Arc<Emmyrc>>,
    pub diagnostic_configs: HashMap<PathBuf, LuaDiagnosticConfig>,
    /// The roots with a config file of their own.
    pub local_config_roots: HashSet<PathBuf>,
}

impl WorkspaceConfigs {
    /// The config `root` is collected and diagnosed with.
    pub fn root_emmyrc(&self, root: &Path) -> &Emmyrc {
        self.root_emmyrcs.get(root).unwrap_or(&self.emmyrc)
    }
}

/// The folders `folders` load, scoped by their own config and the files in
/// `open`, and the configs they load with: each addon and gamemode of a
/// scoped Garry's Mod folder is a config root after `folders`, as a folder of
/// a multi-root workspace would be.
pub fn load_scoped_workspace_configs(
    folders: &[WorkspaceFolder],
    open: &[PathBuf],
    unpicked: UnpickedGamemodes,
    sources: &ConfigSources,
) -> (Vec<WorkspaceFolder>, WorkspaceConfigs) {
    let config_roots = dedup_paths(folders.iter().map(|folder| folder.root.clone()));
    let configs = load_workspace_configs(
        config_roots.clone(),
        &HashSet::new(),
        &HashSet::new(),
        &HashMap::new(),
        sources,
    );
    let mut scoped_folders = Vec::new();
    let mut scoped_roots = HashSet::new();
    let mut parents = HashMap::new();
    let mut changed_scope = false;
    for folder in folders {
        let scoped = scope_workspace_folder(
            folder,
            &configs.root_emmyrc(&folder.root).gmod,
            unpicked,
            open,
            &other_folder_roots(folders, folder),
        );
        if scoped.as_slice() != std::slice::from_ref(folder) {
            // An `addons` folder's addons detect their gamemode bases as the
            // folders of a multi-root workspace do.
            if !is_addons_folder(&folder.root) {
                scoped_roots.extend(scoped.iter().map(|scoped| scoped.root.clone()));
            }
            for scoped in &scoped {
                parents
                    .entry(scoped.root.clone())
                    .or_insert_with(|| folder.root.clone());
            }
            changed_scope = true;
        }
        scoped_folders.extend(scoped);
    }
    if !changed_scope {
        return (scoped_folders, configs);
    }
    let config_roots = config_roots
        .into_iter()
        .chain(scoped_folders.iter().map(|folder| folder.root.clone()))
        .collect();
    let unloaded_roots = scoped_folders
        .iter()
        .filter(|folder| folder.import == WorkspaceImport::None)
        .map(|folder| folder.root.clone())
        .collect();
    let configs = load_workspace_configs(
        config_roots,
        &scoped_roots,
        &unloaded_roots,
        &parents,
        sources,
    );
    (scoped_folders, configs)
}

/// The folders `folders` load with each root's config from `root_emmyrc` and
/// the files in `open`.
pub fn scope_folders<'a>(
    folders: &[WorkspaceFolder],
    open: &[PathBuf],
    unpicked: UnpickedGamemodes,
    root_emmyrc: impl Fn(&Path) -> &'a Emmyrc,
) -> Vec<WorkspaceFolder> {
    folders
        .iter()
        .flat_map(|folder| {
            scope_workspace_folder(
                folder,
                &root_emmyrc(&folder.root).gmod,
                unpicked,
                open,
                &other_folder_roots(folders, folder),
            )
        })
        .collect()
}

/// The roots of the workspace folders besides `folder`, libraries left out.
fn other_folder_roots(folders: &[WorkspaceFolder], folder: &WorkspaceFolder) -> Vec<PathBuf> {
    folders
        .iter()
        .filter(|other| !other.is_library && other.root != folder.root)
        .map(|other| other.root.clone())
        .collect()
}

/// The config of `config_roots`, merged, and each root's own. A root in
/// `scoped_roots` belongs to a scoped Garry's Mod folder, so no gamemode base
/// is detected from it: the folder loads its custom gamemodes itself and the
/// annotations cover the built-in ones. A root in `unloaded_roots` loads none
/// of its files, so its config is its own and stays out of the merged one.
/// A root in `parents` without a config file of its own, an addon or gamemode
/// of an opened Garry's Mod or `addons` folder, or that folder itself, is
/// configured as that folder is, so one addon's config never applies to
/// another's files.
pub fn load_workspace_configs(
    config_roots: Vec<PathBuf>,
    scoped_roots: &HashSet<PathBuf>,
    unloaded_roots: &HashSet<PathBuf>,
    parents: &HashMap<PathBuf, PathBuf>,
    sources: &ConfigSources,
) -> WorkspaceConfigs {
    let config_roots = dedup_paths(config_roots);
    let mut config_files = sources.global_files.clone();
    config_files.extend(preferred_root_config_files(&config_roots));
    let mut emmyrc = load_configs(config_files, sources.partial_emmyrcs.clone());
    (sources.prepare)(&mut emmyrc);

    let mut configs = WorkspaceConfigs {
        roots: config_roots,
        ..WorkspaceConfigs::default()
    };
    if configs.roots.is_empty() {
        configs.emmyrc = Arc::new(emmyrc);
        return configs;
    }

    let mut workspace_configs: Vec<(PathBuf, Emmyrc, bool, bool)> = Vec::new();
    for workspace_root in &configs.roots {
        let local_config_files = config_files_in(workspace_root);
        let has_local_config = !local_config_files.is_empty();
        if has_local_config {
            configs.local_config_roots.insert(workspace_root.clone());
        }
        let parent = parents.get(workspace_root);
        if !has_local_config
            && let Some(parent) = parent.filter(|parent| *parent != workspace_root)
            && let Some((_, parent_emmyrc, _, _)) = workspace_configs
                .iter()
                .find(|(root, _, _, _)| root == parent)
        {
            let inherited = parent_emmyrc.clone();
            workspace_configs.push((workspace_root.clone(), inherited, false, true));
            continue;
        }

        let mut workspace_config_files = sources.global_files.clone();
        workspace_config_files.extend(local_config_files);
        let mut workspace_emmyrc =
            load_configs(workspace_config_files, sources.partial_emmyrcs.clone());
        (sources.prepare)(&mut workspace_emmyrc);
        if !scoped_roots.contains(workspace_root) {
            add_gamemode_base_libraries(&mut workspace_emmyrc, workspace_root);
        }
        workspace_emmyrc.pre_process_emmyrc(workspace_root);
        let keeps_own = has_local_config || parent.is_some();
        workspace_configs.push((
            workspace_root.clone(),
            workspace_emmyrc,
            has_local_config,
            keeps_own,
        ));
    }

    // Isolation can only stay enabled if all workspace configs keep it enabled.
    let isolation_enabled = workspace_configs
        .iter()
        .filter(|(workspace_root, _, _, _)| !unloaded_roots.contains(workspace_root))
        .all(|(_, workspace_emmyrc, _, _)| workspace_emmyrc.workspace.enable_isolation);

    if isolation_enabled {
        let mut merged_emmyrc: Option<Emmyrc> = None;
        for (workspace_root, workspace_emmyrc, _, _) in &workspace_configs {
            configs.diagnostic_configs.insert(
                workspace_root.clone(),
                LuaDiagnosticConfig::new(workspace_emmyrc),
            );
            configs
                .root_emmyrcs
                .insert(workspace_root.clone(), Arc::new(workspace_emmyrc.clone()));

            if unloaded_roots.contains(workspace_root) {
                continue;
            }
            if let Some(merged) = merged_emmyrc.as_mut() {
                merge_isolated_workspace_fields(merged, workspace_emmyrc);
            } else {
                merged_emmyrc = Some(workspace_emmyrc.clone());
            }
        }

        configs.emmyrc = Arc::new(merged_emmyrc.unwrap_or(emmyrc));
        return configs;
    }

    // isolation disabled: build one global merged config while keeping
    // local workspace configs as optional overrides only where present.
    // The baseline is a workspace folder's config, never an addon's or
    // gamemode's of an opened folder, whose config is its own.
    let in_opened_folder = |root: &PathBuf| parents.get(root).is_some_and(|parent| parent != root);
    let baseline_index = workspace_configs
        .iter()
        .position(|(workspace_root, _, has_local, _)| {
            *has_local
                && !unloaded_roots.contains(workspace_root)
                && !in_opened_folder(workspace_root)
        })
        .or_else(|| {
            workspace_configs
                .iter()
                .position(|(workspace_root, _, _, _)| {
                    !unloaded_roots.contains(workspace_root) && !in_opened_folder(workspace_root)
                })
        })
        .unwrap_or(0);

    let mut merged_emmyrc = workspace_configs
        .get(baseline_index)
        .map(|(_, cfg, _, _)| cfg.clone())
        .unwrap_or(emmyrc);

    for (index, (workspace_root, workspace_emmyrc, _, keeps_own)) in
        workspace_configs.into_iter().enumerate()
    {
        if keeps_own {
            configs.diagnostic_configs.insert(
                workspace_root.clone(),
                LuaDiagnosticConfig::new(&workspace_emmyrc),
            );
            configs
                .root_emmyrcs
                .insert(workspace_root.clone(), Arc::new(workspace_emmyrc.clone()));
        }

        if index == baseline_index || unloaded_roots.contains(&workspace_root) {
            continue;
        }

        // An addon or gamemode of an opened folder adds only what every root
        // is analysed with, such as its libraries, never its diagnostics.
        if in_opened_folder(&workspace_root) {
            merge_isolated_workspace_fields(&mut merged_emmyrc, &workspace_emmyrc);
            continue;
        }
        merge_emmyrc_prefer_existing_with_array_union(&mut merged_emmyrc, &workspace_emmyrc);
    }

    // This branch is only entered when at least one workspace disabled isolation.
    merged_emmyrc.workspace.enable_isolation = false;
    configs.emmyrc = Arc::new(merged_emmyrc);
    configs
}

/// Each of `folders` with the folders it registers and the config its files
/// are collected with, its own in `root_emmyrcs` or else `merged`, in
/// registration order. An extra root, library or package directory is
/// collected with the first folder that brings it.
pub fn workspace_collection_groups(
    folders: Vec<WorkspaceFolder>,
    merged: &Arc<Emmyrc>,
    root_emmyrcs: &HashMap<PathBuf, Arc<Emmyrc>>,
) -> Vec<(Arc<Emmyrc>, Vec<WorkspaceFolder>)> {
    let mut configured = HashSet::new();
    folders
        .into_iter()
        .map(|folder| {
            let config = root_emmyrcs.get(&folder.root).unwrap_or(merged).clone();
            let group = workspace_collection_folders(Some(folder), &config, &mut configured);
            (config, group)
        })
        .collect()
}

/// The config files of `dir`, lowest priority first: `.gluarc.json` alone
/// when present, otherwise `.luarc.json`, `.emmyrc.json` and `.emmyrc.lua`.
pub fn config_files_in(dir: &Path) -> Vec<PathBuf> {
    let gluarc = dir.join(".gluarc.json");
    if gluarc.exists() {
        return vec![gluarc];
    }
    [".luarc.json", ".emmyrc.json", ".emmyrc.lua"]
        .into_iter()
        .map(|name| dir.join(name))
        .filter(|path| path.exists())
        .collect()
}

/// The config files of the first of `config_roots` that has any.
fn preferred_root_config_files(config_roots: &[PathBuf]) -> Vec<PathBuf> {
    config_roots
        .iter()
        .map(|root| config_files_in(root))
        .find(|files| !files.is_empty())
        .inspect(|files| log::info!("using preferred workspace config files: {files:?}"))
        .unwrap_or_default()
}

fn dedup_paths(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

/// Adds the gamemodes `workspace_root`'s gamemode derives from (its `.txt`
/// `"base"` chain, e.g. cityrp from sandbox) as libraries, so references into
/// them resolve, unless `gmod.autoDetectGamemodeBase` is `false`.
fn add_gamemode_base_libraries(emmyrc: &mut Emmyrc, workspace_root: &Path) {
    if matches!(emmyrc.gmod.auto_detect_gamemode_base, Some(false)) {
        log::info!("Gamemode base auto-detection explicitly disabled in config");
        return;
    }

    for lib_path in detect_gamemode_base_libraries(workspace_root) {
        let lib_path = lib_path.to_string_lossy().into_owned();
        if emmyrc
            .workspace
            .library
            .iter()
            .any(|item| item.get_path() == &lib_path)
        {
            log::info!("Gamemode base library already exists in workspace library: {lib_path}");
            continue;
        }
        log::info!("Gamemode base library added to workspace library: {lib_path}");
        emmyrc
            .workspace
            .library
            .push(EmmyLibraryItem::Path(lib_path));
    }
}

fn merge_isolated_workspace_fields(merged: &mut Emmyrc, workspace_emmyrc: &Emmyrc) {
    extend_unique(
        &mut merged.workspace.workspace_roots,
        workspace_emmyrc.workspace.workspace_roots.clone(),
    );
    extend_unique(
        &mut merged.workspace.library,
        workspace_emmyrc.workspace.library.clone(),
    );
    extend_unique(
        &mut merged.workspace.package_dirs,
        workspace_emmyrc.workspace.package_dirs.clone(),
    );
    extend_unique(
        &mut merged.workspace.ignore_dir,
        workspace_emmyrc.workspace.ignore_dir.clone(),
    );
    extend_unique(
        &mut merged.workspace.ignore_dir_defaults,
        workspace_emmyrc.workspace.ignore_dir_defaults.clone(),
    );
    extend_unique(
        &mut merged.workspace.ignore_globs,
        workspace_emmyrc.workspace.ignore_globs.clone(),
    );
    merged.workspace.use_default_ignores =
        merged.workspace.use_default_ignores || workspace_emmyrc.workspace.use_default_ignores;
    merged.workspace.enable_isolation =
        merged.workspace.enable_isolation && workspace_emmyrc.workspace.enable_isolation;
    extend_unique(
        &mut merged.runtime.extensions,
        workspace_emmyrc.runtime.extensions.clone(),
    );
    extend_unique(
        &mut merged.runtime.require_pattern,
        workspace_emmyrc.runtime.require_pattern.clone(),
    );
    extend_unique(
        &mut merged.resource.paths,
        workspace_emmyrc.resource.paths.clone(),
    );
}

fn merge_emmyrc_prefer_existing_with_array_union(merged: &mut Emmyrc, incoming: &Emmyrc) {
    let Ok(mut merged_value) = serde_json::to_value(&*merged) else {
        return;
    };
    let Ok(incoming_value) = serde_json::to_value(incoming) else {
        return;
    };

    merge_value_prefer_existing_with_array_union(&mut merged_value, incoming_value);

    if let Ok(new_merged) = serde_json::from_value::<Emmyrc>(merged_value) {
        *merged = new_merged;
    }
}

fn merge_value_prefer_existing_with_array_union(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, overlay_value) in overlay_map {
                if let Some(base_value) = base_map.get_mut(&key) {
                    merge_value_prefer_existing_with_array_union(base_value, overlay_value);
                } else {
                    base_map.insert(key, overlay_value);
                }
            }
        }
        (Value::Array(base_array), Value::Array(overlay_array)) => {
            let mut seen = HashSet::new();
            for item in base_array.iter() {
                if let Ok(key) = serde_json::to_string(item) {
                    seen.insert(key);
                }
            }

            for item in overlay_array {
                if let Ok(key) = serde_json::to_string(&item) {
                    if seen.insert(key) {
                        base_array.push(item);
                    }
                } else if !base_array.contains(&item) {
                    base_array.push(item);
                }
            }
        }
        _ => {
            // Keep existing scalar/object value on conflict.
        }
    }
}

fn extend_unique<T>(target: &mut Vec<T>, incoming: Vec<T>)
where
    T: Eq + Hash + Clone,
{
    let mut seen: HashSet<T> = target.iter().cloned().collect();
    for item in incoming {
        if seen.insert(item.clone()) {
            target.push(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{config_files_in, dedup_paths, preferred_root_config_files};

    fn create_temp_dir() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be valid")
            .as_nanos();
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "glua_workspace_configs_test_{started}_{}_{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("failed to create temp test dir");
        dir
    }

    fn touch(path: &Path) {
        fs::write(path, "{}").expect("failed to create temp config file");
    }

    #[test]
    fn gluarc_json_takes_exclusive_priority() {
        let dir = create_temp_dir();
        let gluarc_json = dir.join(".gluarc.json");
        touch(&gluarc_json);
        touch(&dir.join(".emmyrc.json"));
        touch(&dir.join(".luarc.json"));

        assert_eq!(config_files_in(&dir), vec![gluarc_json]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn config_files_order_luarc_then_emmyrc_json_then_emmyrc_lua() {
        let dir = create_temp_dir();
        let emmyrc_lua = dir.join(".emmyrc.lua");
        let emmyrc_json = dir.join(".emmyrc.json");
        let luarc_json = dir.join(".luarc.json");
        touch(&emmyrc_lua);
        touch(&emmyrc_json);
        touch(&luarc_json);

        assert_eq!(
            config_files_in(&dir),
            vec![luarc_json, emmyrc_json, emmyrc_lua]
        );
        let _ = fs::remove_dir_all(dir);
    }

    /// An addon's own config applies to that addon only: the addons and
    /// gamemodes of a whole Garry's Mod folder without a config of their own
    /// use the folder's, and another workspace folder without one uses the
    /// merged config, which takes no addon's diagnostics.
    #[test]
    fn an_addon_config_applies_to_that_addon_only() {
        let gmod = create_temp_dir().join("garrysmod");
        let other = gmod.parent().unwrap().join("other");
        fs::create_dir_all(&other).unwrap();
        for addon in ["ulx", "wire"] {
            fs::create_dir_all(gmod.join("addons").join(addon).join("lua")).unwrap();
        }
        fs::create_dir_all(gmod.join("gamemodes")).unwrap();
        fs::write(
            gmod.join(".gluarc.json"),
            r#"{ "diagnostics": { "disable": ["unused"] } }"#,
        )
        .unwrap();
        fs::write(
            gmod.join("addons/wire/.gluarc.json"),
            r#"{ "diagnostics": { "disable": ["undefined-global"] } }"#,
        )
        .unwrap();
        let prepare = |_: &mut crate::Emmyrc| {};
        let sources = super::ConfigSources {
            global_files: Vec::new(),
            partial_emmyrcs: None,
            prepare: &prepare,
        };

        let (_, configs) = super::load_scoped_workspace_configs(
            &[
                crate::WorkspaceFolder::new(gmod.clone(), false),
                crate::WorkspaceFolder::new(other.clone(), false),
            ],
            &[],
            crate::UnpickedGamemodes::None,
            &sources,
        );
        let disabled = |root: &Path| {
            configs
                .root_emmyrc(root)
                .diagnostics
                .disable
                .iter()
                .map(|code| format!("{code:?}"))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            (
                disabled(&gmod),
                disabled(&gmod.join("addons/ulx")),
                disabled(&gmod.join("addons/wire")),
                disabled(&other),
            ),
            (
                vec!["Unused".to_string()],
                vec!["Unused".to_string()],
                vec!["UndefinedGlobal".to_string()],
                vec!["Unused".to_string()],
            )
        );
        let _ = fs::remove_dir_all(gmod.parent().unwrap());
    }

    /// With no config in the opened folders, an addon's own config is not
    /// what the other folders use either.
    #[test]
    fn an_addon_config_stays_out_of_folders_without_one() {
        let base = create_temp_dir();
        let gamemode = base.join("ixhl2rp");
        let addons = base.join("addons");
        fs::create_dir_all(&gamemode).unwrap();
        fs::create_dir_all(addons.join("wire/lua")).unwrap();
        fs::write(
            addons.join("wire/.gluarc.json"),
            r#"{ "diagnostics": { "disable": ["undefined-global"] } }"#,
        )
        .unwrap();
        let prepare = |_: &mut crate::Emmyrc| {};
        let sources = super::ConfigSources {
            global_files: Vec::new(),
            partial_emmyrcs: None,
            prepare: &prepare,
        };

        let (_, configs) = super::load_scoped_workspace_configs(
            &[
                crate::WorkspaceFolder::new(gamemode.clone(), false),
                crate::WorkspaceFolder::new(addons.clone(), false),
            ],
            &[],
            crate::UnpickedGamemodes::None,
            &sources,
        );
        let disabled = |root: &Path| configs.root_emmyrc(root).diagnostics.disable.len();

        assert_eq!(
            (disabled(&gamemode), disabled(&addons.join("wire"))),
            (0, 1)
        );
        let _ = fs::remove_dir_all(base);
    }

    /// A reload reads the roots in workspace order, as the first load does,
    /// whichever root's config changed.
    #[test]
    fn dedup_paths_keeps_workspace_order() {
        let path_a = PathBuf::from("/workspace/a");
        let path_b = PathBuf::from("/workspace/b");

        assert_eq!(
            dedup_paths([path_a.clone(), path_b.clone(), path_a.clone()]),
            vec![path_a, path_b]
        );
    }

    #[test]
    fn the_preferred_config_is_the_first_root_with_one() {
        let workspace_a = create_temp_dir();
        let workspace_b = create_temp_dir();
        touch(&workspace_a.join(".emmyrc.json"));
        touch(&workspace_b.join(".emmyrc.json"));

        assert_eq!(
            preferred_root_config_files(&[workspace_a.clone(), workspace_b.clone()]),
            vec![workspace_a.join(".emmyrc.json")]
        );
        let _ = fs::remove_dir_all(workspace_a);
        let _ = fs::remove_dir_all(workspace_b);
    }
}
