use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use glua_code_analysis::{
    WorkspaceFolder, collect_workspace_files, file_path_to_uri, uri_to_file_path,
};
use lsp_types::{Uri, request::Request};
use tokio_util::sync::CancellationToken;

use crate::context::{
    GamemodeChoiceReason, ServerContextSnapshot, SetActiveGamemodeParams, SetActiveGamemodeResult,
};

#[derive(Debug)]
pub enum SetActiveGamemodeRequest {}

impl Request for SetActiveGamemodeRequest {
    type Params = SetActiveGamemodeParams;
    type Result = Option<SetActiveGamemodeResult>;
    const METHOD: &'static str = "gluals/setActiveGamemode";
}

pub async fn on_set_active_gamemode(
    context: ServerContextSnapshot,
    params: SetActiveGamemodeParams,
    _cancel_token: CancellationToken,
) -> Option<SetActiveGamemodeResult> {
    let selection_lock = {
        let workspace = context.workspace_manager().read().await;
        workspace.gamemode_selection_lock()
    };
    let _selection_guard = selection_lock.lock().await;
    let selected_gamemode_id = params.selected_gamemode_id.clone();
    switch_active_gamemode(&context, params.selected_gamemode_id, params.open_documents).await?;
    Some(SetActiveGamemodeResult {
        selected_gamemode_id,
    })
}

pub async fn ensure_gamemode_loaded_for_document(
    context: &ServerContextSnapshot,
    uri: &Uri,
) -> bool {
    let selection_lock = {
        let workspace = context.workspace_manager().read().await;
        workspace.gamemode_selection_lock()
    };
    let _selection_guard = selection_lock.lock().await;

    let (requested_id, interactive, already_loaded, choice_params) = {
        let workspace = context.workspace_manager().read().await;
        let Some(project_loading) = workspace.project_loading.as_ref() else {
            return true;
        };
        let Some(gamemode) = project_loading.gamemode_for_uri(uri) else {
            return true;
        };
        let requested_id = gamemode.id.clone();
        (
            requested_id.clone(),
            project_loading.interactive(),
            project_loading.is_gamemode_loaded(&requested_id),
            project_loading.choose_params(Some(requested_id), GamemodeChoiceReason::DocumentOpen),
        )
    };

    if already_loaded {
        return true;
    }

    let selected_id = if interactive {
        request_document_gamemode_choice(context, choice_params).await
    } else {
        Some(requested_id.clone())
    };
    let Some(selected_id) = selected_id else {
        return false;
    };
    if selected_id != requested_id {
        return false;
    }

    switch_active_gamemode(context, selected_id, Vec::new())
        .await
        .is_some()
}

async fn request_document_gamemode_choice(
    context: &ServerContextSnapshot,
    params: crate::context::ChooseGamemodeParams,
) -> Option<String> {
    let response = context
        .client()
        .send_request(
            context.client().next_id(),
            "gluals/chooseGamemode",
            params,
            CancellationToken::new(),
        )
        .await?;
    let result = response.result?;
    if result.is_null() {
        return None;
    }
    serde_json::from_value::<crate::context::ChooseGamemodeResult>(result)
        .ok()?
        .selected_gamemode_id
}

