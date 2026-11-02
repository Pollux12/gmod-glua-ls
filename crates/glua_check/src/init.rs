use fern::Dispatch;
use glua_code_analysis::{
    ConfigSources, EmmyLibraryItem, EmmyLuaAnalysis, Emmyrc, GmodTopology, UnpickedGamemodes,
    WorkspaceConfigs, WorkspaceFolder, collect_workspace_files, detect_gamemode_base_libraries,
    load_configs, load_scoped_workspace_configs, scope_folders, update_code_style,
    workspace_collection_groups,
};
use log::LevelFilter;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn root_from_configs(config_paths: &[PathBuf], fallback: &Path) -> PathBuf {
    if config_paths.len() != 1 {
        fallback.to_path_buf()
    } else {
        let config_path = &config_paths[0];
        // Need to convert to canonical path to ensure parent() is not an empty
        // string in the case the path is a relative basename.
        match config_path.canonicalize() {
            Ok(path) => path.parent().unwrap().to_path_buf(),
            Err(err) => {
                log::error!(
                    "Failed to canonicalize config path: \"{:?}\": {}",
                    config_path,
                    err
                );
                fallback.to_path_buf()
            }
        }
    }
}

pub fn setup_logger(verbose: bool) {
    let logger = Dispatch::new()
        .format(move |out, message, record| {
            let (color, reset) = match record.level() {
                log::Level::Error => ("\x1b[31m", "\x1b[0m"), // Red
                log::Level::Warn => ("\x1b[33m", "\x1b[0m"),  // Yellow
                log::Level::Info | log::Level::Debug | log::Level::Trace => ("", ""),
            };
            out.finish(format_args!(
                "{}{}: {}{}",
                color,
                record.level(),
                if verbose {
                    format!("({}) {}", record.target(), message)
                } else {
                    message.to_string()
                },
                reset
            ))
        })
        .level(if verbose {
            LevelFilter::Info
        } else {
            LevelFilter::Warn
        })
        .chain(std::io::stderr());

    if let Err(e) = logger.apply() {
        eprintln!("Failed to apply logger: {:?}", e);
    }
}

