#[cfg(test)]
mod test {
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{DiagnosticCode, FileId, VirtualWorkspace};

    const ANNOTATIONS: &str = r#"
        ---@meta
        ---@class GM
        GM = {}

        ---@type GM
        GAMEMODE = nil
    "#;

    const BASE_METHOD: &str = r#"
        function GM:Test18BaseMethod()
            return "base"
        end
    "#;

    const CALLER: &str = r#"
        function GM:Test18Caller()
            return self:Test18BaseMethod()
        end

        hook.Add("InitPostEntity", "Test18", function()
            GAMEMODE:Test18BaseMethod()
        end)
    "#;

    /// A gamemode whose manifest names `test_base`, the base opened as a main
    /// workspace or loaded as a library, and the file that calls the base's
    /// `GM:` method.
    fn derived_gamemode(base_is_library: bool) -> (VirtualWorkspace, FileId) {
        let (mut ws, roots) = if base_is_library {
            VirtualWorkspace::new_with_root_paths(&["glua_gm_test/gamemodes/test"])
        } else {
            VirtualWorkspace::new_with_root_paths(&[
                "glua_gm_test/gamemodes/test_base",
                "glua_gm_test/gamemodes/test",
            ])
        };
        let gamemodes = roots[0].parent().expect("gamemodes folder").to_path_buf();
        let test_base = gamemodes.join("test_base");
        let test = gamemodes.join("test");
        if base_is_library {
            ws.analysis.add_library_workspace(test_base.clone());
        }
        let annotations = gamemodes.parent().expect("test folder").join("annotations");
        ws.analysis.add_library_workspace(annotations.clone());
        let mut emmyrc = ws.get_emmyrc();
        emmyrc
            .gmod
            .gamemode_bases
            .insert("test".to_string(), "test_base".to_string());
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedMethod);
        ws.enable_check(DiagnosticCode::UndefinedField);

        ws.def_file_in(&annotations, "gm.lua", ANNOTATIONS);
        ws.def_file_in(&test_base, "gamemode/cl_init.lua", BASE_METHOD);
        ws.def_file_in(&test, "gamemode/cl_init.lua", "");
        let caller = ws.def_file_in(&test, "gamemode/cl_test18.lua", CALLER);
        (ws, caller)
    }

    fn undefined_member_diagnostics(ws: &VirtualWorkspace, file_id: FileId) -> Vec<String> {
        let codes = [
            NumberOrString::String(DiagnosticCode::UndefinedMethod.get_name().to_string()),
            NumberOrString::String(DiagnosticCode::UndefinedField.get_name().to_string()),
        ];
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diag| diag.code.as_ref().is_some_and(|code| codes.contains(code)))
            .map(|diag| diag.message)
            .collect()
    }

    /// The base gamemode's `GM:` methods reach the derived gamemode's `self`
    /// and `GAMEMODE` when the derived one opens the base as a workspace.
    #[test]
    fn a_base_gamemode_method_resolves_in_the_derived_gamemode() {
        let (ws, caller) = derived_gamemode(false);

        assert_eq!(
            undefined_member_diagnostics(&ws, caller),
            Vec::<String>::new()
        );
    }

    /// The same when the base gamemode is loaded as a library.
    #[test]
    fn a_library_base_gamemode_method_resolves_in_the_derived_gamemode() {
        let (ws, caller) = derived_gamemode(true);

        assert_eq!(
            undefined_member_diagnostics(&ws, caller),
            Vec::<String>::new()
        );
    }

    /// Editing the manifest's base, or deleting the manifest, moves the link
    /// with it; restoring the manifest brings it back.
    #[test]
    fn the_link_follows_the_manifest_through_edit_delete_and_restore() {
        let (mut ws, caller) = derived_gamemode(false);
        let with_base = ws.get_emmyrc();
        let mut without_base = with_base.clone();
        without_base.gmod.gamemode_bases.clear();
        let mut other_base = with_base.clone();
        other_base
            .gmod
            .gamemode_bases
            .insert("test".to_string(), "elsewhere".to_string());

        let mut seen = Vec::new();
        for emmyrc in [without_base, with_base, other_base] {
            ws.update_emmyrc(emmyrc);
            seen.push(undefined_member_diagnostics(&ws, caller).len());
        }

        assert_eq!(seen, [2, 0, 2]);
    }
}
