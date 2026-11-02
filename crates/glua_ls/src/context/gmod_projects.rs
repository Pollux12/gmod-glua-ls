//! A whole Garry's Mod folder's gamemodes and loose files loading while one
//! of their files is open, and the client told which addons and gamemodes
//! each such folder loads.

use std::collections::HashSet;
use std::path::PathBuf;

use glua_code_analysis::{
    ALL_CUSTOM_GAMEMODES, GmodTopology, NO_GAMEMODE, WorkspaceFolder, WorkspaceImport,
    collect_workspace_files, file_path_to_uri,
};
use lsp_types::Uri;
use serde::Serialize;

use super::{ClientProxy, LoadedConfig, ServerContextSnapshot, scope_folders};
use crate::handlers::apply_file_system_changes;

pub const PROJECTS_CHANGED: &str = "gluals/projectsChanged";

/// Scopes the workspace again for the documents now open, then loads what
/// that brings in and unloads what it leaves out, as file creates and
/// deletes. Returns the documents it unloaded.
pub async fn rescope_open_files(context: &ServerContextSnapshot) -> HashSet<Uri> {
    let scope_lock = context.workspace_manager().read().await.scope_lock();
    let _scoping = scope_lock.lock().await;
    let (folders, loaded, open, before) = {
        let manager = context.workspace_manager().read().await;
        let Some(loaded) = manager.loaded_config.clone() else {
            return HashSet::new();
        };
        (
            manager.workspace_folders.clone(),
            loaded,
            manager.open_file_paths(),
            manager.scoped_folders.clone(),
        )
    };
    let scoped = {
        let (folders, loaded, open) = (folders.clone(), loaded.clone(), open.clone());
        tokio::task::spawn_blocking(move || scope_folders(&folders, &loaded, &open))
            .await
            .unwrap_or_else(|err| std::panic::resume_unwind(err.into_panic()))
    };
    if scoped == before {
        return HashSet::new();
    }
    if !same_roots(&scoped, &before) {
        log::info!("a Garry's Mod folder's addons or gamemodes changed on disk; reloading");
        schedule_reload(context).await;
        return HashSet::new();
    }

    let changed = scoped
        .iter()
        .zip(&before)
        .filter(|(now, was)| now.import != was.import)
        .map(|(now, was)| (now.clone(), was.clone()))
        .collect::<Vec<_>>();
    let reconfigures = changed.iter().any(|(now, was)| {
        (now.import == WorkspaceImport::None || was.import == WorkspaceImport::None)
            && loaded.local_config_roots.contains(&now.root)
    });
    let local = context.analysis().read().await.local_file_paths();
    let unloaded = {
        let mut manager = context.workspace_manager().write().await;
        manager.scoped_folders = scoped.clone();
        local
            .iter()
            .filter(|(uri, path)| {
                changed.iter().any(|(now, _)| path.starts_with(&now.root))
                    && !manager.is_workspace_file(uri)
            })
            .map(|(uri, _)| uri.clone())
            .collect::<HashSet<_>>()
    };
    let known = local
        .into_iter()
        .map(|(_, path)| path)
        .chain(open)
        .collect::<HashSet<_>>();
    let loads = {
        let loaded = loaded.clone();
        tokio::task::spawn_blocking(move || added_files(&changed, &loaded, &known))
            .await
            .unwrap_or_else(|err| std::panic::resume_unwind(err.into_panic()))
    };

    let mut batch = loads;
    batch.extend(unloaded.iter().map(|uri| (uri.clone(), None)));
    log::info!(
        "Garry's Mod folder scope: {} files load, {} unload",
        batch.len() - unloaded.len(),
        unloaded.len()
    );
    apply_file_system_changes(context, batch).await;
    send_projects_changed(context.client(), folders, loaded, scoped).await;
    if reconfigures {
        log::info!("a Garry's Mod gamemode with its own config loaded or unloaded; reloading");
        schedule_reload(context).await;
    }
    unloaded
}

async fn schedule_reload(context: &ServerContextSnapshot) {
    let manager = context.workspace_manager().read().await;
    manager
        .schedule_config_reload(std::time::Duration::ZERO, context.workspace_manager_arc())
        .await;
}