pub async fn load_workspace(
    main_path: PathBuf,
    cmd_workspace_folders: Vec<PathBuf>,
    config_paths: Option<Vec<PathBuf>>,
    ignore: Option<Vec<String>>,
    gmod_annotations: Option<PathBuf>,
) -> Option<EmmyLuaAnalysis> {
    let annotations = gmod_annotations.and_then(|annotations_path| {
        if annotations_path.is_dir() {
            log::info!(
                "Adding GMod annotations from: {}",
                annotations_path.display()
            );
            Some(annotations_path.to_string_lossy().into_owned())
        } else {
            log::warn!(
                "GMod annotations path is not an existing directory: {}",
                annotations_path.display()
            );
            None
        }
    });
    let prepare = |emmyrc: &mut Emmyrc| {
        if let Some(annotations) = &annotations {
            emmyrc.prioritize_gmod_annotations_library(annotations.clone());
        }
    };
    let folders = cmd_workspace_folders
        .iter()
        .map(|root| WorkspaceFolder::new(root.clone(), false))
        .collect::<Vec<_>>();

    // Loaded as the server loads them, so each folder, addon and gamemode
    // reads its own config, and load order and library precedence match.
    let (workspace_folders, configs) = match config_paths {
        Some(config_paths) => {
            let config_root = root_from_configs(&config_paths, &main_path);
            let mut emmyrc = load_configs(config_paths, None);
            log::info!(
                "Pre processing configurations using root: \"{}\"",
                config_root.display()
            );
            apply_gamemode_base_detection(&mut emmyrc, &cmd_workspace_folders, &main_path);
            emmyrc.pre_process_emmyrc(&config_root);
            prepare(&mut emmyrc);
            let configs = WorkspaceConfigs {
                emmyrc: emmyrc.into(),
                ..WorkspaceConfigs::default()
            };
            let scoped = scope_folders(&folders, &[], UnpickedGamemodes::AllCustom, |_| {
                configs.emmyrc.as_ref()
            });
            (scoped, configs)
        }
        None => load_scoped_workspace_configs(
            &folders,
            &[],
            UnpickedGamemodes::AllCustom,
            &ConfigSources {
                global_files: Vec::new(),
                partial_emmyrcs: None,
                prepare: &prepare,
            },
        ),
    };

    let mut analysis = EmmyLuaAnalysis::new();
    analysis.update_config(configs.emmyrc.clone());
    analysis.init_std_lib();

    let groups =
        workspace_collection_groups(workspace_folders, &configs.emmyrc, &configs.root_emmyrcs);
    analysis.set_workspace_roots(groups.iter().flat_map(|(_, group)| group));
    analysis.set_root_diagnostic_configs(&configs.diagnostic_configs);

    let file_infos = groups
        .iter()
        .flat_map(|(config, group)| collect_workspace_files(group, config, None, ignore.clone()));
    let mut loaded_paths = HashSet::new();
    let files = file_infos
        .filter(|file| loaded_paths.insert(file.path.clone()))
        .filter_map(|file| {
            if file.path.ends_with(".editorconfig") {
                let file_path = PathBuf::from(file.path);
                let parent_dir = file_path
                    .parent()
                    .unwrap()
                    .to_path_buf()
                    .to_string_lossy()
                    .to_string()
                    .replace("\\", "/");
                let file_normalized = file_path.to_string_lossy().to_string().replace("\\", "/");
                update_code_style(&parent_dir, &file_normalized);
                None
            } else {
                Some(file.into_tuple())
            }
        })
        .collect();
    analysis.update_files_by_path(files);

    if analysis.check_schema_update() {
        analysis.update_schema().await;
    }

    Some(analysis)
}

