//! The config each workspace root loads with, shared by the language server
//! and `glua_check` so both read a workspace the same way.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::{
    ConfigFile, EmmyLibraryItem, Emmyrc, UnpickedGamemodes, WorkspaceFolder, WorkspaceImport,
    detect_gamemode_bases, is_addons_folder, load_configs, load_layered_configs,
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

/// The config of each workspace root, and the shared one the roots have in
/// common.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceConfigs {
    /// The roots the configs were loaded for, in workspace order.
    pub roots: Vec<PathBuf>,
    /// What the roots are parsed and indexed with together: the first loaded
    /// root's config, with the settings every root's files need added.
    pub emmyrc: Arc<Emmyrc>,
    /// Each root's own config, resolved from the defaults, its ancestors'
    /// config files and its own. A file is analysed with its root's.
    pub root_emmyrcs: HashMap<PathBuf, Arc<Emmyrc>>,
    /// The roots with a config file of their own.
    pub local_config_roots: HashSet<PathBuf>,
}

impl WorkspaceConfigs {
    /// The config `root` is collected, analysed and diagnosed with.
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

/// The config of `config_roots`, shared, and each root's own. A root's config
/// layers the defaults, then the config files of its ancestors, then its own,
/// each setting only the keys it names. A root in `scoped_roots` belongs to a
/// scoped Garry's Mod folder, so no gamemode base is detected from it: the
/// folder loads its custom gamemodes itself and the annotations cover the
/// built-in ones. A root in `unloaded_roots` loads none of its files, so its
/// config stays out of the shared one. A root in `parents` is an addon or
/// gamemode of an opened Garry's Mod or `addons` folder, and layers on that
/// folder's config; with no config file of its own it uses the folder's. No
/// root's config reaches a sibling's, so one addon's config never applies to
/// another's files.
pub fn load_workspace_configs(
    config_roots: Vec<PathBuf>,
    scoped_roots: &HashSet<PathBuf>,
    unloaded_roots: &HashSet<PathBuf>,
    parents: &HashMap<PathBuf, PathBuf>,
    sources: &ConfigSources,
) -> WorkspaceConfigs {
    let config_roots = dedup_paths(config_roots);
    let mut shared = load_configs(
        sources.global_files.clone(),
        sources.partial_emmyrcs.clone(),
    );
    (sources.prepare)(&mut shared);

    let mut configs = WorkspaceConfigs {
        roots: config_roots,
        ..WorkspaceConfigs::default()
    };
    if configs.roots.is_empty() {
        configs.emmyrc = Arc::new(shared);
        return configs;
    }

    // An addon or gamemode of an opened folder, configured after every
    // opened folder whatever the workspace order, so it finds its folder's.
    let in_opened_folder = |root: &PathBuf| parents.get(root).is_some_and(|parent| parent != root);
    let config_order = configs
        .roots
        .iter()
        .filter(|root| !in_opened_folder(root))
        .chain(configs.roots.iter().filter(|root| in_opened_folder(root)));
    let mut root_emmyrcs: HashMap<PathBuf, Arc<Emmyrc>> = HashMap::new();
    for root in config_order {
        let own_files = config_files_in(root);
        let has_local_config = !own_files.is_empty();
        if has_local_config {
            configs.local_config_roots.insert(root.clone());
        }
        let parent = parents.get(root).filter(|parent| *parent != root);
        if !has_local_config
            && let Some(parent_emmyrc) = parent.and_then(|parent| root_emmyrcs.get(parent))
        {
            root_emmyrcs.insert(root.clone(), parent_emmyrc.clone());
            continue;
        }

        let mut config_files = sources
            .global_files
            .iter()
            .cloned()
            .map(ConfigFile::from)
            .collect::<Vec<_>>();
        config_files.extend(ancestor_config_files(root, parents));
        config_files.extend(own_files.into_iter().map(ConfigFile::from));
        let mut emmyrc = load_layered_configs(config_files, sources.partial_emmyrcs.clone());
        (sources.prepare)(&mut emmyrc);
        add_gamemode_bases(&mut emmyrc, root, !scoped_roots.contains(root));
        emmyrc.pre_process_emmyrc(root);
        root_emmyrcs.insert(root.clone(), Arc::new(emmyrc));
    }

    let loaded_roots = configs
        .roots
        .iter()
        .filter(|root| !unloaded_roots.contains(*root))
        .collect::<Vec<_>>();
    let base = loaded_roots
        .iter()
        .find(|root| !in_opened_folder(root))
        .or(loaded_roots.first())
        .or(configs.roots.first().as_ref())
        .and_then(|root| root_emmyrcs.get(*root));
    let mut shared = base.map_or(shared, |base| (**base).clone());
    for root in loaded_roots {
        if let Some(root_emmyrc) = root_emmyrcs.get(root) {
            merge_shared_fields(&mut shared, root_emmyrc);
        }
    }
    configs.emmyrc = Arc::new(shared);
    configs.root_emmyrcs = root_emmyrcs;
    configs
}

/// The config files of the ancestors of `root`, the outermost first, each
/// resolving its relative paths against its own folder.
fn ancestor_config_files(root: &Path, parents: &HashMap<PathBuf, PathBuf>) -> Vec<ConfigFile> {
    let mut ancestors = Vec::new();
    let mut current = root;
    while let Some(parent) = parents.get(current).map(PathBuf::as_path)
        && parent != current
        && !ancestors.contains(&parent)
    {
        ancestors.push(parent);
        current = parent;
    }
    ancestors
        .into_iter()
        .rev()
        .flat_map(|ancestor| {
            config_files_in(ancestor)
                .into_iter()
                .map(|path| ConfigFile {
                    path,
                    resolve_against: Some(ancestor.to_path_buf()),
                })
        })
        .collect()
}

/// Each of `folders` with the folders it registers and the config its files
/// are collected with, its own in `root_emmyrcs` or else `merged`, in
/// registration order. An extra root, library or package directory is
/// collected with the first folder that brings it. A folder that is open is
/// collected as itself, never as another folder's library.
pub fn workspace_collection_groups(
    folders: Vec<WorkspaceFolder>,
    merged: &Arc<Emmyrc>,
    root_emmyrcs: &HashMap<PathBuf, Arc<Emmyrc>>,
) -> Vec<(Arc<Emmyrc>, Vec<WorkspaceFolder>)> {
    let mut configured = HashSet::new();
    let open_roots = folders
        .iter()
        .filter(|folder| !folder.is_library)
        .map(|folder| canonical_or_same(&folder.root))
        .collect::<HashSet<_>>();
    folders
        .into_iter()
        .map(|folder| {
            let config = root_emmyrcs.get(&folder.root).unwrap_or(merged).clone();
            let mut group = workspace_collection_folders(Some(folder), &config, &mut configured);
            let own = group.first().map(|own| own.root.clone());
            group.retain(|registered| {
                !registered.is_library
                    || Some(&registered.root) == own.as_ref()
                    || !open_roots.contains(&canonical_or_same(&registered.root))
            });
            (config, group)
        })
        .collect()
}

fn canonical_or_same(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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

fn dedup_paths(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

/// Reads the `.txt` `"base"` chain of the gamemodes under `workspace_root`
/// (e.g. cityrp from sandbox) into `gmod.gamemodeBases`, and adds the gamemodes
/// they derive from as libraries when `load_libraries`, so references into
/// them resolve, unless `gmod.autoDetectGamemodeBase` is `false`.
fn add_gamemode_bases(emmyrc: &mut Emmyrc, workspace_root: &Path, load_libraries: bool) {
    if matches!(emmyrc.gmod.auto_detect_gamemode_base, Some(false)) {
        log::info!("Gamemode base auto-detection explicitly disabled in config");
        return;
    }

    let found = detect_gamemode_bases(workspace_root);
    emmyrc.gmod.gamemode_bases = found.bases;
    if !load_libraries {
        return;
    }
    for lib_path in found.libraries {
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

/// Adds to `shared` what every root's files are parsed and indexed with: the
/// directories, libraries and ignores that decide which files load, and the
/// parser's and module resolution's settings.
fn merge_shared_fields(shared: &mut Emmyrc, root_emmyrc: &Emmyrc) {
    extend_unique(
        &mut shared.workspace.workspace_roots,
        root_emmyrc.workspace.workspace_roots.clone(),
    );
    extend_unique(
        &mut shared.workspace.library,
        root_emmyrc.workspace.library.clone(),
    );
    extend_unique(
        &mut shared.workspace.package_dirs,
        root_emmyrc.workspace.package_dirs.clone(),
    );
    extend_unique(
        &mut shared.workspace.ignore_dir,
        root_emmyrc.workspace.ignore_dir.clone(),
    );
    extend_unique(
        &mut shared.workspace.ignore_dir_defaults,
        root_emmyrc.workspace.ignore_dir_defaults.clone(),
    );
    extend_unique(
        &mut shared.workspace.ignore_globs,
        root_emmyrc.workspace.ignore_globs.clone(),
    );
    shared.workspace.enable_isolation =
        shared.workspace.enable_isolation && root_emmyrc.workspace.enable_isolation;
    extend_unique(
        &mut shared.runtime.extensions,
        root_emmyrc.runtime.extensions.clone(),
    );
    extend_unique(
        &mut shared.runtime.require_pattern,
        root_emmyrc.runtime.require_pattern.clone(),
    );
    extend_unique(
        &mut shared.runtime.require_like_function,
        root_emmyrc.runtime.require_like_function.clone(),
    );
    extend_unique(
        &mut shared.runtime.nonstandard_symbol,
        root_emmyrc.runtime.nonstandard_symbol.clone(),
    );
    for (name, special) in &root_emmyrc.runtime.special {
        shared
            .runtime
            .special
            .entry(name.clone())
            .or_insert_with(|| special.clone());
    }
    extend_unique(
        &mut shared.resource.paths,
        root_emmyrc.resource.paths.clone(),
    );
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

    use super::{config_files_in, dedup_paths};

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
    /// defaults, so neither the Garry's Mod folder's nor an addon's reaches it.
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
                Vec::<String>::new(),
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

    /// An addon's config still adds what every file is parsed and analysed
    /// with, such as its require-like loaders, to the shared config.
    #[test]
    fn an_addon_config_still_shares_its_runtime_settings() {
        let gmod = create_temp_dir().join("garrysmod");
        fs::create_dir_all(gmod.join("addons/wire/lua")).unwrap();
        fs::create_dir_all(gmod.join("gamemodes")).unwrap();
        fs::write(
            gmod.join("addons/wire/.gluarc.json"),
            r#"{ "runtime": { "requireLikeFunction": ["wire_require"] }, "diagnostics": { "disable": ["undefined-global"] } }"#,
        )
        .unwrap();
        let prepare = |_: &mut crate::Emmyrc| {};
        let sources = super::ConfigSources {
            global_files: Vec::new(),
            partial_emmyrcs: None,
            prepare: &prepare,
        };

        let (_, configs) = super::load_scoped_workspace_configs(
            &[crate::WorkspaceFolder::new(gmod.clone(), false)],
            &[],
            crate::UnpickedGamemodes::None,
            &sources,
        );

        assert_eq!(
            (
                configs.emmyrc.runtime.require_like_function.clone(),
                configs.emmyrc.diagnostics.disable.len()
            ),
            (vec!["wire_require".to_string()], 0)
        );
        let _ = fs::remove_dir_all(gmod.parent().unwrap());
    }

    /// A gamemode opened before its whole Garry's Mod folder is configured as
    /// it is when opened after it.
    #[test]
    fn a_gamemode_opened_before_its_gmod_folder_uses_the_folder_config() {
        let gmod = create_temp_dir().join("garrysmod");
        let helix = gmod.join("gamemodes/helix");
        fs::create_dir_all(&helix).unwrap();
        fs::write(
            helix.join("helix.txt"),
            "\"helix\"\n{\n\t\"base\"\t\"sandbox\"\n}\n",
        )
        .unwrap();
        fs::create_dir_all(gmod.join("addons")).unwrap();
        fs::write(
            gmod.join(".gluarc.json"),
            r#"{ "diagnostics": { "disable": ["unused"] } }"#,
        )
        .unwrap();
        let prepare = |_: &mut crate::Emmyrc| {};
        let sources = super::ConfigSources {
            global_files: Vec::new(),
            partial_emmyrcs: None,
            prepare: &prepare,
        };
        let disabled = |folders: &[crate::WorkspaceFolder]| {
            let (_, configs) = super::load_scoped_workspace_configs(
                folders,
                &[],
                crate::UnpickedGamemodes::None,
                &sources,
            );
            configs.root_emmyrc(&helix).diagnostics.disable.len()
        };
        let gamemode = crate::WorkspaceFolder::new(helix.clone(), false);
        let whole = crate::WorkspaceFolder::new(gmod.clone(), false);

        assert_eq!(
            (
                disabled(&[gamemode.clone(), whole.clone()]),
                disabled(&[whole, gamemode])
            ),
            (1, 1)
        );
        let _ = fs::remove_dir_all(gmod.parent().unwrap());
    }

    /// A gamemode of a whole Garry's Mod folder that has a config of its own
    /// still knows the gamemode its manifest derives from.
    #[test]
    fn a_scoped_gamemode_with_its_own_config_knows_its_manifest_base() {
        let gmod = create_temp_dir().join("garrysmod");
        let gamemodes = gmod.join("gamemodes");
        write(
            &gamemodes.join("parent/parent.txt"),
            "\"parent\"
{
	\"base\"	\"sandbox\"
}
",
        );
        write(
            &gamemodes.join("child/child.txt"),
            "\"child\"
{
	\"base\"	\"parent\"
}
",
        );
        write(&gamemodes.join("child/.gluarc.json"), "{}");
        fs::create_dir_all(gmod.join("addons")).unwrap();
        let child = gamemodes.join("child");
        let prepare = |_: &mut crate::Emmyrc| {};
        let sources = super::ConfigSources {
            global_files: Vec::new(),
            partial_emmyrcs: None,
            prepare: &prepare,
        };

        let (folders, configs) = super::load_scoped_workspace_configs(
            &[crate::WorkspaceFolder::new(gmod.clone(), false)],
            &[child.join("gamemode/cl_init.lua")],
            crate::UnpickedGamemodes::None,
            &sources,
        );

        let loaded = folders.iter().map(|f| f.root.clone()).collect::<Vec<_>>();
        assert!(loaded.contains(&child), "{loaded:?}");
        assert_eq!(
            configs
                .root_emmyrc(&child)
                .gmod
                .gamemode_bases
                .get("child")
                .map(String::as_str),
            Some("parent")
        );
        let _ = fs::remove_dir_all(gmod.parent().unwrap());
    }

    /// A gamemode of Steam's `GarrysMod` folder, opened above its `garrysmod`
    /// child, knows the gamemode its manifest derives from without a config of
    /// its own, as it does when `garrysmod` is opened.
    #[test]
    fn a_gamemode_of_steams_garrysmod_folder_knows_its_manifest_base() {
        let steam = create_temp_dir().join("GarrysMod");
        let gamemodes = steam.join("garrysmod/gamemodes");
        write(
            &gamemodes.join("parent/parent.txt"),
            "\"parent\"\n{\n\t\"base\"\t\"sandbox\"\n}\n",
        );
        write(
            &gamemodes.join("child/child.txt"),
            "\"child\"\n{\n\t\"base\"\t\"parent\"\n}\n",
        );
        fs::create_dir_all(steam.join("garrysmod/addons")).unwrap();
        let child = gamemodes.join("child");

        let configs = load(std::slice::from_ref(&steam));

        assert_eq!(
            configs
                .root_emmyrc(&child)
                .gmod
                .gamemode_bases
                .get("child")
                .map(String::as_str),
            Some("parent")
        );
        let _ = fs::remove_dir_all(steam.parent().unwrap());
    }

    fn load(folders: &[PathBuf]) -> super::WorkspaceConfigs {
        let prepare = |_: &mut crate::Emmyrc| {};
        let sources = super::ConfigSources {
            global_files: Vec::new(),
            partial_emmyrcs: None,
            prepare: &prepare,
        };
        let folders = folders
            .iter()
            .map(|root| crate::WorkspaceFolder::new(root.clone(), false))
            .collect::<Vec<_>>();
        super::load_scoped_workspace_configs(
            &folders,
            &[],
            crate::UnpickedGamemodes::None,
            &sources,
        )
        .1
    }

    fn hint(configs: &super::WorkspaceConfigs, root: &Path, param: &str) -> Option<String> {
        configs
            .root_emmyrc(root)
            .gmod
            .file_param_defaults
            .get(param)
            .cloned()
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A later folder's explicit setting is its own, ahead of or after a
    /// folder whose config sets nothing, and reaches no other folder.
    #[test]
    fn a_folders_config_is_its_own_whatever_the_folder_order() {
        let base = create_temp_dir();
        let first = base.join("first");
        let second = base.join("second");
        write(&first.join(".gluarc.json"), "{}");
        write(
            &second.join(".gluarc.json"),
            r#"{ "gmod": { "fileParamDefaults": { "panel": "DHTML" } } }"#,
        );

        let in_order = load(&[first.clone(), second.clone()]);
        let reversed = load(&[second.clone(), first.clone()]);

        for configs in [in_order, reversed] {
            assert_eq!(
                (
                    hint(&configs, &first, "panel"),
                    hint(&configs, &second, "panel")
                ),
                (Some("Panel".to_string()), Some("DHTML".to_string()))
            );
        }
        let _ = fs::remove_dir_all(base);
    }

    /// Sibling folders never merge: each keeps its own explicit values, and a
    /// folder that sets nothing keeps the defaults whatever its siblings set.
    #[test]
    fn sibling_folders_do_not_merge_into_each_other() {
        let base = create_temp_dir();
        let a = base.join("a");
        let b = base.join("b");
        let c = base.join("c");
        write(
            &a.join(".gluarc.json"),
            r#"{ "diagnostics": { "disable": ["unused"] } }"#,
        );
        write(
            &b.join(".gluarc.json"),
            r#"{ "diagnostics": { "disable": ["undefined-global"] } }"#,
        );
        fs::create_dir_all(&c).unwrap();

        let configs = load(&[a.clone(), b.clone(), c.clone()]);
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
            (disabled(&a), disabled(&b), disabled(&c)),
            (
                vec!["Unused".to_string()],
                vec!["UndefinedGlobal".to_string()],
                Vec::<String>::new()
            )
        );
        let _ = fs::remove_dir_all(base);
    }

    /// An addon's config sets only the keys it names; the rest fall through
    /// to its Garry's Mod folder's, and a sibling addon sees none of it.
    #[test]
    fn an_addon_config_keeps_its_folders_other_keys() {
        let gmod = create_temp_dir().join("garrysmod");
        for addon in ["ulx", "wire", "dupe"] {
            fs::create_dir_all(gmod.join("addons").join(addon).join("lua")).unwrap();
        }
        fs::create_dir_all(gmod.join("gamemodes")).unwrap();
        write(
            &gmod.join(".gluarc.json"),
            r#"{ "gmod": { "defaultRealm": "client", "fileParamDefaults": { "panel": "DHTML" } } }"#,
        );
        write(
            &gmod.join("addons/wire/.gluarc.json"),
            r#"{ "gmod": { "defaultRealm": "server", "fileParamDefaults": { "widget": "Widget" } } }"#,
        );

        let configs = load(std::slice::from_ref(&gmod));
        let seen = |addon: &str| {
            let root = gmod.join("addons").join(addon);
            let config = configs.root_emmyrc(&root);
            (
                format!("{:?}", config.gmod.default_realm),
                hint(&configs, &root, "panel"),
                hint(&configs, &root, "widget"),
            )
        };

        assert_eq!(
            (seen("wire"), seen("ulx"), seen("dupe")),
            (
                (
                    "Server".to_string(),
                    Some("DHTML".to_string()),
                    Some("Widget".to_string())
                ),
                ("Client".to_string(), Some("DHTML".to_string()), None),
                ("Client".to_string(), Some("DHTML".to_string()), None),
            )
        );
        let _ = fs::remove_dir_all(gmod.parent().unwrap());
    }

    /// A setting an ancestor's file names is resolved against the ancestor's
    /// folder, even in the config of an addon that layers on it.
    #[test]
    fn an_ancestors_relative_paths_stay_relative_to_the_ancestor() {
        let gmod = create_temp_dir().join("garrysmod");
        fs::create_dir_all(gmod.join("addons/wire/lua")).unwrap();
        fs::create_dir_all(gmod.join("gamemodes")).unwrap();
        write(
            &gmod.join(".gluarc.json"),
            r#"{ "workspace": { "library": ["./shared_libs"] } }"#,
        );
        write(&gmod.join("addons/wire/.gluarc.json"), "{}");

        let configs = load(std::slice::from_ref(&gmod));
        let libraries = configs
            .root_emmyrc(&gmod.join("addons/wire"))
            .workspace
            .library
            .iter()
            .map(|library| PathBuf::from(library.get_path()))
            .collect::<Vec<_>>();

        assert_eq!(libraries, vec![gmod.join("shared_libs")]);
        let _ = fs::remove_dir_all(gmod.parent().unwrap());
    }

    /// A folder that is open stays a main workspace when another open folder
    /// lists it in `workspace.library`, whichever folder comes first.
    #[test]
    fn an_open_folder_listed_as_a_library_stays_main() {
        let base = create_temp_dir();
        let first = base.join("first");
        let second = base.join("second");
        write(
            &first.join(".gluarc.json"),
            r#"{ "workspace": { "library": ["../second"] } }"#,
        );
        write(&second.join("gamemode/cl_second.lua"), "");

        for folders in [
            [first.clone(), second.clone()],
            [second.clone(), first.clone()],
        ] {
            let configs = load(&folders);
            let groups = crate::workspace_collection_groups(
                folders
                    .iter()
                    .map(|root| crate::WorkspaceFolder::new(root.clone(), false))
                    .collect(),
                &configs.emmyrc,
                &configs.root_emmyrcs,
            );
            let mut analysis = crate::EmmyLuaAnalysis::new();
            analysis.set_workspace_roots(groups.iter().flat_map(|(_, group)| group));
            let is_main = |root: &Path| {
                analysis
                    .get_workspace_id_for_root(&crate::normalize_workspace_root(root.to_path_buf()))
                    .map(|id| {
                        analysis
                            .compilation
                            .get_db()
                            .get_module_index()
                            .is_main_workspace_id(id)
                    })
            };

            assert_eq!(
                (is_main(&first), is_main(&second)),
                (Some(true), Some(true)),
                "folders: {folders:?}"
            );
        }
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
}