fn same_roots(left: &[WorkspaceFolder], right: &[WorkspaceFolder]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.root == right.root)
}

/// The files each folder in `changed` now loads that it did not before, read
/// from disk, leaving out those already loaded or open.
fn added_files(
    changed: &[(WorkspaceFolder, WorkspaceFolder)],
    loaded: &LoadedConfig,
    known: &HashSet<PathBuf>,
) -> Vec<(Uri, Option<String>)> {
    changed
        .iter()
        .filter_map(|(now, was)| {
            let added = match (&now.import, &was.import) {
                (WorkspaceImport::All, WorkspaceImport::All) | (WorkspaceImport::None, _) => {
                    return None;
                }
                (WorkspaceImport::All, _) => now.clone(),
                (WorkspaceImport::SubPaths(now_paths), was_import) => {
                    let was_paths = match was_import {
                        WorkspaceImport::SubPaths(paths) => paths.as_slice(),
                        WorkspaceImport::All | WorkspaceImport::None => &[],
                    };
                    let added = now_paths
                        .iter()
                        .filter(|path| !was_paths.contains(path))
                        .cloned()
                        .collect::<Vec<_>>();
                    if added.is_empty() {
                        return None;
                    }
                    WorkspaceFolder::with_sub_paths(now.root.clone(), added, now.is_library)
                }
            };
            Some(collect_workspace_files(
                &vec![added],
                loaded.root_emmyrc(&now.root),
                None,
                None,
            ))
        })
        .flatten()
        .filter(|file| !known.contains(&PathBuf::from(&file.path)))
        .filter_map(|file| {
            let (path, text) = file.into_tuple();
            Some((file_path_to_uri(&path)?, text))
        })
        .collect()
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectsChanged {
    pub roots: Vec<GmodProject>,
}

/// A workspace folder that is a whole Garry's Mod folder.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GmodProject {
    pub folder_uri: Uri,
    pub root_uri: Uri,
    pub gamemodes: Vec<ProjectGamemode>,
    pub addons: Vec<Uri>,
    /// The configured `gmod.activeGamemode`.
    pub active_gamemode: Option<String>,
    pub load_everything: bool,
    /// The addon and gamemode folders loaded whole.
    pub loaded: Vec<Uri>,
    /// Several custom gamemodes and none picked.
    pub needs_pick: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectGamemode {
    pub name: String,
    pub uri: Uri,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derives_from: Option<String>,
    pub builtin: bool,
}

/// The Garry's Mod folders among the client's `folders` and what each loads.
pub fn projects(
    folders: &[WorkspaceFolder],
    loaded: &LoadedConfig,
    scoped: &[WorkspaceFolder],
) -> ProjectsChanged {
    let roots = folders
        .iter()
        .filter(|folder| !folder.is_library)
        .filter_map(|folder| {
            let topology = GmodTopology::discover(&folder.root)?;
            let gmod = &loaded.root_emmyrc(&folder.root).gmod;
            let units = topology
                .addons
                .iter()
                .chain(topology.gamemodes.iter().map(|gamemode| &gamemode.dir));
            let loaded_units = if gmod.load_everything {
                units.cloned().collect::<Vec<_>>()
            } else {
                units
                    .filter(|unit| {
                        scoped.iter().any(|scoped| {
                            &scoped.root == *unit && scoped.import == WorkspaceImport::All
                        })
                    })
                    .cloned()
                    .collect()
            };
            let picked = match gmod.active_gamemode.as_deref() {
                Some(ALL_CUSTOM_GAMEMODES | NO_GAMEMODE) => true,
                Some(name) => topology.gamemode(name).is_some(),
                None => false,
            };
            Some(GmodProject {
                folder_uri: file_path_to_uri(&folder.root)?,
                root_uri: file_path_to_uri(&topology.root)?,
                gamemodes: topology
                    .gamemodes
                    .iter()
                    .filter_map(|gamemode| {
                        Some(ProjectGamemode {
                            name: gamemode.name.clone(),
                            uri: file_path_to_uri(&gamemode.dir)?,
                            derives_from: gamemode.derives_from.clone(),
                            builtin: gamemode.builtin,
                        })
                    })
                    .collect(),
                addons: topology
                    .addons
                    .iter()
                    .filter_map(file_path_to_uri)
                    .collect(),
                active_gamemode: gmod.active_gamemode.clone(),
                load_everything: gmod.load_everything,
                loaded: loaded_units.iter().filter_map(file_path_to_uri).collect(),
                needs_pick: !gmod.load_everything
                    && !picked
                    && topology.custom_gamemodes().nth(1).is_some(),
            })
        })
        .collect();
    ProjectsChanged { roots }
}

/// Tells the client which addons and gamemodes each Garry's Mod folder loads.
pub async fn send_projects_changed(
    client: &ClientProxy,
    folders: Vec<WorkspaceFolder>,
    loaded: LoadedConfig,
    scoped: Vec<WorkspaceFolder>,
) {
    let projects = tokio::task::spawn_blocking(move || projects(&folders, &loaded, &scoped))
        .await
        .unwrap_or_else(|err| std::panic::resume_unwind(err.into_panic()));
    client.send_notification(PROJECTS_CHANGED, projects);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use glua_code_analysis::{WorkspaceFolder, file_path_to_uri};
    use lsp_server::Connection;
    use lsp_types::{
        ClientCapabilities, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
        TextDocumentIdentifier, TextDocumentItem, Uri,
    };
    use tokio_util::sync::CancellationToken;

    use super::projects;
    use crate::context::{ServerContext, ServerContextSnapshot, WorkspaceManager};
    use crate::handlers::{on_did_close_document, on_did_open_text_document};

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("create folder");
        fs::write(path, text).expect("write file");
    }

    fn gamemode(root: &Path, name: &str, base: &str) {
        write(
            root,
            &format!("gamemodes/{name}/{name}.txt"),
            &format!("\"{name}\"\n{{\n\t\"base\"\t\"{base}\"\n}}\n"),
        );
    }

    /// A `garrysmod` folder with one addon, the built-ins and the custom
    /// gamemodes `alpha`, `beta` deriving from it and `gamma`, with
    /// `config` as its `.gluarc.json`.
    fn install(name: &str, config: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be valid")
            .as_nanos();
        let root = std::env::temp_dir()
            .join(format!("glua_ls_gmod_projects_{name}_{unique}"))
            .join("garrysmod");
        write(
            &root,
            "addons/tools/lua/autorun/sh_tools.lua",
            "TOOLS = {}\n",
        );
        gamemode(&root, "base", "");
        gamemode(&root, "sandbox", "base");
        gamemode(&root, "alpha", "sandbox");
        gamemode(&root, "beta", "alpha");
        gamemode(&root, "gamma", "sandbox");
        write(
            &root,
            "gamemodes/alpha/gamemode/shared.lua",
            "ALPHA = {}\nfunction ALPHA.Get() return 1 end\n",
        );
        write(
            &root,
            "gamemodes/beta/gamemode/init.lua",
            "local unused_value = ALPHA.Get()\nlocal tools = TOOLS\nreturn tools\n",
        );
        write(&root, "gamemodes/gamma/gamemode/init.lua", "GAMMA = 1\n");
        write(
            &root,
            "gamemodes/sandbox/gamemode/init.lua",
            "SANDBOX = 1\n",
        );
        write(&root, "lua/autorun/sh_loose.lua", "local loose = TOOLS\n");
        write(&root, ".gluarc.json", config);
        root
    }

    fn uri(root: &Path, relative: &str) -> Uri {
        file_path_to_uri(&root.join(relative)).expect("file URI should parse")
    }

    fn start(runtime: &tokio::runtime::Runtime, root: &Path) -> (ServerContext, Connection) {
        let (connection, peer) = Connection::memory();
        let context = runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            snapshot.workspace_manager().write().await.workspace_folders =
                vec![WorkspaceFolder::new(root.to_path_buf(), false)];
            WorkspaceManager::reload_config(snapshot.workspace_manager_arc()).await;
            context
        });
        (context, peer)
    }

    /// Opens (`true`) or closes `relative` as the client's notifications do and
    /// waits for the analysis to settle.
    fn toggle(
        runtime: &tokio::runtime::Runtime,
        snapshot: &ServerContextSnapshot,
        root: &Path,
        relative: &str,
        open: bool,
    ) {
        let uri = uri(root, relative);
        runtime.block_on(async {
            {
                let mut manager = snapshot.workspace_manager().write().await;
                if open {
                    manager.current_open_files.insert(uri.clone());
                } else {
                    manager.current_open_files.remove(&uri);
                }
            }
            if open {
                let session = snapshot.note_document_opened(&uri, 1);
                let text = fs::read_to_string(root.join(relative)).expect("read file");
                on_did_open_text_document(
                    snapshot.clone(),
                    DidOpenTextDocumentParams {
                        text_document: TextDocumentItem::new(uri, "lua".to_string(), 1, text),
                    },
                    session,
                )
                .await;
            } else {
                let session = snapshot.mark_document_closed(&uri);
                on_did_close_document(
                    snapshot.clone(),
                    DidCloseTextDocumentParams {
                        text_document: TextDocumentIdentifier::new(uri),
                    },
                    session,
                )
                .await;
            }
            snapshot
                .debounced_analysis()
                .wait_until_fresh_for(&CancellationToken::new(), "test")
                .await;
        });
    }

    /// Every loaded file outside std with its diagnostics, by path relative
    /// to `root`.
    fn loaded(
        runtime: &tokio::runtime::Runtime,
        snapshot: &ServerContextSnapshot,
        root: &Path,
    ) -> BTreeMap<String, Vec<String>> {
        runtime.block_on(async {
            let analysis = snapshot.analysis().read().await;
            analysis
                .local_file_paths()
                .into_iter()
                .filter_map(|(uri, path)| {
                    let relative = path.strip_prefix(root).ok()?;
                    let file_id = analysis.get_file_id(&uri)?;
                    let mut diagnostics = analysis
                        .diagnose_file(file_id, CancellationToken::new())
                        .unwrap_or_default()
                        .into_iter()
                        .map(|diagnostic| format!("{:?} {}", diagnostic.code, diagnostic.message))
                        .collect::<Vec<_>>();
                    diagnostics.sort();
                    Some((relative.to_string_lossy().replace('\\', "/"), diagnostics))
                })
                .collect()
        })
    }

    fn paths(loaded: &BTreeMap<String, Vec<String>>) -> Vec<&str> {
        loaded.keys().map(String::as_str).collect()
    }

    #[test]
    fn a_gamemode_loads_with_its_chain_while_one_of_its_files_is_open() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let root = install("open", "{}");
        let (context, _peer) = start(&runtime, &root);
        let snapshot = context.snapshot();
        let before = loaded(&runtime, &snapshot, &root);

        toggle(
            &runtime,
            &snapshot,
            &root,
            "gamemodes/beta/gamemode/init.lua",
            true,
        );
        let opened = loaded(&runtime, &snapshot, &root);
        toggle(
            &runtime,
            &snapshot,
            &root,
            "gamemodes/beta/gamemode/init.lua",
            false,
        );
        let closed = loaded(&runtime, &snapshot, &root);

        assert_eq!(
            (paths(&before), paths(&opened), &closed),
            (
                vec!["addons/tools/lua/autorun/sh_tools.lua"],
                vec![
                    "addons/tools/lua/autorun/sh_tools.lua",
                    "gamemodes/alpha/gamemode/shared.lua",
                    "gamemodes/beta/gamemode/init.lua",
                ],
                &before
            )
        );
        let _ = fs::remove_dir_all(root.parent().expect("temp dir"));
    }

    #[test]
    fn an_opened_gamemode_settles_as_a_cold_load_that_picks_it() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let opened_root = install("opened", "{}");
        let picked_root = install("picked", r#"{ "gmod": { "activeGamemode": "beta" } }"#);
        let (opened_context, _opened_peer) = start(&runtime, &opened_root);
        let (picked_context, _picked_peer) = start(&runtime, &picked_root);
        toggle(
            &runtime,
            &opened_context.snapshot(),
            &opened_root,
            "gamemodes/beta/gamemode/init.lua",
            true,
        );

        assert_eq!(
            loaded(&runtime, &opened_context.snapshot(), &opened_root),
            loaded(&runtime, &picked_context.snapshot(), &picked_root)
        );
        let _ = fs::remove_dir_all(opened_root.parent().expect("temp dir"));
        let _ = fs::remove_dir_all(picked_root.parent().expect("temp dir"));
    }

    #[test]
    fn a_loose_file_loads_alone_while_it_is_open() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let root = install("loose", "{}");
        let (context, _peer) = start(&runtime, &root);
        let snapshot = context.snapshot();

        toggle(&runtime, &snapshot, &root, "lua/autorun/sh_loose.lua", true);
        let opened = loaded(&runtime, &snapshot, &root);
        toggle(
            &runtime,
            &snapshot,
            &root,
            "lua/autorun/sh_loose.lua",
            false,
        );
        let closed = loaded(&runtime, &snapshot, &root);

        assert_eq!(
            (paths(&opened), paths(&closed)),
            (
                vec![
                    "addons/tools/lua/autorun/sh_tools.lua",
                    "lua/autorun/sh_loose.lua"
                ],
                vec!["addons/tools/lua/autorun/sh_tools.lua"]
            )
        );
        let _ = fs::remove_dir_all(root.parent().expect("temp dir"));
    }

    #[test]
    fn the_client_learns_what_loads_and_whether_a_gamemode_needs_picking() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let unpicked = install("unpicked", "{}");
        let picked = install("projects", r#"{ "gmod": { "activeGamemode": "beta" } }"#);
        let addons_only = install("addons_only", r#"{ "gmod": { "activeGamemode": "" } }"#);
        let all_custom = install("all_custom", r#"{ "gmod": { "activeGamemode": "*" } }"#);
        let everything = install("everything", r#"{ "gmod": { "loadEverything": true } }"#);
        let summary = |root: &Path| {
            let (context, _peer) = start(&runtime, root);
            runtime.block_on(async {
                let snapshot = context.snapshot();
                let manager = snapshot.workspace_manager().read().await;
                let project = projects(
                    &manager.workspace_folders,
                    manager.loaded_config.as_ref().expect("loaded config"),
                    &manager.scoped_folders,
                )
                .roots
                .into_iter()
                .next()
                .expect("a Garry's Mod folder");
                (
                    project.needs_pick,
                    project.loaded.len(),
                    project.gamemodes.len(),
                )
            })
        };

        assert_eq!(
            [
                summary(&unpicked),
                summary(&picked),
                summary(&addons_only),
                summary(&all_custom),
                summary(&everything),
            ],
            [
                (true, 1, 5),
                (false, 3, 5),
                (false, 1, 5),
                (false, 4, 5),
                (false, 6, 5),
            ]
        );
        for root in [unpicked, picked, addons_only, all_custom, everything] {
            let _ = fs::remove_dir_all(root.parent().expect("temp dir"));
        }
    }

    /// Waits until `root`'s loaded files, as [`loaded`] lists them, include
    /// `relative` (or leave it out, when not `present`), as a scheduled
    /// reload settles.
    fn wait_until_loaded(
        runtime: &tokio::runtime::Runtime,
        snapshot: &ServerContextSnapshot,
        root: &Path,
        relative: &str,
        present: bool,
    ) -> BTreeMap<String, Vec<String>> {
        for _ in 0..300 {
            let loaded = loaded(runtime, snapshot, root);
            if loaded.contains_key(relative) == present {
                return loaded;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("{relative} never became loaded: {present}");
    }

    #[test]
    fn an_addon_created_on_disk_loads() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let root = install("new_addon", "{}");
        let (context, _peer) = start(&runtime, &root);
        let snapshot = context.snapshot();
        write(
            &root,
            "addons/extra/lua/autorun/sh_extra.lua",
            "EXTRA = 1\n",
        );

        runtime.block_on(crate::handlers::on_did_change_watched_files(
            snapshot.clone(),
            lsp_types::DidChangeWatchedFilesParams {
                changes: vec![lsp_types::FileEvent {
                    uri: uri(&root, "addons/extra"),
                    typ: lsp_types::FileChangeType::CREATED,
                }],
            },
        ));

        assert_eq!(
            paths(&wait_until_loaded(
                &runtime,
                &snapshot,
                &root,
                "addons/extra/lua/autorun/sh_extra.lua",
                true
            )),
            vec![
                "addons/extra/lua/autorun/sh_extra.lua",
                "addons/tools/lua/autorun/sh_tools.lua",
            ]
        );
        let _ = fs::remove_dir_all(root.parent().expect("temp dir"));
    }

    #[test]
    fn a_gamemode_with_its_own_config_loads_and_keeps_its_open_document() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let root = install("gamemode_config", "{}");
        write(&root, "gamemodes/gamma/.gluarc.json", "{}");
        write(
            &root,
            "gamemodes/gamma/gamemode/shared.lua",
            "GAMMA_SHARED = 1\n",
        );
        let (context, _peer) = start(&runtime, &root);
        let snapshot = context.snapshot();
        let before = loaded(&runtime, &snapshot, &root);

        toggle(
            &runtime,
            &snapshot,
            &root,
            "gamemodes/gamma/gamemode/init.lua",
            true,
        );
        let opened = loaded(&runtime, &snapshot, &root);
        let edits_reach_the_server = snapshot
            .note_document_seen_version(&uri(&root, "gamemodes/gamma/gamemode/init.lua"), 2)
            .is_some();
        toggle(
            &runtime,
            &snapshot,
            &root,
            "gamemodes/gamma/gamemode/init.lua",
            false,
        );
        let closed = loaded(&runtime, &snapshot, &root);

        assert_eq!(
            (paths(&opened), edits_reach_the_server, &closed),
            (
                vec![
                    "addons/tools/lua/autorun/sh_tools.lua",
                    "gamemodes/gamma/gamemode/init.lua",
                    "gamemodes/gamma/gamemode/shared.lua",
                ],
                true,
                &before
            )
        );
        let _ = fs::remove_dir_all(root.parent().expect("temp dir"));
    }

    #[test]
    fn a_gamemode_library_loads_only_with_its_gamemode() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let root = install("gamemode_library", "{}");
        write(&root, "gamma_lib/sh_gamma_lib.lua", "GAMMA_LIB = 1\n");
        let library = root.join("gamma_lib").to_string_lossy().replace('\\', "/");
        write(
            &root,
            "gamemodes/gamma/.gluarc.json",
            &format!(r#"{{ "workspace": {{ "library": ["{library}"] }} }}"#),
        );
        let (context, _peer) = start(&runtime, &root);
        let snapshot = context.snapshot();
        let before = loaded(&runtime, &snapshot, &root);

        toggle(
            &runtime,
            &snapshot,
            &root,
            "gamemodes/gamma/gamemode/init.lua",
            true,
        );
        let opened = wait_until_loaded(
            &runtime,
            &snapshot,
            &root,
            "gamma_lib/sh_gamma_lib.lua",
            true,
        );
        toggle(
            &runtime,
            &snapshot,
            &root,
            "gamemodes/gamma/gamemode/init.lua",
            false,
        );
        let closed = wait_until_loaded(
            &runtime,
            &snapshot,
            &root,
            "gamma_lib/sh_gamma_lib.lua",
            false,
        );

        assert_eq!(
            (paths(&before), paths(&opened), &closed),
            (
                vec!["addons/tools/lua/autorun/sh_tools.lua"],
                vec![
                    "addons/tools/lua/autorun/sh_tools.lua",
                    "gamemodes/gamma/gamemode/init.lua",
                    "gamma_lib/sh_gamma_lib.lua",
                ],
                &before
            )
        );
        let _ = fs::remove_dir_all(root.parent().expect("temp dir"));
    }
}
