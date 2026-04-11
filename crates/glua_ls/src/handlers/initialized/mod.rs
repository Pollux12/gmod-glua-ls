mod client_config;
mod codestyle;
mod locale;
mod std_i18n;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use crate::{
    cmd_args::CmdArgs,
    context::{
        FileDiagnostic, LspFeatures, ProgressTask, ServerContextSnapshot, StatusBar,
        WorkspaceFileMatcher, get_client_id, load_emmy_config,
    },
    handlers::{
        initialized::std_i18n::try_generate_translated_std, text_document::register_files_watch,
    },
    logger::init_logger,
};
pub use client_config::{ClientConfig, get_client_config};
use codestyle::load_editorconfig;
use glua_code_analysis::{
    EmmyLuaAnalysis, Emmyrc, LuaDiagnosticConfig, WorkspaceFolder, calculate_include_and_exclude,
    collect_workspace_files, fetch_schema_urls, uri_to_file_path,
};
use lsp_types::InitializeParams;
use tokio::sync::RwLock;

pub async fn initialized_handler(
    context: ServerContextSnapshot,
    params: InitializeParams,
    cmd_args: CmdArgs,
) -> Option<()> {
    // init locale
    locale::set_ls_locale(&params);
    let workspace_folders = get_workspace_folders(&params);
    let main_root: Option<&str> = match workspace_folders.first() {
        Some(path) => path.root.to_str(),
        None => None,
    };

    // init logger
    init_logger(main_root, &cmd_args);
    log::info!("main root: {:?}", main_root);

    let client_id = if let Some(editor) = &cmd_args.editor {
        editor.clone().into()
    } else {
        get_client_id(&params.client_info)
    };
    let supports_config_request = params
        .capabilities
        .workspace
        .as_ref()?
        .configuration
        .unwrap_or_default();
    log::info!("client_id: {:?}", client_id);

    {
        log::info!("set workspace folders: {:?}", workspace_folders);
        let mut workspace_manager = context.workspace_manager().write().await;
        workspace_manager.workspace_folders = workspace_folders.clone();
        log::info!("workspace folders set");
    }

    let client_config = get_client_config(&context, client_id, supports_config_request).await;

    // Extract gmodAnnotationsPath from initialization options if provided
    // CLI argument takes precedence over VSCode extension-provided path
    let mut client_config = client_config;
    if let Some(ref init_options) = params.initialization_options {
        if let Some(gmod_path) = init_options.get("gmodAnnotationsPath") {
            if let Some(path_str) = gmod_path.as_str() {
                log::info!("Received gmodAnnotationsPath from VSCode: {}", path_str);
                // Only use VSCode path if CLI didn't provide one
                if client_config.gmod_annotations_path.is_none() {
                    client_config.gmod_annotations_path = Some(path_str.to_string());
                }
            }
        }

        // Extract gamemode base libraries detected by the VSCode extension
        if let Some(libraries) = init_options.get("gamemodeBaseLibraries") {
            if let Some(arr) = libraries.as_array() {
                for lib in arr {
                    if let Some(lib_str) = lib.as_str() {
                        if !lib_str.is_empty() {
                            client_config.gamemode_base_libraries.push(lib_str.to_string());
                        }
                    }
                }
                if !client_config.gamemode_base_libraries.is_empty() {
                    log::info!(
                        "Received gamemode base libraries from VSCode: {:?}",
                        client_config.gamemode_base_libraries
                    );
                }
            }
        }
    }

    // Apply CLI-provided annotations path (highest precedence after .gluarc.json)
    if let Some(cli_arg) = &cmd_args.gmod_annotations_path {
        if let Some(cli_path) = cli_arg.as_deref() {
            log::info!("Using GMod annotations path from CLI: {}", cli_path);
            client_config.gmod_annotations_path = Some(cli_path.to_string());
        } else {
            log::info!("GMod annotations explicitly disabled via CLI");
            client_config.gmod_annotations_path = Some(String::new());
        }
    }

    log::info!("client_config: {:?}", client_config);

    let params_json = serde_json::to_string_pretty(&params).unwrap();
    log::info!("initialization_params: {}", params_json);

    // init config
    let config_roots = workspace_folders
        .iter()
        .map(|workspace| workspace.root.clone())
        .collect();
    let loaded = load_emmy_config(config_roots, client_config.clone());
    let emmyrc = loaded.emmyrc;
    let workspace_diagnostic_configs = loaded.workspace_diagnostic_configs;
    let workspace_emmyrcs = loaded.workspace_emmyrcs;
    let workspace_matchers = loaded.workspace_matchers;
    load_editorconfig(workspace_folders.clone(), emmyrc.as_ref());

    // init std lib
    init_std_lib(context.analysis(), &cmd_args, emmyrc.clone()).await;

    {
        let mut workspace_manager = context.workspace_manager().write().await;
        workspace_manager.client_config = client_config.clone();
        let (include, exclude, exclude_dir) = calculate_include_and_exclude(&emmyrc);
        workspace_manager.match_file_pattern =
            WorkspaceFileMatcher::new(include, exclude, exclude_dir);
        workspace_manager.per_root_matchers = workspace_matchers;
        log::info!("workspace manager updated with client config and watch file patterns")
    }

    init_analysis(
        context.analysis(),
        context.status_bar(),
        context.file_diagnostic(),
        context.lsp_features(),
        workspace_folders,
        emmyrc.clone(),
        workspace_diagnostic_configs,
        workspace_emmyrcs,
    )
    .await;

    register_files_watch(context.clone(), &params.capabilities).await;
    Some(())
}

