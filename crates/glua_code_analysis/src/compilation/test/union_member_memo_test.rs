#[cfg(test)]
mod test {
    use crate::VirtualWorkspace;

    const ARMS: usize = 40;
    const READER: &str = "lua/plugins/propertysystem/cl_init.lua";

    fn arm_files() -> Vec<(String, String)> {
        let mut files = vec![(
            "lua/plugins/propertysystem/sh_plugin.lua".to_string(),
            "property = property or {}\nproperty.settings = property.settings or {}\n".to_string(),
        )];
        for arm in 0..ARMS {
            files.push((
                format!("lua/plugins/propertysystem/mapsettings/gm_map_{arm}.lua"),
                format!(
                    "property.settings[1] = {{ [\"rent\"] = {arm}, [\"name\"] = \"arm{arm}\", [\"team\"] = {{ {arm} }} }}\n"
                ),
            ));
        }
        files
    }

    fn reader(extra: &str) -> (String, String) {
        (
            READER.to_string(),
            format!(
                "local n = tonumber(arg)
local first = property.settings[n][\"name\"]
local rent = property.settings[n][\"rent\"]
local again = property.settings[n][\"name\"]
local missing = property.settings[n][\"absent\"]
local teams = property.settings[n][\"team\"]
{extra}"
            ),
        )
    }

    fn borrowed(files: &[(String, String)]) -> Vec<(&str, &str)> {
        files
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect()
    }

    fn local_types(ws: &VirtualWorkspace, names: &[&str]) -> Vec<String> {
        let file_id = ws
            .analysis
            .get_file_id(&ws.virtual_url_generator.new_uri(READER))
            .expect("reader file");
        names
            .iter()
            .map(|name| ws.humanize_type(ws.local_ty(file_id, name)))
            .collect()
    }

    const SAME_FILE_WRITE: &str = "local before = property.settings[n][\"flag\"]
property.settings[1] = { [\"flag\"] = true, [\"name\"] = 5 }
local after = property.settings[n][\"flag\"]
local after_name = property.settings[n][\"name\"]
";

    #[test]
    fn union_member_reads_at_several_sites_agree() {
        let mut files = arm_files();
        files.push(reader(""));
        let mut ws = VirtualWorkspace::new();
        ws.def_files(borrowed(&files));

        let types = local_types(&ws, &["first", "again", "rent", "missing"]);

        assert_eq!(types, ["string", "string", "integer", "nil"]);
    }

    #[test]
    fn union_member_reads_do_not_share_across_a_same_file_write() {
        let mut files = arm_files();
        files.push(reader(SAME_FILE_WRITE));
        let mut ws = VirtualWorkspace::new();
        ws.def_files(borrowed(&files));

        let types = local_types(&ws, &["first", "before", "after", "after_name"]);

        assert_eq!(types, ["string", "unknown", "true", "(integer|string)"]);
    }

    #[test]
    fn union_member_reads_match_cold_after_edit_delete_and_reopen() {
        let mut files = arm_files();
        files.push(reader(SAME_FILE_WRITE));
        let mut warm = VirtualWorkspace::new();
        warm.def_files(borrowed(&files));
        let primed = warm.semantic_digest();

        let edited_path = "lua/plugins/propertysystem/mapsettings/gm_map_3.lua";
        let edited = "property.settings[1] = { [\"rent\"] = true, [\"name\"] = false }
";
        warm.def_file(edited_path, edited);
        let after_edit = warm.semantic_digest();
        let deleted_path = "lua/plugins/propertysystem/mapsettings/gm_map_7.lua";
        let deleted_uri = warm.virtual_url_generator.new_uri(deleted_path);
        warm.analysis.update_file_by_uri(&deleted_uri, None);
        let after_delete = warm.semantic_digest();
        let reopened = "property.settings[1] = { [\"rent\"] = 1.5, [\"name\"] = \"back\" }
";
        warm.def_file(deleted_path, reopened);

        let mut final_files = files.clone();
        for (path, text) in &mut final_files {
            if path == edited_path {
                *text = edited.to_string();
            } else if path == deleted_path {
                *text = reopened.to_string();
            }
        }
        let mut cold = VirtualWorkspace::new();
        cold.def_files(borrowed(&final_files));

        assert_ne!(primed, after_edit);
        assert_ne!(after_edit, after_delete);
        assert_eq!(warm.semantic_digest(), cold.semantic_digest());
    }
}