async fn switch_active_gamemode(
    context: &ServerContextSnapshot,
    selected_gamemode_id: String,
    open_document_snapshots: Vec<crate::context::OpenDocumentSnapshot>,
) -> Option<()> {
    let (
        old_roots,
        new_roots,
        new_base_roots,
        workspace_emmyrcs,
        merged_emmyrc,
        open_documents,
        changed,
        project_loading_state,
    ) = {
        let merged_emmyrc = context.analysis().read().await.get_emmyrc();
        let mut workspace = context.workspace_manager().write().await;
        let project_loading = workspace.project_loading.as_mut()?;
        if !project_loading.is_valid_primary_id(&selected_gamemode_id) {
            return None;
        }

        let old_roots = project_loading.loaded_gamemode_roots();
        project_loading.merge_open_document_snapshots(open_document_snapshots);
        let changed = project_loading.set_active_gamemode(Some(selected_gamemode_id));
        let new_roots = project_loading.loaded_gamemode_roots();
        let new_base_roots = new_roots.iter().skip(1).cloned().collect::<Vec<_>>();
        let open_documents = project_loading.open_documents_in_loaded_projects();
        let project_loading_state = project_loading.state();
        let loaded_workspace_folders = project_loading.loaded_workspace_folders();
        workspace.workspace_folders = loaded_workspace_folders;
        workspace.update_workspace_version(crate::context::WorkspaceDiagnosticLevel::Fast, true);

        (
            old_roots,
            new_roots,
            new_base_roots,
            workspace.workspace_emmyrcs.clone(),
            merged_emmyrc,
            open_documents,
            changed,
            project_loading_state,
        )
    };

    let old_only_roots = old_roots
        .iter()
        .filter(|old_root| {
            !new_roots
                .iter()
                .any(|new_root| paths_equal(old_root, new_root))
        })
        .cloned()
        .collect::<Vec<_>>();
    let new_only_roots = new_roots
        .iter()
        .filter(|new_root| {
            !old_roots
                .iter()
                .any(|old_root| paths_equal(old_root, new_root))
        })
        .cloned()
        .collect::<Vec<_>>();

    if !changed && open_documents.is_empty() {
        return Some(());
    }

    let removed_uris = {
        let analysis = context.analysis().read().await;
        let vfs = analysis.compilation.get_db().get_vfs();
        vfs.get_all_file_ids()
            .into_iter()
            .filter_map(|file_id| {
                let path = vfs.get_file_path(&file_id)?;
                old_only_roots
                    .iter()
                    .any(|root| path.starts_with(root))
                    .then(|| vfs.get_uri(&file_id))
                    .flatten()
            })
            .collect::<Vec<_>>()
    };

    let mut updates = HashMap::<Uri, Option<String>>::new();
    for uri in &removed_uris {
        updates.insert(uri.clone(), None);
    }
    for root in &new_only_roots {
        let config = nearest_config(root, &workspace_emmyrcs).unwrap_or(&merged_emmyrc);
        let is_library = new_base_roots
            .iter()
            .any(|base_root| paths_equal(base_root, root));
        for file in collect_workspace_files(
            &vec![WorkspaceFolder::new(root.clone(), is_library)],
            config.as_ref(),
            None,
            None,
        ) {
            if let Some(uri) = file_path_to_uri(&PathBuf::from(&file.path)) {
                updates.insert(uri, Some(file.content));
            }
        }
    }
    for (uri, document) in &open_documents {
        if is_uri_in_roots(uri, &new_roots) {
            updates.insert(uri.clone(), Some(document.text.clone()));
        }
    }

    let updated_file_ids = {
        let mut analysis = context.analysis().write().await;
        for base_root in new_base_roots {
            analysis.add_library_workspace(base_root);
        }
        let mut updates = updates.into_iter().collect::<Vec<_>>();
        updates.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
        let updated = analysis.update_files_by_uri(updates);
        context
            .file_diagnostic()
            .invalidate_shared_diagnostic_data();
        updated
    };

    if !context.lsp_features().supports_pull_diagnostic() {
        for uri in removed_uris {
            context
                .file_diagnostic()
                .clear_push_file_diagnostics(uri)
                .await;
        }
        let interval = merged_emmyrc.diagnostics.diagnostic_interval.unwrap_or(500);
        context
            .file_diagnostic()
            .add_files_diagnostic_task(
                updated_file_ids,
                interval,
                Some(context.debounced_analysis_arc()),
            )
            .await;
    }

    context.client().refresh_semantic_tokens();
    context.client().refresh_inlay_hints();
    context.client().refresh_code_lens();
    if context.lsp_features().supports_workspace_diagnostic() {
        context.client().refresh_workspace_diagnostics();
    }
    context
        .client()
        .send_notification("gluals/projectsChanged", project_loading_state);

    for (uri, document) in open_documents {
        if is_uri_in_roots(&uri, &new_roots) {
            context
                .note_document_applied_version(&uri, document.version)
                .await;
        }
    }

    Some(())
}

fn nearest_config<'a>(
    path: &Path,
    configs: &'a HashMap<PathBuf, std::sync::Arc<glua_code_analysis::Emmyrc>>,
) -> Option<&'a std::sync::Arc<glua_code_analysis::Emmyrc>> {
    configs
        .iter()
        .filter(|(root, _)| path.starts_with(root))
        .max_by_key(|(root, _)| root.as_os_str().len())
        .map(|(_, config)| config)
}

fn is_uri_in_roots(uri: &Uri, roots: &[PathBuf]) -> bool {
    uri_to_file_path(uri).is_some_and(|path| roots.iter().any(|root| path.starts_with(root)))
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}
