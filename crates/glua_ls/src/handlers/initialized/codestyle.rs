use glua_code_analysis::{Emmyrc, WorkspaceFolder};

pub fn load_editorconfig(workspace_folders: Vec<WorkspaceFolder>, emmyrc: &Emmyrc) -> Option<()> {
    crate::codestyle::apply_workspace_code_style(&workspace_folders, emmyrc)
}
