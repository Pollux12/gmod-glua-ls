//! A Garry's Mod install opened as one folder, and which of its addons and
//! gamemodes a workspace loads. Each addon and gamemode is registered as its
//! own workspace root, so the folder analyses as the multi-root workspace of
//! the ones it loads would.

use std::path::{Path, PathBuf};

use crate::gamemode_base::{
    find_gamemode_manifest, is_annotated_gamemode, is_valid_gamemode_name,
    read_gamemode_folder_base,
};
use crate::{EmmyrcGmod, WorkspaceFolder, WorkspaceImport};

/// The gamemodes Garry's Mod ships.
pub const BUILTIN_GAMEMODES: [&str; 3] = ["base", "sandbox", "terrortown"];

/// The `gmod.activeGamemode` value that picks every custom gamemode.
pub const ALL_CUSTOM_GAMEMODES: &str = "*";

/// The `gmod.activeGamemode` value that picks no gamemode, for working on addons.
pub const NO_GAMEMODE: &str = "";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gamemode {
    pub name: String,
    pub dir: PathBuf,
    /// The gamemode its manifest's `"base"` names.
    pub derives_from: Option<String>,
    pub builtin: bool,
}

/// The addons and gamemodes of a GMod root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodTopology {
    /// The workspace folder that was opened.
    pub folder: PathBuf,
    /// Its `garrysmod` folder: `folder` or its `garrysmod` child.
    pub root: PathBuf,
    pub addons: Vec<PathBuf>,
    pub gamemodes: Vec<Gamemode>,
}

/// What a GMod root loads with several custom gamemodes and no pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnpickedGamemodes {
    /// None of them: the language server loads each while a file in it is open.
    None,
    /// Every custom gamemode: the command-line tools.
    AllCustom,
}

impl GmodTopology {
    /// The topology of `folder` when it is a GMod root: holding a `garrysmod/`
    /// child, which is then the root (Steam's `GarrysMod/garrysmod`), or
    /// itself named `garrysmod` or holding both `addons/` and `gamemodes/`.
    pub fn discover(folder: &Path) -> Option<Self> {
        let child = folder.join("garrysmod");
        let root = if child.is_dir() {
            child
        } else if is_gmod_root(folder) {
            folder.to_path_buf()
        } else {
            return None;
        };
        Some(Self {
            folder: folder.to_path_buf(),
            addons: sorted_dirs(&root.join("addons")),
            gamemodes: sorted_dirs(&root.join("gamemodes"))
                .into_iter()
                .filter_map(read_gamemode)
                .collect(),
            root,
        })
    }

    pub fn gamemode(&self, name: &str) -> Option<&Gamemode> {
        self.gamemodes
            .iter()
            .find(|gamemode| gamemode.name == name)
            .or_else(|| {
                self.gamemodes
                    .iter()
                    .find(|gamemode| gamemode.name.eq_ignore_ascii_case(name))
            })
    }

    pub fn custom_gamemodes(&self) -> impl Iterator<Item = &Gamemode> {
        self.gamemodes.iter().filter(|gamemode| !gamemode.builtin)
    }

    /// `name` and the gamemodes it derives from, in order; the walk stops at
    /// a gamemode the annotations declare (`base`, `sandbox`), a missing
    /// gamemode or a cycle, so a gamemode deriving from `terrortown` brings it.
    pub fn chain(&self, name: &str) -> Vec<&Gamemode> {
        let Some(start) = self.gamemode(name) else {
            return Vec::new();
        };
        let mut chain = vec![start];
        let mut current = start;
        while let Some(base) = &current.derives_from {
            let Some(next) = self.gamemode(base) else {
                break;
            };
            if is_annotated_gamemode(&next.name) || chain.contains(&next) {
                break;
            }
            chain.push(next);
            current = next;
        }
        chain
    }

