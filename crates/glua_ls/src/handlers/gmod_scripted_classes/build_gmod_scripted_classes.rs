use glua_code_analysis::{
    DbIndex, FileId, GmodClassCallLiteral, GmodScriptedClassCallMetadata, LuaDocument,
    file_path_to_uri,
};
use tokio_util::sync::CancellationToken;

use super::gmod_scripted_classes_request::{GmodScriptedClassEntry, GmodScriptedClassesResult};

pub fn build_gmod_scripted_classes(
    db: &DbIndex,
    cancel_token: &CancellationToken,
) -> Option<GmodScriptedClassesResult> {
    if cancel_token.is_cancelled() {
        return None;
    }

    let scopes = &db.get_emmyrc().gmod.scripted_class_scopes;
    let definitions = scopes.resolved_definitions();

    let mut file_paths = Vec::new();
    for file_id in db.get_vfs().get_all_local_file_ids() {
        if cancel_token.is_cancelled() {
            return None;
        }

        if let Some(file_path) = db.get_vfs().get_file_path(&file_id) {
            file_paths.push((file_id, file_path.as_path()));
        }
    }

    let (_, scoped_matches) = scopes.scan_scripted_class_scope_files(file_paths);

    let mut entries = Vec::new();
    for (file_id, scope_match) in scoped_matches {
        if cancel_token.is_cancelled() {
            return None;
        }

        let Some(uri) = file_uri_string(db, file_id) else {
            continue;
        };

        entries.push(GmodScriptedClassEntry {
            uri,
            class_type: scope_match.definition.class_global.clone(),
            class_name: scope_match.class_name,
            definition_id: Some(scope_match.definition.id),
            range: None,
        });
    }

    for (file_id, file_metadata) in db.get_gmod_class_metadata_index().iter_file_metadata() {
        if cancel_token.is_cancelled() {
            return None;
        }

        let Some(uri) = file_uri_string(db, *file_id) else {
            continue;
        };
        let document = db.get_vfs().get_document(file_id);

        push_vgui_panel_entries(
            &mut entries,
            &uri,
            document.as_ref(),
            &file_metadata.vgui_register_calls,
        );
        push_vgui_panel_entries(
            &mut entries,
            &uri,
            document.as_ref(),
            &file_metadata.derma_define_control_calls,
        );
    }

    entries.sort_by(|left, right| {
        left.class_type
            .cmp(&right.class_type)
            .then_with(|| left.class_name.cmp(&right.class_name))
            .then_with(|| left.uri.cmp(&right.uri))
    });
    entries.dedup_by(|left, right| {
        left.uri == right.uri
            && left.class_type == right.class_type
            && left.class_name == right.class_name
    });

    Some(GmodScriptedClassesResult {
        definitions,
        entries,
    })
}

fn file_uri_string(db: &DbIndex, file_id: FileId) -> Option<String> {
    db.get_vfs()
        .get_uri(&file_id)
        .or_else(|| {
            db.get_vfs()
                .get_file_path(&file_id)
                .and_then(|file_path| file_path_to_uri(&file_path))
        })
        .map(|uri| uri.to_string())
}

fn push_vgui_panel_entries(
    entries: &mut Vec<GmodScriptedClassEntry>,
    uri: &str,
    document: Option<&LuaDocument<'_>>,
    calls: &[GmodScriptedClassCallMetadata],
) {
    for call in calls {
        let Some(panel_name) = extract_vgui_panel_name(call) else {
            continue;
        };
        let range = document.and_then(|doc| doc.to_lsp_range(call.syntax_id.get_range()));

        entries.push(GmodScriptedClassEntry {
            uri: uri.to_string(),
            class_type: "VGUI".to_string(),
            class_name: panel_name.to_string(),
            definition_id: None,
            range,
        });
    }
}

fn extract_vgui_panel_name(call: &GmodScriptedClassCallMetadata) -> Option<&str> {
    match call.literal_args.first() {
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => Some(name.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use glua_code_analysis::{Emmyrc, VirtualWorkspace};
    use googletest::prelude::*;
    use tokio_util::sync::CancellationToken;

    use super::build_gmod_scripted_classes;

    #[gtest]
    fn build_gmod_scripted_classes_filters_to_scoped_paths() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file("lua/entities/test_entity/init.lua", "local ENT = {}");
        ws.def_file("lua/plugins/my_plugin/sh_init.lua", "local PLUGIN = {}");
        ws.def_file("lua/autorun/ignored.lua", "local x = 1");

        let entries = build_gmod_scripted_classes(ws.get_db_mut(), &CancellationToken::new())
            .or_fail()?
            .entries;

        verify_that!(
            entries
                .iter()
                .any(|entry| entry.class_type == "ENT" && entry.class_name == "test_entity"),
            eq(true)
        )?;
        verify_that!(
            entries
                .iter()
                .any(|entry| entry.class_type == "PLUGIN" && entry.class_name == "my_plugin"),
            eq(true)
        )?;
        verify_that!(
            entries.iter().any(|entry| {
                entry.class_type == "ENT"
                    && entry.class_name == "test_entity"
                    && entry.range.is_none()
            }),
            eq(true)
        )?;
        verify_that!(
            entries.iter().any(|entry| entry.class_name == "ignored"),
            eq(false)
        )
    }

    #[gtest]
    fn build_gmod_scripted_classes_uses_per_definition_excludes_for_stools() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/weapons/gmod_tool/stools/hoverball.lua",
            "local TOOL = {}",
        );

        let entries = build_gmod_scripted_classes(ws.get_db_mut(), &CancellationToken::new())
            .or_fail()?
            .entries;

        verify_that!(
            entries.iter().any(|entry| {
                entry.class_type == "TOOL"
                    && entry.class_name == "hoverball"
                    && entry.definition_id.as_deref() == Some("stools")
            }),
            eq(true)
        )
    }

    #[gtest]
    fn build_gmod_scripted_classes_includes_vgui_panels_from_metadata() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/autorun/client/cl_panel_defs.lua",
            r#"
            vgui.Register("MyPanel", {}, "DPanel")
            derma.DefineControl("MyControl", "desc", {}, "DPanel")

            local panel_name = "DynamicPanel"
            vgui.Register(panel_name, {}, "DFrame")
            derma.DefineControl("", "desc", {}, "DLabel")
        "#,
        );

        let entries = build_gmod_scripted_classes(ws.get_db_mut(), &CancellationToken::new())
            .or_fail()?
            .entries;

        verify_that!(
            entries
                .iter()
                .any(|entry| { entry.class_type == "VGUI" && entry.class_name == "MyPanel" }),
            eq(true)
        )?;
        verify_that!(
            entries
                .iter()
                .any(|entry| { entry.class_type == "VGUI" && entry.class_name == "MyControl" }),
            eq(true)
        )?;
        verify_that!(
            entries.iter().any(|entry| {
                entry.class_type == "VGUI" && entry.class_name == "MyPanel" && entry.range.is_some()
            }),
            eq(true)
        )?;
        verify_that!(
            entries
                .iter()
                .any(|entry| { entry.class_type == "VGUI" && entry.class_name == "DynamicPanel" }),
            eq(false)
        )?;
        verify_that!(
            entries
                .iter()
                .any(|entry| entry.class_type == "VGUI" && entry.class_name.is_empty()),
            eq(false)
        )
    }
}
