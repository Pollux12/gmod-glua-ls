#[cfg(test)]
mod test {
    use std::sync::Arc;

    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{DiagnosticCode, Emmyrc, EmmyrcLuaVersion, VirtualWorkspace};

    fn has_undefined_global_name(
        ws: &mut VirtualWorkspace,
        file_path: &str,
        content: &str,
        name: &str,
    ) -> bool {
        let file_id = ws.def_file(file_path, content);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedGlobal.get_name().to_string(),
        ));
        let message_needled = format!("undefined global variable: {name}");

        diagnostics.iter().any(|diagnostic| {
            diagnostic.code == code && diagnostic.message.contains(&message_needled)
        })
    }

    #[test]
    fn legacy_module_seeall_allows_global_fallback() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        assert!(!has_undefined_global_name(
            &mut ws,
            "class.lua",
            r#"
            module("class", package.seeall)
            local _ = print
            "#,
            "print",
        ));
    }

    #[test]
    fn legacy_module_without_seeall_reports_undefined_global() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        assert!(has_undefined_global_name(
            &mut ws,
            "class.lua",
            r#"
            module("class")
            local _ = print
            "#,
            "print",
        ));
    }

    #[test]
    fn legacy_module_does_not_leak_bare_name_to_other_files() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "class.lua",
            r#"
            module("class", package.seeall)
            function Create() end
            "#,
        );
        assert!(has_undefined_global_name(
            &mut ws,
            "consumer.lua",
            r#"
            Create()
            "#,
            "Create",
        ));
    }

    #[test]
    fn legacy_module_namespace_does_not_leak_across_main_workspaces() {
        let mut analysis = crate::EmmyLuaAnalysis::new();
        analysis.init_std_lib(None);
        let mut emmyrc = Emmyrc::default();
        emmyrc.workspace.enable_isolation = true;
        analysis.update_config(Arc::new(emmyrc));

        let workspace_a = std::env::temp_dir().join("legacy-module-workspace-a");
        let workspace_b = std::env::temp_dir().join("legacy-module-workspace-b");
        analysis.add_main_workspace(workspace_a.clone());
        analysis.add_main_workspace(workspace_b.clone());

        let file_a = lsp_types::Uri::parse_from_file_path(&workspace_a.join("class.lua")).unwrap();
        let file_b =
            lsp_types::Uri::parse_from_file_path(&workspace_b.join("consumer.lua")).unwrap();

        analysis.update_file_by_uri(
            &file_a,
            Some(
                r#"
                module("class", package.seeall)
                function Create() end
                "#
                .to_string(),
            ),
        );
        let file_id_b = analysis
            .update_file_by_uri(
                &file_b,
                Some(
                    r#"
                    local c = class.Create
                    "#
                    .to_string(),
                ),
            )
            .expect("consumer file id");

        analysis
            .diagnostic
            .enable_only(crate::DiagnosticCode::UndefinedGlobal);
        let diagnostics = analysis
            .diagnose_file(file_id_b, tokio_util::sync::CancellationToken::new())
            .unwrap_or_default();

        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("undefined global variable: class")
        }));
    }

    #[test]
    fn legacy_module_implicit_fields_are_visible() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        ws.update_emmyrc(emmyrc);

        let content = r#"
            module("class.sub")
            local a = _M
            local b = _NAME
            local c = _PACKAGE
            "#;

        assert!(!has_undefined_global_name(
            &mut ws,
            "class.lua",
            content,
            "_M"
        ));
        assert!(!has_undefined_global_name(
            &mut ws,
            "class.lua",
            content,
            "_NAME"
        ));
        assert!(!has_undefined_global_name(
            &mut ws,
            "class.lua",
            content,
            "_PACKAGE"
        ));
    }

    #[test]
    fn legacy_module_without_seeall_respects_undefined_global_allowlist() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        emmyrc.diagnostics.globals = vec!["print".into()];
        ws.update_emmyrc(emmyrc);

        assert!(!has_undefined_global_name(
            &mut ws,
            "class.lua",
            r#"
            module("class")
            local _ = print
            "#,
            "print",
        ));
    }
}
