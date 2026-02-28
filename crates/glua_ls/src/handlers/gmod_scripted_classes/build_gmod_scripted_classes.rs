use std::path::Path;

use glua_code_analysis::{
    DbIndex, FileId, GmodClassCallLiteral, GmodScriptedClassCallMetadata, LuaDocument,
    file_path_to_uri,
};
use tokio_util::sync::CancellationToken;
use wax::Pattern;

use super::gmod_scripted_classes_request::GmodScriptedClassEntry;

#[derive(Debug, Clone, Copy)]
struct ScopedClassRule {
    class_type: &'static str,
    folder_segments: &'static [&'static str],
}

#[derive(Debug, Clone)]
struct ScopedClassMatch {
    class_type: &'static str,
    class_name: String,
}

const SCOPED_CLASS_RULES: &[ScopedClassRule] = &[
    ScopedClassRule {
        class_type: "TOOL",
        folder_segments: &["weapons", "gmod_tool", "stools"],
    },
    ScopedClassRule {
        class_type: "ENT",
        folder_segments: &["entities"],
    },
    ScopedClassRule {
        class_type: "SWEP",
        folder_segments: &["weapons"],
    },
    ScopedClassRule {
        class_type: "EFFECT",
        folder_segments: &["effects"],
    },
    ScopedClassRule {
        class_type: "PLUGIN",
        folder_segments: &["plugins"],
    },
];

pub fn build_gmod_scripted_classes(
    db: &DbIndex,
    cancel_token: &CancellationToken,
) -> Option<Vec<GmodScriptedClassEntry>> {
    if cancel_token.is_cancelled() {
        return None;
    }

    let scopes = &db.get_emmyrc().gmod.scripted_class_scopes;
    let include_glob = if scopes.include.is_empty() {
        None
    } else {
        let include_patterns = scopes
            .include
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        match wax::any(include_patterns) {
            Ok(glob) => Some(glob),
            Err(err) => {
                log::warn!("Invalid gmod.scriptedClassScopes.include pattern: {err}");
                None
            }
        }
    };

    let exclude_glob = if scopes.exclude.is_empty() {
        None
    } else {
        let exclude_patterns = scopes
            .exclude
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        match wax::any(exclude_patterns) {
            Ok(glob) => Some(glob),
            Err(err) => {
                log::warn!("Invalid gmod.scriptedClassScopes.exclude pattern: {err}");
                return Some(Vec::new());
            }
        }
    };

    let mut entries = Vec::new();
    for file_id in db.get_vfs().get_all_local_file_ids() {
        if cancel_token.is_cancelled() {
            return None;
        }

        if !is_file_in_scope(db, file_id, include_glob.as_ref(), exclude_glob.as_ref()) {
            continue;
        }

        let Some(file_path) = db.get_vfs().get_file_path(&file_id) else {
            continue;
        };
        let Some(scope_match) = detect_scoped_class_from_path(file_path) else {
            continue;
        };
        let Some(uri) = file_uri_string(db, file_id) else {
            continue;
        };

        entries.push(GmodScriptedClassEntry {
            uri,
            class_type: scope_match.class_type.to_string(),
            class_name: scope_match.class_name,
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

    Some(entries)
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

fn is_file_in_scope(
    db: &DbIndex,
    file_id: FileId,
    include_glob: Option<&wax::Any<'_>>,
    exclude_glob: Option<&wax::Any<'_>>,
) -> bool {
    let Some(file_path) = db.get_vfs().get_file_path(&file_id) else {
        return include_glob.is_none();
    };

    let normalized_path = file_path.to_string_lossy().replace('\\', "/");
    let mut candidate_paths = Vec::new();
    push_path_candidates(&mut candidate_paths, &normalized_path);

    let normalized_lower = normalized_path.to_ascii_lowercase();
    if let Some(lua_idx) = normalized_lower.find("/lua/") {
        let lua_relative_path = normalized_path[lua_idx + 1..].to_string();
        push_path_candidates(&mut candidate_paths, &lua_relative_path);
        if let Some(stripped) = lua_relative_path.strip_prefix("lua/") {
            push_path_candidates(&mut candidate_paths, stripped);
        }
    }

    if let Some(file_name) = file_path.file_name().and_then(|name| name.to_str()) {
        push_candidate_path(&mut candidate_paths, file_name);
    }

    if let Some(include) = include_glob
        && !candidate_paths
            .iter()
            .any(|path| include.is_match(Path::new(path)))
    {
        return false;
    }

    if let Some(exclude) = exclude_glob
        && candidate_paths
            .iter()
            .any(|path| exclude.is_match(Path::new(path)))
    {
        return false;
    }

    true
}

fn detect_scoped_class_from_path(file_path: &Path) -> Option<ScopedClassMatch> {
    let normalized_path = file_path.to_string_lossy().replace('\\', "/");
    let lower_segments = normalized_path
        .to_ascii_lowercase()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if lower_segments.is_empty() {
        return None;
    }

    let mut best_match: Option<(&ScopedClassRule, usize, usize)> = None;
    for rule in SCOPED_CLASS_RULES {
        let rule_len = rule.folder_segments.len();
        if rule_len == 0 || lower_segments.len() < rule_len {
            continue;
        }

        for start_idx in (0..=lower_segments.len() - rule_len).rev() {
            let mut matched = true;
            for (offset, rule_segment) in rule.folder_segments.iter().enumerate() {
                if lower_segments[start_idx + offset] != *rule_segment {
                    matched = false;
                    break;
                }
            }

            if !matched {
                continue;
            }

            let end_idx = start_idx + rule_len - 1;
            let replace_best = match best_match {
                None => true,
                Some((_, best_end_idx, best_rule_len)) => {
                    end_idx > best_end_idx || (end_idx == best_end_idx && rule_len > best_rule_len)
                }
            };
            if replace_best {
                best_match = Some((rule, end_idx, rule_len));
            }

            break;
        }
    }

    let (rule, best_end_idx, _) = best_match?;
    let class_idx = best_end_idx + 1;
    if class_idx >= lower_segments.len() {
        return None;
    }

    let class_name = if class_idx == lower_segments.len() - 1 {
        lower_segments[class_idx]
            .strip_suffix(".lua")
            .unwrap_or(lower_segments[class_idx].as_str())
            .to_string()
    } else {
        lower_segments[class_idx].clone()
    };

    if class_name.is_empty() {
        return None;
    }

    Some(ScopedClassMatch {
        class_type: rule.class_type,
        class_name,
    })
}

fn push_path_candidates(candidate_paths: &mut Vec<String>, path: &str) {
    push_candidate_path(candidate_paths, path);

    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    for idx in 0..segments.len() {
        push_candidate_path(candidate_paths, &segments[idx..].join("/"));
    }
}

fn push_candidate_path(candidate_paths: &mut Vec<String>, candidate: &str) {
    if candidate.is_empty() {
        return;
    }

    if candidate_paths.iter().any(|existing| existing == candidate) {
        return;
    }

    candidate_paths.push(candidate.to_string());
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

        let entries =
            build_gmod_scripted_classes(ws.get_db_mut(), &CancellationToken::new()).or_fail()?;

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

        let entries =
            build_gmod_scripted_classes(ws.get_db_mut(), &CancellationToken::new()).or_fail()?;

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