    /// The gamemodes loaded whatever is open: the pick (`"*"` for every custom
    /// gamemode, `""` for none) with its chain, or with none the sole custom
    /// gamemode's chain, or with several what `unpicked` says. A pick that
    /// names no gamemode counts as none.
    pub fn picked_gamemodes(
        &self,
        pick: Option<&str>,
        unpicked: UnpickedGamemodes,
    ) -> Vec<&Gamemode> {
        match pick {
            Some(ALL_CUSTOM_GAMEMODES) => return self.custom_gamemodes().collect(),
            Some(NO_GAMEMODE) => return Vec::new(),
            Some(name) if self.gamemode(name).is_some() => return self.chain(name),
            Some(name) => log::warn!(
                "gmod.activeGamemode names no gamemode in {:?}: {name}",
                self.root
            ),
            None => {}
        }
        let mut custom = self.custom_gamemodes();
        match (custom.next(), custom.next(), unpicked) {
            (None, _, _) => Vec::new(),
            (Some(only), None, _) => self.chain(&only.name),
            (Some(_), Some(_), UnpickedGamemodes::None) => Vec::new(),
            (Some(_), Some(_), UnpickedGamemodes::AllCustom) => self.custom_gamemodes().collect(),
        }
    }

    /// The gamemode whose folder holds `path`.
    pub fn gamemode_of(&self, path: &Path) -> Option<&Gamemode> {
        self.gamemodes
            .iter()
            .find(|gamemode| path.starts_with(&gamemode.dir))
    }

    /// The workspace folders the root loads with `picked` and the files in
    /// `open`: every addon and gamemode registered as its own root, each
    /// loaded addon and gamemode whole (a gamemode with an open file and its
    /// chain among them) and the others importing nothing, then the opened
    /// folder with only the open files outside them. The roots are the same
    /// whatever is picked or open.
    pub fn scoped_folders(&self, picked: &[&Gamemode], open: &[PathBuf]) -> Vec<WorkspaceFolder> {
        let mut loaded = picked
            .iter()
            .map(|gamemode| gamemode.name.as_str())
            .collect::<Vec<_>>();
        let mut loose = Vec::new();
        for path in open.iter().filter(|path| path.starts_with(&self.folder)) {
            if self.addons.iter().any(|addon| path.starts_with(addon)) {
                continue;
            }
            match self.gamemode_of(path) {
                Some(gamemode) => loaded.extend(
                    self.chain(&gamemode.name)
                        .into_iter()
                        .map(|gamemode| gamemode.name.as_str()),
                ),
                None => {
                    if let Ok(relative) = path.strip_prefix(&self.folder) {
                        loose.push(relative.to_path_buf());
                    }
                }
            }
        }
        loose.sort();
        loose.dedup();

        let addons = self
            .addons
            .iter()
            .map(|addon| WorkspaceFolder::new(addon.clone(), false));
        let gamemodes = self.gamemodes.iter().map(|gamemode| {
            if loaded.contains(&gamemode.name.as_str()) {
                WorkspaceFolder::new(gamemode.dir.clone(), false)
            } else {
                WorkspaceFolder {
                    root: gamemode.dir.clone(),
                    import: WorkspaceImport::None,
                    is_library: false,
                }
            }
        });
        addons
            .chain(gamemodes)
            .chain(std::iter::once(WorkspaceFolder::with_sub_paths(
                self.folder.clone(),
                loose,
                false,
            )))
            .collect()
    }
}

/// The folders `folder` loads: a GMod root's addons and gamemodes, each its
/// own root, scoped by `gmod` and the files in `open`; any other folder, a
/// library, or a GMod root under `gmod.loadEverything`, as it is.
pub fn scope_workspace_folder(
    folder: &WorkspaceFolder,
    gmod: &EmmyrcGmod,
    unpicked: UnpickedGamemodes,
    open: &[PathBuf],
) -> Vec<WorkspaceFolder> {
    if folder.is_library || gmod.load_everything || folder.import != WorkspaceImport::All {
        return vec![folder.clone()];
    }
    match GmodTopology::discover(&folder.root) {
        Some(topology) => topology.scoped_folders(
            &topology.picked_gamemodes(gmod.active_gamemode.as_deref(), unpicked),
            open,
        ),
        None => vec![folder.clone()],
    }
}

fn is_gmod_root(dir: &Path) -> bool {
    dir.is_dir()
        && (dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("garrysmod"))
            || (dir.join("addons").is_dir() && dir.join("gamemodes").is_dir()))
}

fn sorted_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

