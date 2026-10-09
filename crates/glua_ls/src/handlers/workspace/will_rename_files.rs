use std::{collections::HashMap, str::FromStr};

use glua_code_analysis::{FileRenames, load_path_rename_edits, uri_to_file_path};
use lsp_types::{FileRename, RenameFilesParams, TextEdit, Uri, WorkspaceEdit};
use tokio_util::sync::CancellationToken;

use crate::context::ServerContextSnapshot;

/// Rewrites the load paths naming the files about to be renamed; the client
/// applies the edit together with the rename.
pub async fn on_will_rename_files_handler(
    context: ServerContextSnapshot,
    params: RenameFilesParams,
    cancel_token: CancellationToken,
) -> Option<WorkspaceEdit> {
    let renames = file_renames(&params.files);
    if renames.is_empty() {
        return None;
    }
    let analysis = context.read_analysis(&cancel_token).await?;
    let db = analysis.compilation.get_db();
    let vfs = db.get_vfs();
    #[allow(clippy::mutable_key_type)]
    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
    for edit in load_path_rename_edits(db, &renames) {
        let (Some(uri), Some(document)) =
            (vfs.get_uri(&edit.file_id), vfs.get_document(&edit.file_id))
        else {
            continue;
        };
        let Some(range) = document.to_lsp_range(edit.range) else {
            continue;
        };
        changes.entry(uri).or_default().push(TextEdit {
            range,
            new_text: edit.new_path,
        });
    }
    if changes.is_empty() {
        return None;
    }
    Some(WorkspaceEdit {
        changes: Some(changes),
        ..Default::default()
    })
}

/// The renames between `file:` URIs, as paths.
pub(super) fn file_renames(files: &[FileRename]) -> FileRenames {
    FileRenames::new(
        files
            .iter()
            .filter_map(|rename| {
                let old_path = uri_to_file_path(&Uri::from_str(&rename.old_uri).ok()?)?;
                let new_path = uri_to_file_path(&Uri::from_str(&rename.new_uri).ok()?)?;
                Some((old_path, new_path))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::Path};

    use glua_code_analysis::file_path_to_uri;
    use lsp_types::{
        ClientCapabilities, FileRename, Position, Range, RenameFilesParams, TextEdit, Uri,
        WorkspaceEdit,
    };
    use tokio_util::sync::CancellationToken;

    use crate::context::ServerContext;

    fn uri(root: &Path, path: &str) -> Uri {
        file_path_to_uri(&root.join(path)).expect("file URI")
    }

    fn text_edit(line: u32, start: u32, end: u32, new_text: &str) -> TextEdit {
        TextEdit {
            range: Range::new(Position::new(line, start), Position::new(line, end)),
            new_text: new_text.to_string(),
        }
    }

    /// The edit `workspace/willRenameFiles` answers for `renames` in a
    /// workspace holding `files`.
    fn will_rename_edit(
        files: &[(&str, &str)],
        renames: &[(&str, &str)],
    ) -> (std::path::PathBuf, Option<WorkspaceEdit>) {
        let root = crate::util::unique_temp_dir("gmod_glua_ls_will_rename");
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (connection, _peer) = lsp_server::Connection::memory();
        let edit = runtime.block_on(async {
            let context = ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            {
                let mut analysis = snapshot.analysis().write().await;
                analysis.add_main_workspace(root.clone());
                for (path, text) in files {
                    analysis.update_file_by_uri(&uri(&root, path), Some(text.to_string()));
                }
            }
            let params = RenameFilesParams {
                files: renames
                    .iter()
                    .map(|(old, new)| FileRename {
                        old_uri: uri(&root, old).to_string(),
                        new_uri: uri(&root, new).to_string(),
                    })
                    .collect(),
            };
            super::on_will_rename_files_handler(snapshot, params, CancellationToken::new()).await
        });
        (root, edit)
    }

    /// Renamed files get their `AddCSLuaFile`, `include` and `require`
    /// strings rewritten in the client's positions, and nothing else.
    #[test]
    #[allow(clippy::mutable_key_type)]
    fn renaming_files_rewrites_the_strings_loading_them() {
        let (root, edit) = will_rename_edit(
            &[
                ("lua/myaddon/sh_config.lua", "return {}\n"),
                ("lua/includes/modules/myutil.lua", "return {}\n"),
                ("lua/autorun/sh_other.lua", "return {}\n"),
                (
                    "lua/autorun/sh_loader.lua",
                    "AddCSLuaFile(\"myaddon/sh_config.lua\")\ninclude(\"myaddon/sh_config.lua\")\nlocal util = require(\"myutil\")\ninclude(\"sh_other.lua\")\n",
                ),
            ],
            &[
                (
                    "lua/myaddon/sh_config.lua",
                    "lua/myaddon/shared/sh_settings.lua",
                ),
                (
                    "lua/includes/modules/myutil.lua",
                    "lua/includes/modules/myhelpers.lua",
                ),
            ],
        );

        let expected = WorkspaceEdit {
            changes: Some(HashMap::from([(
                uri(&root, "lua/autorun/sh_loader.lua"),
                vec![
                    text_edit(0, 14, 35, "myaddon/shared/sh_settings.lua"),
                    text_edit(1, 9, 30, "myaddon/shared/sh_settings.lua"),
                    text_edit(2, 22, 28, "myhelpers"),
                ],
            )])),
            ..Default::default()
        };
        assert_eq!(edit, Some(expected));
    }

    /// Renaming a gamemode folder rewrites the paths into it from the files
    /// outside it.
    #[test]
    #[allow(clippy::mutable_key_type)]
    fn renaming_a_folder_rewrites_the_paths_into_it() {
        let (root, edit) = will_rename_edit(
            &[
                ("gamemodes/mygm/gamemode/modules/cl_hud.lua", "return {}\n"),
                (
                    "gamemodes/mygm/gamemode/modules/sh_round.lua",
                    "return {}\n",
                ),
                (
                    "gamemodes/mygm/gamemode/init.lua",
                    "AddCSLuaFile(\"modules/cl_hud.lua\")\ninclude(\"modules/sh_round.lua\")\n",
                ),
            ],
            &[(
                "gamemodes/mygm/gamemode/modules",
                "gamemodes/mygm/gamemode/systems",
            )],
        );

        let expected = WorkspaceEdit {
            changes: Some(HashMap::from([(
                uri(&root, "gamemodes/mygm/gamemode/init.lua"),
                vec![
                    text_edit(0, 14, 32, "systems/cl_hud.lua"),
                    text_edit(1, 9, 29, "systems/sh_round.lua"),
                ],
            )])),
            ..Default::default()
        };
        assert_eq!(edit, Some(expected));
    }
}
