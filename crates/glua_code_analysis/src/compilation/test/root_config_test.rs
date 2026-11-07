#[cfg(test)]
mod test {
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
        sync::Arc,
    };

    use crate::{DiagnosticCode, Emmyrc, EmmyrcChange, FileId, VirtualWorkspace};

    const PANEL_FILE: &str = r#"
        local function open(panel)
            local seen = panel
        end
    "#;

    fn with_panel_hint(type_name: &str) -> Emmyrc {
        let mut emmyrc = Emmyrc::default();
        emmyrc
            .gmod
            .file_param_defaults
            .insert("panel".to_string(), type_name.to_string());
        emmyrc
    }

    fn seen_type(ws: &VirtualWorkspace, file_id: FileId) -> String {
        ws.humanize_type(ws.local_ty(file_id, "seen"))
    }

    fn define_dhtml(ws: &mut VirtualWorkspace, root: &Path) {
        ws.def_file_in(root, "dhtml.lua", "---@class DHTML");
    }

    /// A later folder's `gmod.fileParamDefaults` reaches its own files and no
    /// other folder's, whichever folder comes first.
    #[test]
    fn a_folders_config_reaches_only_its_own_files_whatever_the_order() {
        for second_first in [false, true] {
            let names = if second_first {
                ["second", "first"]
            } else {
                ["first", "second"]
            };
            let (mut ws, roots) = VirtualWorkspace::new_with_roots(&names);
            let (first, second) = if second_first {
                (&roots[1], &roots[0])
            } else {
                (&roots[0], &roots[1])
            };
            ws.set_root_emmyrcs(vec![
                (first, Emmyrc::default()),
                (second, with_panel_hint("DHTML")),
            ]);
            define_dhtml(&mut ws, first);
            let in_first = ws.def_file_in(first, "cl_first.lua", PANEL_FILE);
            let in_second = ws.def_file_in(second, "cl_second.lua", PANEL_FILE);

            assert_eq!(
                (seen_type(&ws, in_first), seen_type(&ws, in_second)),
                ("unknown".to_string(), "DHTML".to_string()),
                "second_first = {second_first}"
            );
        }
    }

    fn applied_roots(configs: Vec<(&PathBuf, Emmyrc)>) -> HashMap<PathBuf, Arc<Emmyrc>> {
        configs
            .into_iter()
            .map(|(root, emmyrc)| (root.clone(), Arc::new(emmyrc)))
            .collect()
    }

    /// An edit to one root's config re-analyses that root's files and no
    /// other root's, and the result matches a root configured from the start.
    #[test]
    fn a_root_config_edit_reanalyses_only_that_roots_files() {
        let (mut ws, roots) = VirtualWorkspace::new_with_roots(&["first", "second"]);
        let (first, second) = (&roots[0], &roots[1]);
        ws.set_root_emmyrcs(vec![
            (first, Emmyrc::default()),
            (second, Emmyrc::default()),
        ]);
        define_dhtml(&mut ws, first);
        let in_first = ws.def_file_in(first, "cl_first.lua", PANEL_FILE);
        let in_second = ws.def_file_in(second, "cl_second.lua", PANEL_FILE);
        assert_eq!(
            (seen_type(&ws, in_first), seen_type(&ws, in_second)),
            ("unknown".to_string(), "unknown".to_string())
        );

        let shared = ws.analysis.get_emmyrc();
        let applied = ws.analysis.apply_configs(
            shared,
            &applied_roots(vec![
                (first, Emmyrc::default()),
                (second, with_panel_hint("DHTML")),
            ]),
        );
        let affected = ws.analysis.reindex_files(applied.files.clone());

        assert_eq!(applied.shared, EmmyrcChange::Unchanged);
        assert_eq!(applied.files, vec![in_second]);
        assert!(!affected.contains(&in_first));
        assert!(
            !ws.analysis
                .compilation
                .last_evaluated_files()
                .contains(&in_first)
        );
        assert_eq!(
            (seen_type(&ws, in_first), seen_type(&ws, in_second)),
            ("unknown".to_string(), "DHTML".to_string())
        );
    }

    /// A root's change that only affects its diagnostics re-analyses nothing,
    /// and neither does the root other than the config's baseline.
    #[test]
    fn a_root_diagnostics_edit_reanalyses_no_file() {
        let (mut ws, roots) = VirtualWorkspace::new_with_roots(&["first", "second"]);
        let (first, second) = (&roots[0], &roots[1]);
        ws.set_root_emmyrcs(vec![
            (first, Emmyrc::default()),
            (second, Emmyrc::default()),
        ]);
        ws.def_file_in(first, "cl_first.lua", PANEL_FILE);
        ws.def_file_in(second, "cl_second.lua", PANEL_FILE);
        let mut edited = Emmyrc::default();
        edited.diagnostics.disable.push(DiagnosticCode::Unused);

        let shared = ws.analysis.get_emmyrc();
        let applied = ws.analysis.apply_configs(
            shared,
            &applied_roots(vec![(first, Emmyrc::default()), (second, edited)]),
        );

        assert_eq!(applied.files, Vec::<FileId>::new());
    }

    /// A root's setting that a file is analysed with is not part of the
    /// config the roots share, so editing it re-analyses no other root.
    #[test]
    fn a_root_setting_stays_out_of_the_shared_change() {
        let (mut ws, roots) = VirtualWorkspace::new_with_roots(&["first", "second"]);
        let (first, second) = (&roots[0], &roots[1]);
        ws.set_root_emmyrcs(vec![
            (first, Emmyrc::default()),
            (second, Emmyrc::default()),
        ]);
        let in_first = ws.def_file_in(first, "cl_first.lua", PANEL_FILE);
        ws.def_file_in(second, "cl_second.lua", PANEL_FILE);

        let shared = Arc::new(with_panel_hint("DHTML"));
        let applied = ws.analysis.apply_configs(
            shared,
            &applied_roots(vec![
                (first, with_panel_hint("DHTML")),
                (second, Emmyrc::default()),
            ]),
        );

        assert_eq!(applied.shared, EmmyrcChange::Unchanged);
        assert_eq!(applied.files, vec![in_first]);
    }

    const ARRAY_FILE: &str = r#"
        ---@class Bag : string[]

        ---@type string[]
        local list = {}
        ---@type Bag
        local bag = {}

        local from_list = list[1]
        local from_bag = bag[1]

        ---@generic V
        ---@param t table<integer, V>
        ---@return V
        local function value_of(t) end
        local from_generic = value_of(bag)
    "#;

    fn with_array_index(array_index: bool) -> Emmyrc {
        let mut emmyrc = Emmyrc::default();
        emmyrc.strict.array_index = array_index;
        emmyrc
    }

    fn array_reads(ws: &VirtualWorkspace, file_id: FileId) -> Vec<String> {
        ["from_list", "from_bag", "from_generic"]
            .iter()
            .map(|name| ws.humanize_type(ws.local_ty(file_id, name)))
            .collect()
    }

    /// `strict.arrayIndex` of a root that is not the first one decides the
    /// element reads of its own files, and an edit to it re-reads them.
    #[test]
    fn a_later_roots_array_index_setting_decides_its_element_reads() {
        let (mut ws, roots) = VirtualWorkspace::new_with_roots(&["first", "second"]);
        let (first, second) = (&roots[0], &roots[1]);
        ws.set_root_emmyrcs(vec![
            (first, with_array_index(false)),
            (second, with_array_index(true)),
        ]);
        let in_first = ws.def_file_in(first, "cl_first.lua", ARRAY_FILE);
        let in_second = ws.def_file_in(second, "cl_second.lua", ARRAY_FILE);
        let strict = array_reads(&ws, in_first);
        assert_eq!(strict, ["string", "string", "string"]);
        let nilable = array_reads(&ws, in_second);
        assert_eq!(nilable, ["string?", "string?", "string?"]);

        let shared = ws.analysis.get_emmyrc();
        let applied = ws.analysis.apply_configs(
            shared,
            &applied_roots(vec![
                (first, with_array_index(false)),
                (second, with_array_index(false)),
            ]),
        );
        ws.analysis.reindex_files(applied.files.clone());

        assert_eq!(applied.files, vec![in_second]);
        assert_eq!(array_reads(&ws, in_second), strict);
    }
}