/// Auto-detect GMod gamemode base libraries from `<gamemodes>/<name>/<name>.txt`
/// unless explicitly disabled via `gmod.autoDetectGamemodeBase = false`.
///
/// Detection roots are the actual workspace folders (with `main_path` as a
/// sensible fallback when none were given), except a whole Garry's Mod folder
/// that is scoped to its addons and gamemodes, whose built-in gamemodes the
/// annotations cover. `config_root` is intentionally
/// NOT used as a detection root: when `--config` points outside the project
/// it would scan the wrong directory.
///
/// Detected paths are appended to `emmyrc.workspace.library` as
/// `EmmyLibraryItem::Path`, deduped against existing entries via canonical
/// path comparison so relative-vs-absolute equivalents collapse.
pub(crate) fn apply_gamemode_base_detection(
    emmyrc: &mut Emmyrc,
    workspace_folders: &[PathBuf],
    main_path: &Path,
) {
    if matches!(emmyrc.gmod.auto_detect_gamemode_base, Some(false)) {
        return;
    }

    let raw_roots: Vec<PathBuf> = if workspace_folders.is_empty() {
        vec![main_path.to_path_buf()]
    } else {
        workspace_folders.to_vec()
    };

    // Normalize: file path -> parent dir; dedupe by canonical path.
    let mut seen_canon: HashSet<PathBuf> = HashSet::new();
    let mut normalized: Vec<PathBuf> = Vec::new();
    let scoped = |dir: &Path| !emmyrc.gmod.load_everything && GmodTopology::discover(dir).is_some();
    for root in raw_roots {
        let dir = if root.is_file() {
            match root.parent() {
                Some(p) => p.to_path_buf(),
                None => continue,
            }
        } else {
            root
        };
        if scoped(&dir) {
            continue;
        }
        let key = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if seen_canon.insert(key) {
            normalized.push(dir);
        }
    }

    for root in &normalized {
        for detected in detect_gamemode_base_libraries(root) {
            let detected_str = detected.to_string_lossy().into_owned();
            let detected_canon = detected.canonicalize().unwrap_or_else(|_| detected.clone());
            let already_present = emmyrc.workspace.library.iter().any(|item| {
                let p = PathBuf::from(item.get_path());
                if p == detected || item.get_path() == &detected_str {
                    return true;
                }
                // canonical equivalence catches relative-vs-absolute cases.
                p.canonicalize()
                    .map(|c| c == detected_canon)
                    .unwrap_or(false)
            });
            if already_present {
                continue;
            }
            log::info!(
                "Auto-detected gamemode base library: {}",
                detected.display()
            );
            emmyrc
                .workspace
                .library
                .push(EmmyLibraryItem::Path(detected_str));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static WORKSPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_workspace() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let n = WORKSPACE_COUNTER.fetch_add(1, Ordering::Relaxed);
        dir.push(format!(
            "glua_check_gmbase_{}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            n
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_gamemode(root: &Path, name: &str, base: Option<&str>) {
        let folder = root.join("gamemodes").join(name);
        fs::create_dir_all(&folder).unwrap();
        let body = match base {
            Some(b) => format!("\"{name}\"\n{{\n\t\"base\"\t\"{b}\"\n}}\n"),
            None => format!("\"{name}\"\n{{\n\t\"title\"\t\"{name}\"\n}}\n"),
        };
        fs::write(folder.join(format!("{name}.txt")), body).unwrap();
    }

    #[test]
    fn auto_detects_chain_from_workspace_folder() {
        let root = make_workspace();
        write_gamemode(&root, "darkrp", Some("framework"));
        write_gamemode(&root, "framework", Some("ancestor"));
        write_gamemode(&root, "ancestor", None);

        let mut emmyrc = Emmyrc::default();
        apply_gamemode_base_detection(&mut emmyrc, std::slice::from_ref(&root), &root);

        let lib_paths: Vec<String> = emmyrc
            .workspace
            .library
            .iter()
            .map(|i| i.get_path().clone())
            .collect();
        assert_eq!(
            lib_paths.len(),
            2,
            "expected framework + ancestor, got {lib_paths:?}"
        );
        assert!(lib_paths.iter().any(|p| p.ends_with("framework")));
        assert!(lib_paths.iter().any(|p| p.ends_with("ancestor")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn skips_detection_when_disabled() {
        let root = make_workspace();
        write_gamemode(&root, "darkrp", Some("framework"));
        write_gamemode(&root, "framework", None);

        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.auto_detect_gamemode_base = Some(false);
        apply_gamemode_base_detection(&mut emmyrc, std::slice::from_ref(&root), &root);

        assert!(
            emmyrc.workspace.library.is_empty(),
            "detection must be a no-op when disabled"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn falls_back_to_main_path_when_no_workspaces() {
        let root = make_workspace();
        write_gamemode(&root, "darkrp", Some("framework"));
        write_gamemode(&root, "framework", None);

        let mut emmyrc = Emmyrc::default();
        apply_gamemode_base_detection(&mut emmyrc, &[], &root);

        assert_eq!(emmyrc.workspace.library.len(), 1);
        assert!(
            emmyrc.workspace.library[0]
                .get_path()
                .ends_with("framework")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn handles_file_workspace_path() {
        let root = make_workspace();
        write_gamemode(&root, "darkrp", Some("framework"));
        write_gamemode(&root, "framework", None);
        let some_file = root.join("init.lua");
        fs::write(&some_file, "-- placeholder\n").unwrap();

        let mut emmyrc = Emmyrc::default();
        apply_gamemode_base_detection(&mut emmyrc, &[some_file], &root);

        assert_eq!(
            emmyrc.workspace.library.len(),
            1,
            "file-path workspace must be normalized to its parent dir"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dedupes_against_existing_library_entry() {
        let root = make_workspace();
        write_gamemode(&root, "darkrp", Some("framework"));
        write_gamemode(&root, "framework", None);
        let framework_path = root.join("gamemodes").join("framework");

        let mut emmyrc = Emmyrc::default();
        emmyrc.workspace.library.push(EmmyLibraryItem::Path(
            framework_path.to_string_lossy().into(),
        ));
        apply_gamemode_base_detection(&mut emmyrc, std::slice::from_ref(&root), &root);

        assert_eq!(
            emmyrc.workspace.library.len(),
            1,
            "must not push a duplicate library entry, got {:?}",
            emmyrc.workspace.library
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn loads_gmod_annotations_before_configured_libraries() {
        let root = make_workspace();
        let annotations = make_workspace();
        let library = make_workspace();
        let config_path = root.join(".gluarc.json");
        fs::write(
            &config_path,
            format!(
                r#"{{"workspace":{{"library":["{}"]}}}}"#,
                library.to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();

        let analysis = load_workspace(
            root.clone(),
            vec![root.clone()],
            Some(vec![config_path]),
            None,
            Some(annotations.clone()),
        )
        .await
        .expect("workspace should load");

        let annotations_id = analysis
            .get_workspace_id_for_root(&annotations)
            .expect("annotations workspace should be registered");
        let library_id = analysis
            .get_workspace_id_for_root(&library)
            .expect("configured library workspace should be registered");
        assert!(annotations_id.id < library_id.id);
        assert_eq!(
            analysis.emmyrc.gmod.annotations_path.as_deref(),
            Some(annotations.to_string_lossy().as_ref())
        );
        assert_eq!(
            analysis.emmyrc.workspace.library[0].get_path(),
            annotations.to_string_lossy().as_ref()
        );

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(annotations);
        let _ = fs::remove_dir_all(library);
    }

    /// A package directory loads as the server loads it.
    #[tokio::test]
    async fn loads_configured_package_dirs() {
        let root = make_workspace();
        let packages = make_workspace();
        let package = packages.join("mypackage");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("init.lua"),
            "PackageGlobal = 1
",
        )
        .unwrap();
        let config_path = root.join(".gluarc.json");
        fs::write(
            &config_path,
            format!(
                r#"{{"workspace":{{"packageDirs":["{}"]}}}}"#,
                package.to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();

        let analysis = load_workspace(
            root.clone(),
            vec![root.clone()],
            Some(vec![config_path]),
            None,
            None,
        )
        .await
        .expect("workspace should load");

        let uri = glua_code_analysis::file_path_to_uri(&package.join("init.lua"))
            .expect("URI should parse");
        assert!(analysis.get_file_id(&uri).is_some());

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(packages);
    }

    /// Each addon of a whole Garry's Mod folder is collected with its own
    /// config, as the server collects it.
    #[tokio::test]
    async fn collects_each_addon_of_a_gmod_folder_with_its_own_config() {
        let root = make_workspace();
        let file = |addon: &str| root.join(format!("addons/{addon}/lua/extras/sh_extra.lua"));
        for addon in ["configured", "plain"] {
            fs::create_dir_all(file(addon).parent().unwrap()).unwrap();
            fs::write(file(addon), "EXTRA = {}\n").unwrap();
        }
        fs::create_dir_all(root.join("gamemodes")).unwrap();
        fs::write(
            root.join("addons/configured/.gluarc.json"),
            r#"{"workspace":{"ignoreDir":["lua/extras/"]}}"#,
        )
        .unwrap();

        let analysis = load_workspace(root.clone(), vec![root.clone()], None, None, None)
            .await
            .expect("workspace should load");
        let loaded = |addon: &str| {
            let uri = glua_code_analysis::file_path_to_uri(&file(addon)).expect("URI should parse");
            analysis.get_file_id(&uri).is_some()
        };

        assert_eq!((loaded("configured"), loaded("plain")), (false, true));
        let _ = fs::remove_dir_all(root);
    }
}
