pub mod cmd_args;
mod init;
mod output;
mod terminal_display;

pub use cmd_args::*;
use output::output_result;
use std::{
    error::Error,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

use crate::init::setup_logger;

fn normalize_path_for_compare(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let path_str = path.to_string_lossy();
        if let Some(stripped) = path_str.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }

    path.to_path_buf()
}

pub async fn run_check(cmd_args: CmdArgs) -> Result<(), Box<dyn Error + Sync + Send>> {
    setup_logger(cmd_args.verbose);

    let cwd = std::env::current_dir()?;
    let workspaces: Vec<_> = cmd_args
        .workspace
        .into_iter()
        .map(|workspace| {
            if workspace.is_absolute() {
                workspace
            } else {
                cwd.join(workspace)
            }
        })
        .collect();
    let main_path = workspaces
        .first()
        .ok_or("Failed to load workspace")?
        .clone();

    let analysis = match init::load_workspace(
        main_path.clone(),
        workspaces.clone(),
        cmd_args.config,
        cmd_args.ignore,
        cmd_args.gmod_annotations,
    )
    .await
    {
        Some(analysis) => analysis,
        None => {
            return Err("Failed to load workspace".into());
        }
    };

    let db = analysis.compilation.get_db();
    let ignore_dirs: Vec<PathBuf> = analysis
        .emmyrc
        .workspace
        .ignore_dir
        .iter()
        .map(|d| normalize_path_for_compare(Path::new(d)))
        .collect();
    let need_check_files: Vec<_> = db
        .get_module_index()
        .get_main_workspace_file_ids()
        .into_iter()
        .filter(|file_id| {
            if let Some(file_path) = db.get_vfs().get_file_path(file_id) {
                let normalized_file_path = normalize_path_for_compare(file_path.as_path());
                !ignore_dirs
                    .iter()
                    .any(|dir| normalized_file_path.starts_with(dir))
            } else {
                true
            }
        })
        .collect();

    let (sender, receiver) = tokio::sync::mpsc::channel(100);
    let analysis = Arc::new(analysis);
    let shared_data = analysis.precompute_diagnostic_shared_data();
    let db = analysis.compilation.get_db();
    for file_id in need_check_files.clone() {
        let sender = sender.clone();
        let analysis = analysis.clone();
        let shared_data = shared_data.clone();
        tokio::spawn(async move {
            let cancel_token = CancellationToken::new();
            let diagnostics =
                analysis.diagnose_file_with_shared(file_id, cancel_token, shared_data);
            sender.send((file_id, diagnostics)).await.unwrap();
        });
    }

    let exit_code = output_result(
        need_check_files.len(),
        db,
        main_path,
        receiver,
        cmd_args.output_format,
        cmd_args.output,
        cmd_args.warnings_as_errors,
    )
    .await;

    if exit_code != 0 {
        return Err(format!("exit code: {}", exit_code).into());
    }

    eprintln!("Check finished");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::normalize_path_for_compare;

    #[cfg(windows)]
    #[test]
    fn test_normalize_path_for_compare_strips_verbatim_prefix() {
        let normalized = normalize_path_for_compare(PathBuf::from(r"\\?\C:\project\lua").as_path());
        assert_eq!(normalized, PathBuf::from(r"C:\project\lua"));
    }

    #[test]
    fn test_normalize_path_for_compare_preserves_normal_path() {
        let path = PathBuf::from(r"C:\project\lua");
        let normalized = normalize_path_for_compare(path.as_path());
        assert_eq!(normalized, path);
    }
}