pub async fn init_analysis(
    analysis: &RwLock<EmmyLuaAnalysis>,
    status_bar: &StatusBar,
    file_diagnostic: &FileDiagnostic,
    lsp_features: &LspFeatures,
    workspace_folders: Vec<WorkspaceFolder>,
    emmyrc: Arc<Emmyrc>,
    workspace_diagnostic_configs: HashMap<PathBuf, LuaDiagnosticConfig>,
    workspace_emmyrcs: HashMap<PathBuf, Arc<Emmyrc>>,
) {
    if let Ok(emmyrc_json) = serde_json::to_string_pretty(emmyrc.as_ref()) {
        log::info!("current config : {}", emmyrc_json);
    }

    status_bar
        .create_progress_task(ProgressTask::LoadWorkspace)
        .await;
    status_bar.update_progress_task(
        ProgressTask::LoadWorkspace,
        None,
        Some("Loading workspace files".to_string()),
    );

    let workspace_roots = workspace_folders
        .into_iter()
        .map(|workspace| workspace.root)
        .collect::<Vec<_>>();

    let mut workspace_collection_groups: Vec<(Arc<Emmyrc>, Vec<WorkspaceFolder>)> = Vec::new();
    if workspace_roots.is_empty() {
        workspace_collection_groups.push((
            emmyrc.clone(),
            build_workspace_collection_folders(None, emmyrc.as_ref()),
        ));
    } else {
        for workspace_root in workspace_roots {
            let workspace_config = workspace_emmyrcs
                .get(&workspace_root)
                .cloned()
                .unwrap_or_else(|| emmyrc.clone());
            workspace_collection_groups.push((
                workspace_config.clone(),
                build_workspace_collection_folders(Some(workspace_root), workspace_config.as_ref()),
            ));
        }
    }

    status_bar.update_progress_task(
        ProgressTask::LoadWorkspace,
        None,
        Some(String::from("Collecting files")),
    );

    // load files with per-workspace configs
    let mut files = Vec::new();
    let mut loaded_paths = HashSet::new();
    let mut canonical_path_cache: HashMap<PathBuf, PathBuf> = HashMap::new();
    for (workspace_config, workspace_group) in &workspace_collection_groups {
        for file in collect_workspace_files(workspace_group, workspace_config.as_ref(), None, None)
        {
            let raw_path = PathBuf::from(&file.path);
            let dedup_key = if let Some(cached) = canonical_path_cache.get(&raw_path) {
                cached.clone()
            } else {
                let canonical_path = raw_path.canonicalize().unwrap_or_else(|_| raw_path.clone());
                let normalized = if cfg!(windows) {
                    PathBuf::from(canonical_path.to_string_lossy().to_ascii_lowercase())
                } else {
                    canonical_path
                };
                canonical_path_cache.insert(raw_path, normalized.clone());
                normalized
            };
            if loaded_paths.insert(dedup_key) {
                files.push(file.into_tuple());
            }
        }
    }

    let file_count = files.len();
    if file_count != 0 {
        status_bar.update_progress_task(
            ProgressTask::LoadWorkspace,
            None,
            Some(format!("Indexing {} files", file_count)),
        );
    }

    // Hold the write lock only for analysis state mutations.
    let mut mut_analysis = analysis.write().await;

    // update config
    mut_analysis.update_config(emmyrc.clone());

    let mut added_main_roots = HashSet::new();
    let mut added_library_roots = HashSet::new();
    for (_, workspace_group) in &workspace_collection_groups {
        for workspace in workspace_group {
            if workspace.is_library {
                if added_library_roots.insert(workspace.root.clone()) {
                    log::info!("add library: {:?}", workspace.root);
                    mut_analysis.add_library_workspace(workspace.root.clone());
                }
            } else if added_main_roots.insert(workspace.root.clone()) {
                log::info!("add workspace root: {:?}", workspace.root);
                mut_analysis.add_main_workspace(workspace.root.clone());
            }
        }
    }

    // Map workspace root paths to WorkspaceIds for per-workspace diagnostic configs
    if !workspace_diagnostic_configs.is_empty() {
        let mut ws_diag_configs = HashMap::new();
        for (root, diag_config) in workspace_diagnostic_configs {
            if let Some(workspace_id) = mut_analysis.get_workspace_id_for_root(&root) {
                log::info!(
                    "setting per-workspace diagnostic config for {:?} (workspace_id: {:?})",
                    root,
                    workspace_id
                );
                ws_diag_configs.insert(workspace_id, Arc::new(diag_config));
            }
        }
        if !ws_diag_configs.is_empty() {
            mut_analysis.set_workspace_diagnostic_configs(ws_diag_configs);
        }
    }

    if file_count != 0 {
        mut_analysis.update_files_by_path(files);
    }

    let schema_urls = if mut_analysis.check_schema_update() {
        mut_analysis.get_schemas_to_fetch()
    } else {
        Vec::new()
    };

    drop(mut_analysis);

    status_bar.update_progress_task(
        ProgressTask::LoadWorkspace,
        None,
        Some(String::from("Finished loading workspace files")),
    );
    status_bar.finish_progress_task(
        ProgressTask::LoadWorkspace,
        Some("Indexing complete".to_string()),
    );

    if !schema_urls.is_empty() {
        let url_contents = fetch_schema_urls(schema_urls).await;
        let mut mut_analysis = analysis.write().await;
        mut_analysis.apply_fetched_schemas(url_contents);
    }

    if !lsp_features.supports_workspace_diagnostic() {
        file_diagnostic
            .add_workspace_diagnostic_task(0, false)
            .await;
    }
}

