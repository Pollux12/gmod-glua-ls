#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, VirtualWorkspace};
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    const DUPLICATE_ANNOTATIONS: &str = r#"
        ---@meta

        ---@class Critter
        local Critter = {}

        ---@class Bird : Critter
        ---@field song string
        local Bird = {}

        ---@return boolean
        ---@return_cast self Bird
        function Critter:IsBird() end
    "#;

    const PROBE: &str = r#"
        ---@param c Critter
        local function guarded(c)
            if c:IsBird() then
                return c.song
            end
        end
        return guarded
    "#;

    fn workspace_with_libraries(libraries: &[&str]) -> VirtualWorkspace {
        let mut workspace = VirtualWorkspace::new();
        for library in libraries {
            workspace
                .analysis
                .add_library_workspace(workspace.virtual_url_generator.new_path(library));
        }
        workspace
    }

    fn diagnostic_count(
        workspace: &mut VirtualWorkspace,
        file_id: crate::FileId,
        code: DiagnosticCode,
    ) -> usize {
        workspace.analysis.diagnostic.enable_only(code);
        workspace
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .iter()
            .filter(|diagnostic| {
                diagnostic.code == Some(NumberOrString::String(code.get_name().to_string()))
            })
            .count()
    }

    fn define_duplicate_reproduction(workspace: &mut VirtualWorkspace) -> crate::FileId {
        workspace.def_file("lib_a/annotations.lua", DUPLICATE_ANNOTATIONS);
        workspace.def_file("lib_b/annotations.lua", DUPLICATE_ANNOTATIONS);
        workspace.def_file("probe.lua", PROBE)
    }

    #[test]
    fn duplicate_library_return_cast_uses_first_library_signature() {
        let mut workspace = workspace_with_libraries(&["lib_a", "lib_b"]);
        let probe_file = define_duplicate_reproduction(&mut workspace);

        assert_eq!(
            diagnostic_count(
                &mut workspace,
                probe_file,
                DiagnosticCode::InferUnguardedChild
            ),
            0
        );
    }

    #[test]
    fn single_library_return_cast_control_remains_clean() {
        let mut workspace = workspace_with_libraries(&["lib_a"]);
        workspace.def_file("lib_a/annotations.lua", DUPLICATE_ANNOTATIONS);
        let probe_file = workspace.def_file("probe.lua", PROBE);

        assert_eq!(
            diagnostic_count(
                &mut workspace,
                probe_file,
                DiagnosticCode::InferUnguardedChild
            ),
            0
        );
    }

    #[test]
    fn conflicting_return_cast_follows_library_registration_order() {
        const FIRST: &str = r#"
            ---@meta
            ---@class Critter
            local Critter = {}
            ---@class SingingBird : Critter
            ---@field song string
            local SingingBird = {}
            ---@return boolean
            ---@return_cast self SingingBird
            function Critter:IsKind() end
        "#;
        const SECOND: &str = r#"
            ---@meta
            ---@class Critter
            local Critter = {}
            ---@class QuietBird : Critter
            ---@field silence boolean
            local QuietBird = {}
            ---@return boolean
            ---@return_cast self QuietBird
            function Critter:IsKind() end
        "#;
        const CONSUMER: &str = r#"
            ---@param c Critter
            local function guarded(c)
                if c:IsKind() then
                    return c.song
                end
            end
            return guarded
        "#;

        let mut first_wins = workspace_with_libraries(&["lib_a", "lib_b"]);
        first_wins.def_file("lib_a/annotations.lua", FIRST);
        first_wins.def_file("lib_b/annotations.lua", SECOND);
        let first_probe = first_wins.def_file("probe.lua", CONSUMER);

        let mut second_wins = workspace_with_libraries(&["lib_b", "lib_a"]);
        second_wins.def_file("lib_a/annotations.lua", FIRST);
        second_wins.def_file("lib_b/annotations.lua", SECOND);
        let second_probe = second_wins.def_file("probe.lua", CONSUMER);

        assert_eq!(
            diagnostic_count(
                &mut first_wins,
                first_probe,
                DiagnosticCode::InferUnguardedChild
            ),
            0
        );
        assert_eq!(
            diagnostic_count(
                &mut second_wins,
                second_probe,
                DiagnosticCode::UndefinedField
            ),
            1
        );
    }

    #[test]
    fn later_library_still_contributes_unique_members() {
        let mut workspace = workspace_with_libraries(&["lib_a", "lib_b"]);
        workspace.def_file("lib_a/annotations.lua", DUPLICATE_ANNOTATIONS);
        workspace.def_file(
            "lib_b/extension.lua",
            r#"
            ---@meta
            ---@class (partial) Critter
            ---@field age integer
            "#,
        );
        let probe_file = workspace.def_file(
            "probe.lua",
            r#"
            ---@param c Critter
            local function read(c)
                return c.age
            end
            return read
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut workspace, probe_file, DiagnosticCode::UndefinedField),
            0
        );
    }

    #[test]
    fn collision_report_is_grouped_readable_and_deterministic() {
        let mut workspace = workspace_with_libraries(&["lib_a", "lib_b"]);
        define_duplicate_reproduction(&mut workspace);

        let collisions = workspace.analysis.library_definition_collisions();

        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].type_collisions, 2);
        assert_eq!(collisions[0].member_collisions, 2);
        assert_eq!(
            collisions[0].examples,
            vec![
                "Bird".to_string(),
                "Bird.song".to_string(),
                "Critter".to_string(),
                "Critter:IsBird".to_string(),
            ]
        );
        assert!(collisions[0].preferred_root.ends_with("lib_a"));
        assert!(collisions[0].shadowed_root.ends_with("lib_b"));
    }

    #[test]
    fn disjoint_partial_library_extensions_do_not_report_collisions() {
        let mut workspace = workspace_with_libraries(&["lib_a", "lib_b"]);
        workspace.def_file(
            "lib_a/extension.lua",
            r#"
            ---@meta
            ---@class (partial) SharedType
            ---@field first string
            "#,
        );
        workspace.def_file(
            "lib_b/extension.lua",
            r#"
            ---@meta
            ---@class (partial) SharedType
            ---@field second integer
            "#,
        );

        assert!(
            workspace
                .analysis
                .library_definition_collisions()
                .is_empty()
        );
    }

    #[test]
    fn collision_report_updates_after_delete_and_reopen() {
        let mut workspace = workspace_with_libraries(&["lib_a", "lib_b"]);
        let preferred_uri = workspace
            .virtual_url_generator
            .new_uri("lib_a/annotations.lua");
        let probe_file = define_duplicate_reproduction(&mut workspace);
        assert_eq!(workspace.analysis.library_definition_collisions().len(), 1);

        workspace
            .analysis
            .remove_file_by_uri(&preferred_uri)
            .expect("preferred library file should exist");
        assert!(
            workspace
                .analysis
                .library_definition_collisions()
                .is_empty()
        );
        assert_eq!(
            diagnostic_count(
                &mut workspace,
                probe_file,
                DiagnosticCode::InferUnguardedChild
            ),
            0
        );

        workspace
            .analysis
            .update_file_by_uri(&preferred_uri, Some(DUPLICATE_ANNOTATIONS.to_string()))
            .expect("preferred library file should reopen");
        assert_eq!(workspace.analysis.library_definition_collisions().len(), 1);
        assert_eq!(
            diagnostic_count(
                &mut workspace,
                probe_file,
                DiagnosticCode::InferUnguardedChild
            ),
            0
        );
    }

    #[test]
    fn collision_report_counts_aliases_attributes_fields_and_methods() {
        const DEFINITIONS: &str = r#"
            ---@meta
            ---@alias SharedAlias string
            ---@attribute shared_attribute(value: string)

            ---@class SharedClass
            ---@field value string
            local SharedClass = {}

            function SharedClass:Read() end
        "#;
        let mut workspace = workspace_with_libraries(&["lib_a", "lib_b"]);
        workspace.def_file("lib_a/annotations.lua", DEFINITIONS);
        workspace.def_file("lib_b/annotations.lua", DEFINITIONS);

        let collisions = workspace.analysis.library_definition_collisions();

        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].type_collisions, 3);
        assert_eq!(collisions[0].member_collisions, 2);
        assert!(collisions[0].examples.contains(&"SharedAlias".to_string()));
        assert!(
            collisions[0]
                .examples
                .contains(&"shared_attribute".to_string())
        );
        assert!(
            collisions[0]
                .examples
                .contains(&"SharedClass.value".to_string())
        );
        assert!(
            collisions[0]
                .examples
                .contains(&"SharedClass:Read".to_string())
        );
    }

    #[test]
    fn duplicate_registration_of_one_normalized_root_is_not_a_collision() {
        let mut workspace = VirtualWorkspace::new();
        let root = workspace.virtual_url_generator.new_path("lib_a");
        workspace.analysis.add_library_workspace(root.clone());
        workspace.analysis.add_library_workspace(root);
        workspace.def_file("lib_a/annotations.lua", DUPLICATE_ANNOTATIONS);

        assert!(
            workspace
                .analysis
                .library_definition_collisions()
                .is_empty()
        );
    }

    #[test]
    fn warning_message_contains_precedence_counts_and_examples() {
        let mut workspace = workspace_with_libraries(&["lib_a", "lib_b"]);
        define_duplicate_reproduction(&mut workspace);
        let collision = workspace
            .analysis
            .library_definition_collisions()
            .into_iter()
            .next()
            .expect("collision");

        let warning = collision.warning_message();

        assert!(
            warning
                .contains("The earlier entry in .gluarc.json (workspace.library) takes priority")
        );
        assert!(warning.contains("2 types, 2 members"));
        assert!(warning.contains("Critter:IsBird"));
    }
}