fn read_gamemode(dir: PathBuf) -> Option<Gamemode> {
    let name = dir.file_name()?.to_str()?.to_string();
    if !is_valid_gamemode_name(&name) {
        return None;
    }
    find_gamemode_manifest(&dir)?;
    let derives_from = read_gamemode_folder_base(&dir).filter(|base| is_valid_gamemode_name(base));
    let builtin = BUILTIN_GAMEMODES
        .iter()
        .any(|builtin| name.eq_ignore_ascii_case(builtin));
    Some(Gamemode {
        name,
        dir,
        derives_from,
        builtin,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "glua_gmod_workspace_{}_{}_{name}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn gamemode(root: &Path, name: &str, manifest: &str, base: Option<&str>) {
        let dir = root.join("gamemodes").join(name);
        fs::create_dir_all(dir.join("gamemode")).expect("create gamemode");
        let base = base.map_or(String::new(), |base| format!("\t\"base\"\t\"{base}\"\n"));
        fs::write(
            dir.join(format!("{manifest}.txt")),
            format!("\"{name}\"\n{{\n{base}\t\"title\"\t\"{name}\"\n}}\n"),
        )
        .expect("write manifest");
    }

    /// A `garrysmod` folder with two addons, the built-ins, and `helix-hl2rp`
    /// deriving from `helix` deriving from `sandbox`.
    fn install(name: &str) -> PathBuf {
        let root = temp_dir(name).join("garrysmod");
        for addon in ["wiremod", "ulx"] {
            fs::create_dir_all(root.join("addons").join(addon).join("lua")).expect("addon");
        }
        gamemode(&root, "base", "base", None);
        gamemode(&root, "sandbox", "sandbox", Some("base"));
        gamemode(&root, "terrortown", "terrortown", Some("base"));
        gamemode(&root, "helix", "helix", Some("sandbox"));
        gamemode(&root, "helix-hl2rp", "ixhl2rp", Some("helix"));
        fs::create_dir_all(root.join("lua").join("autorun")).expect("lua");
        root
    }

    fn names(gamemodes: &[&Gamemode]) -> Vec<String> {
        gamemodes
            .iter()
            .map(|gamemode| gamemode.name.clone())
            .collect()
    }

    fn loaded(folders: &[WorkspaceFolder]) -> Vec<PathBuf> {
        folders
            .iter()
            .filter(|folder| matches!(folder.import, WorkspaceImport::All))
            .map(|folder| folder.root.clone())
            .collect()
    }

    #[test]
    fn a_gmod_root_is_named_garrysmod_holds_addons_and_gamemodes_or_holds_one() {
        let named = install("named");
        let parent = named.parent().expect("parent").to_path_buf();
        let both = temp_dir("both");
        fs::create_dir_all(both.join("addons")).expect("addons");
        fs::create_dir_all(both.join("gamemodes")).expect("gamemodes");
        let addon = temp_dir("addon");
        fs::create_dir_all(addon.join("gamemodes").join("mine")).expect("gamemodes");
        fs::create_dir_all(addon.join("lua")).expect("lua");

        assert_eq!(
            [&named, &parent, &both, &addon]
                .map(|folder| { GmodTopology::discover(folder).map(|topology| topology.root) }),
            [
                Some(named.clone()),
                Some(named.clone()),
                Some(both.clone()),
                None
            ]
        );
    }

    /// Steam installs to `GarrysMod/garrysmod`; the outer folder's name
    /// matches `garrysmod` but its addons and gamemodes are in the inner one.
    #[test]
    fn steams_garrysmod_folder_is_scoped_to_its_garrysmod_child() {
        let steam = temp_dir("steam").join("GarrysMod");
        let root = steam.join("garrysmod");
        fs::create_dir_all(root.join("addons").join("ulx")).expect("addon");
        gamemode(&root, "helix", "helix", Some("sandbox"));

        let topology = GmodTopology::discover(&steam).expect("gmod root");

        assert_eq!(
            (
                &topology.folder,
                &topology.root,
                topology.addons.len(),
                names(&topology.gamemodes.iter().collect::<Vec<_>>())
            ),
            (&steam, &root, 1, vec!["helix".to_string()])
        );
    }

    #[test]
    fn a_gamemode_needs_a_manifest_which_may_be_named_otherwise() {
        let root = install("manifest");
        fs::create_dir_all(root.join("gamemodes").join("no_manifest")).expect("folder");
        let topology = GmodTopology::discover(&root).expect("gmod root");

        assert_eq!(
            topology
                .gamemodes
                .iter()
                .map(|gamemode| (gamemode.name.as_str(), gamemode.builtin))
                .collect::<Vec<_>>(),
            [
                ("base", true),
                ("helix", false),
                ("helix-hl2rp", false),
                ("sandbox", true),
                ("terrortown", true)
            ]
        );
    }

    #[test]
    fn a_chain_follows_custom_gamemodes_and_stops_at_a_builtin() {
        let topology = GmodTopology::discover(&install("chain")).expect("gmod root");

        assert_eq!(
            (
                names(&topology.chain("helix-hl2rp")),
                names(&topology.chain("sandbox"))
            ),
            (
                vec!["helix-hl2rp".to_string(), "helix".to_string()],
                vec!["sandbox".to_string()]
            )
        );
    }

    /// The annotations declare `base` and `sandbox` but not `terrortown`, so
    /// a gamemode deriving from `terrortown` loads it too, as the base
    /// detection of a single gamemode folder does.
    #[test]
    fn a_chain_brings_terrortown_which_the_annotations_do_not_declare() {
        let root = install("terrortown_chain");
        gamemode(&root, "ttt_custom", "ttt_custom", Some("terrortown"));
        let topology = GmodTopology::discover(&root).expect("gmod root");

        assert_eq!(
            (
                names(&topology.chain("ttt_custom")),
                names(&topology.chain("terrortown"))
            ),
            (
                vec!["ttt_custom".to_string(), "terrortown".to_string()],
                vec!["terrortown".to_string()]
            )
        );
    }

    #[test]
    fn a_chain_rejects_a_base_that_leaves_the_gamemodes_folder() {
        let root = install("traversal");
        gamemode(&root, "escape", "escape", Some("../../addons/ulx"));
        let topology = GmodTopology::discover(&root).expect("gmod root");

        assert_eq!(names(&topology.chain("escape")), vec!["escape".to_string()]);
    }

    #[test]
    fn the_pick_decides_the_loaded_gamemodes() {
        let root = install("pick");
        let topology = GmodTopology::discover(&root).expect("gmod root");
        let picked = |pick, unpicked| names(&topology.picked_gamemodes(pick, unpicked));

        assert_eq!(
            [
                picked(Some("helix-hl2rp"), UnpickedGamemodes::None),
                picked(Some("sandbox"), UnpickedGamemodes::None),
                picked(Some("*"), UnpickedGamemodes::None),
                picked(Some(""), UnpickedGamemodes::AllCustom),
                picked(Some("missing"), UnpickedGamemodes::None),
                picked(None, UnpickedGamemodes::None),
                picked(None, UnpickedGamemodes::AllCustom),
            ],
            [
                vec!["helix-hl2rp".to_string(), "helix".to_string()],
                vec!["sandbox".to_string()],
                vec!["helix".to_string(), "helix-hl2rp".to_string()],
                Vec::new(),
                Vec::new(),
                Vec::new(),
                vec!["helix".to_string(), "helix-hl2rp".to_string()],
            ]
        );
    }

    #[test]
    fn a_sole_custom_gamemode_is_picked_with_its_chain() {
        let root = install("sole");
        fs::remove_dir_all(root.join("gamemodes").join("helix-hl2rp")).expect("remove");
        let topology = GmodTopology::discover(&root).expect("gmod root");

        assert_eq!(
            names(&topology.picked_gamemodes(None, UnpickedGamemodes::None)),
            vec!["helix".to_string()]
        );
    }

    #[test]
    fn an_open_file_loads_its_gamemode_with_its_chain_or_itself_alone() {
        let root = install("open");
        let topology = GmodTopology::discover(&root).expect("gmod root");
        let open = [
            root.join("gamemodes/helix-hl2rp/gamemode/init.lua"),
            root.join("lua/autorun/loose.lua"),
            root.join("addons/ulx/lua/autorun/ulx.lua"),
        ];

        let folders = topology.scoped_folders(&[], &open);

        let last = folders.last().expect("the opened folder");
        assert_eq!(
            (loaded(&folders), &last.root, &last.import),
            (
                vec![
                    root.join("addons/ulx"),
                    root.join("addons/wiremod"),
                    root.join("gamemodes/helix"),
                    root.join("gamemodes/helix-hl2rp"),
                ],
                &root,
                &WorkspaceImport::SubPaths(vec![PathBuf::from("lua/autorun/loose.lua")])
            )
        );
    }

    /// Each loaded file's module path and the root of its workspace, with
    /// `folders` registered and collected in order.
    fn modules(folders: &[WorkspaceFolder]) -> Vec<(PathBuf, String, PathBuf)> {
        let mut analysis = crate::EmmyLuaAnalysis::new();
        for folder in folders {
            analysis.add_main_workspace(folder.root.clone());
        }
        let files = crate::collect_workspace_files(&folders.to_vec(), &analysis.emmyrc, None, None);
        analysis.update_files_by_path(files.into_iter().map(|file| file.into_tuple()).collect());
        let db = analysis.compilation.get_db();
        let module_index = db.get_module_index();
        let mut modules = db
            .get_vfs()
            .get_all_local_file_ids()
            .into_iter()
            .filter(|file_id| !module_index.is_std(file_id))
            .map(|file_id| {
                let module = module_index.get_module(file_id).expect("module");
                let root = module_index
                    .workspaces()
                    .iter()
                    .find(|workspace| workspace.id == module.workspace_id)
                    .expect("workspace")
                    .root
                    .clone();
                (
                    db.get_vfs().get_file_path(&file_id).expect("path").clone(),
                    module.full_module_name.clone(),
                    root,
                )
            })
            .collect::<Vec<_>>();
        modules.sort();
        modules
    }

    #[test]
    fn a_scoped_gmod_root_loads_as_the_multi_root_workspace_of_its_loaded_folders() {
        let root = install("multi_root");
        for file in [
            "addons/ulx/lua/ulib/shared.lua",
            "addons/wiremod/lua/autorun/wire.lua",
            "gamemodes/helix/gamemode/init.lua",
            "gamemodes/helix-hl2rp/gamemode/init.lua",
            "gamemodes/sandbox/gamemode/init.lua",
            "lua/autorun/loose.lua",
        ] {
            let path = root.join(file);
            fs::create_dir_all(path.parent().expect("parent")).expect("create folder");
            fs::write(path, "local value = 1\n").expect("write lua");
        }
        let topology = GmodTopology::discover(&root).expect("gmod root");
        let multi_root = [
            "addons/ulx",
            "addons/wiremod",
            "gamemodes/helix",
            "gamemodes/helix-hl2rp",
        ]
        .map(|folder| WorkspaceFolder::new(root.join(folder), false));

        let scoped = topology.scoped_folders(
            &topology.picked_gamemodes(Some("helix-hl2rp"), UnpickedGamemodes::None),
            &[],
        );

        assert_eq!(modules(&scoped), modules(&multi_root));
    }

    #[test]
    fn only_a_gmod_root_without_load_everything_is_scoped() {
        let root = install("scope");
        let gamemode = WorkspaceFolder::new(root.join("gamemodes/helix"), false);
        let whole = WorkspaceFolder::new(root.clone(), false);
        let everything = EmmyrcGmod {
            load_everything: true,
            ..EmmyrcGmod::default()
        };
        let scope = |folder: &WorkspaceFolder, gmod: &EmmyrcGmod| {
            scope_workspace_folder(folder, gmod, UnpickedGamemodes::None, &[])
        };

        assert_eq!(
            (
                scope(&gamemode, &EmmyrcGmod::default()),
                scope(&whole, &everything),
                scope(&whole, &EmmyrcGmod::default()).len(),
            ),
            (vec![gamemode.clone()], vec![whole.clone()], 2 + 5 + 1)
        );
    }

    #[test]
    fn the_registered_roots_do_not_depend_on_the_pick_or_open_files() {
        let root = install("roots");
        let topology = GmodTopology::discover(&root).expect("gmod root");
        let roots = |folders: Vec<WorkspaceFolder>| {
            folders
                .into_iter()
                .map(|folder| folder.root)
                .collect::<Vec<_>>()
        };

        let none = roots(topology.scoped_folders(&[], &[]));
        let picked = roots(topology.scoped_folders(
            &topology.picked_gamemodes(Some("*"), UnpickedGamemodes::None),
            &[root.join("lua/autorun/loose.lua")],
        ));

        assert_eq!(none, picked);
    }
}