fn build_workspace_collection_folders(
    workspace_root: Option<PathBuf>,
    emmyrc: &Emmyrc,
) -> Vec<WorkspaceFolder> {
    let mut workspaces = Vec::new();

    if let Some(workspace_root) = workspace_root {
        workspaces.push(WorkspaceFolder::new(workspace_root, false));
    }

    for extra_root in &emmyrc.workspace.workspace_roots {
        workspaces.push(WorkspaceFolder::new(PathBuf::from(extra_root), false));
    }

    for lib in &emmyrc.workspace.library {
        workspaces.push(WorkspaceFolder::new(PathBuf::from(lib.get_path()), true));
    }

    for package_dir in &emmyrc.workspace.package_dirs {
        let package_path = PathBuf::from(package_dir);
        if let Some(parent) = package_path.parent() {
            if let Some(name) = package_path.file_name() {
                workspaces.push(WorkspaceFolder::with_sub_paths(
                    parent.to_path_buf(),
                    vec![PathBuf::from(name)],
                    true,
                ));
            } else {
                log::warn!("package dir {:?} has no file name", package_path);
            }
        } else {
            log::warn!("package dir {:?} has no parent", package_path);
        }
    }

    workspaces
}

pub fn get_workspace_folders(params: &InitializeParams) -> Vec<WorkspaceFolder> {
    let mut workspace_folders = Vec::new();
    if let Some(workspaces) = &params.workspace_folders {
        for workspace in workspaces {
            if let Some(path) = uri_to_file_path(&workspace.uri) {
                workspace_folders.push(WorkspaceFolder::new(path, false));
            }
        }
    }

    if workspace_folders.is_empty() {
        // However, most LSP clients still provide this field
        #[allow(deprecated)]
        if let Some(uri) = &params.root_uri {
            let root_workspace = uri_to_file_path(uri);
            if let Some(path) = root_workspace {
                workspace_folders.push(WorkspaceFolder::new(path, false));
            }
        }
    }

    workspace_folders
}

pub async fn init_std_lib(
    analysis: &RwLock<EmmyLuaAnalysis>,
    cmd_args: &CmdArgs,
    emmyrc: Arc<Emmyrc>,
) {
    log::info!(
        "initializing std lib with resources path: {:?}",
        cmd_args.resources_path
    );
    let mut analysis = analysis.write().await;
    if cmd_args.load_stdlib.0 {
        // double update config
        analysis.update_config(emmyrc);
        try_generate_translated_std();
        analysis.init_std_lib(cmd_args.resources_path.0.clone());
    }

    log::info!("initialized std lib complete");
}
