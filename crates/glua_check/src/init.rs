use fern::Dispatch;
use glua_code_analysis::{
    EmmyLibraryItem, EmmyLuaAnalysis, Emmyrc, WorkspaceFolder, collect_workspace_files,
    detect_gamemode_base_libraries, load_configs, update_code_style,
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
    let (config_files, config_root): (Vec<PathBuf>, PathBuf) =
        if let Some(config_paths) = config_paths {
            (
                config_paths.clone(),
                root_from_configs(&config_paths, &main_path),
            )
        } else {
            (
                discover_config_files_in_order(&main_path),
                main_path.clone(),
            )
        };

    let mut emmyrc = load_configs(config_files, None);
    log::info!(
        "Pre processing configurations using root: \"{}\"",
        config_root.display()
    );

    apply_gamemode_base_detection(&mut emmyrc, &cmd_workspace_folders, &main_path);

    emmyrc.pre_process_emmyrc(&config_root);

    let mut workspace_folders = cmd_workspace_folders
        .iter()
        .map(|path| WorkspaceFolder::new(path.clone(), false))
        .collect::<Vec<WorkspaceFolder>>();
    let mut analysis = EmmyLuaAnalysis::new();
    analysis.update_config(emmyrc.clone().into());
    analysis.init_std_lib(None);

    // Add GMod annotations as library workspace if provided
    if let Some(annotations_path) = gmod_annotations {
        if annotations_path.exists() {
            log::info!(
                "Adding GMod annotations from: {}",
                annotations_path.display()
            );
            analysis.add_library_workspace(annotations_path.clone());
            workspace_folders.push(WorkspaceFolder::new(annotations_path, true));
        } else {
            log::warn!(
                "GMod annotations path does not exist: {}",
                annotations_path.display()
            );
        }
    }

    for lib in &emmyrc.workspace.library {
        let path = PathBuf::from(lib.get_path().clone());
        analysis.add_library_workspace(path.clone());
        workspace_folders.push(WorkspaceFolder::new(path.clone(), true));
    }

    for path in &workspace_folders {
        if path.is_library {
            continue;
        }
        analysis.add_main_workspace(path.root.clone());
    }

    for root in &emmyrc.workspace.workspace_roots {
        analysis.add_main_workspace(PathBuf::from(root));
    }

    let file_infos = collect_workspace_files(&workspace_folders, &analysis.emmyrc, None, ignore);
    let files = file_infos
        .into_iter()
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

fn discover_config_files_in_order(root: &Path) -> Vec<PathBuf> {
    // .gluarc.json is the GMod-specific config — if present, it takes exclusive priority.
    let gluarc = root.join(".gluarc.json");
    if gluarc.exists() {
        return vec![gluarc];
    }
    [
        root.join(".luarc.json"),
        root.join(".emmyrc.json"),
        root.join(".emmyrc.lua"),
    ]
    .into_iter()
    .filter(|path| path.exists())
    .collect()
}

/// Auto-detect GMod gamemode base libraries from `<gamemodes>/<name>/<name>.txt`
/// unless explicitly disabled via `gmod.autoDetectGamemodeBase = false`.
///
/// Detection roots are the actual workspace folders (with `main_path` as a
/// sensible fallback when none were given). `config_root` is intentionally
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
    for root in raw_roots {
        let dir = if root.is_file() {
            match root.parent() {
                Some(p) => p.to_path_buf(),
                None => continue,
            }
        } else {
            root
        };
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

    fn make_workspace() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "glua_check_gmbase_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
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
        write_gamemode(&root, "darkrp", Some("sandbox"));
        write_gamemode(&root, "sandbox", Some("base"));
        write_gamemode(&root, "base", None);

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
            "expected sandbox + base, got {lib_paths:?}"
        );
        assert!(lib_paths.iter().any(|p| p.ends_with("sandbox")));
        assert!(lib_paths.iter().any(|p| p.ends_with("base")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn skips_detection_when_disabled() {
        let root = make_workspace();
        write_gamemode(&root, "darkrp", Some("sandbox"));
        write_gamemode(&root, "sandbox", None);

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
        write_gamemode(&root, "darkrp", Some("sandbox"));
        write_gamemode(&root, "sandbox", None);

        let mut emmyrc = Emmyrc::default();
        apply_gamemode_base_detection(&mut emmyrc, &[], &root);

        assert_eq!(emmyrc.workspace.library.len(), 1);
        assert!(emmyrc.workspace.library[0].get_path().ends_with("sandbox"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn handles_file_workspace_path() {
        let root = make_workspace();
        write_gamemode(&root, "darkrp", Some("sandbox"));
        write_gamemode(&root, "sandbox", None);
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
        write_gamemode(&root, "darkrp", Some("sandbox"));
        write_gamemode(&root, "sandbox", None);
        let sandbox_path = root.join("gamemodes").join("sandbox");

        let mut emmyrc = Emmyrc::default();
        emmyrc
            .workspace
            .library
            .push(EmmyLibraryItem::Path(sandbox_path.to_string_lossy().into()));
        apply_gamemode_base_detection(&mut emmyrc, std::slice::from_ref(&root), &root);

        assert_eq!(
            emmyrc.workspace.library.len(),
            1,
            "must not push a duplicate library entry, got {:?}",
            emmyrc.workspace.library
        );
        let _ = fs::remove_dir_all(root);
    }
}
