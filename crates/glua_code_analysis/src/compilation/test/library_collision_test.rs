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

    /// `lib_a` is the earlier configured library, so its declaration must own
    /// the `Choice` alias: `Choice<string, integer>` resolves to `string`.
    const ALIAS_FIRST: &str = r#"
        ---@meta
        ---@alias Choice<A, B> A
    "#;

    /// Same arity with swapped formal names; if the shadowed declaration wins,
    /// `Choice<string, integer>` resolves to `integer` instead of `string`.
    const ALIAS_SECOND: &str = r#"
        ---@meta
        ---@alias Choice<B, A> A
    "#;

    /// Conflicting generic constraints: the earlier library constrains `T` to
    /// `string`, the shadowed one to `integer`. Because template params are
    /// positional, this independently proves the winning declaration's
    /// constraint vector is the one consumers are checked against.
    const CONSTRAINED_ALIAS_FIRST: &str = r#"
        ---@meta
        ---@alias Choice<T: string> T
    "#;

    const CONSTRAINED_ALIAS_SECOND: &str = r#"
        ---@meta
        ---@alias Choice<T: integer> T
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
            .0
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
            .map(|(id, _)| id)
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

    #[test]
    fn earlier_library_alias_wins_regardless_of_ingestion_order() {
        let mut defined_first = workspace_with_libraries(&["lib_a", "lib_b"]);
        defined_first.def_file("lib_a/annotations.lua", ALIAS_FIRST);
        defined_first.def_file("lib_b/annotations.lua", ALIAS_SECOND);

        let mut defined_second = workspace_with_libraries(&["lib_a", "lib_b"]);
        defined_second.def_file("lib_b/annotations.lua", ALIAS_SECOND);
        defined_second.def_file("lib_a/annotations.lua", ALIAS_FIRST);

        for workspace in [&mut defined_first, &mut defined_second] {
            let consumer = workspace.ty("Choice<string, integer>");
            assert!(
                workspace.check_type(&consumer, &crate::LuaType::String),
                "earlier library alias must resolve Choice<string, integer> to string"
            );
            assert!(
                !workspace.check_type(&consumer, &crate::LuaType::Integer),
                "shadowed library alias must not resolve Choice<string, integer> to integer"
            );
        }
    }

    #[test]
    fn alias_winner_survives_delete_and_reopen_lifecycle() {
        let mut workspace = workspace_with_libraries(&["lib_a", "lib_b"]);
        let winner_uri = workspace
            .virtual_url_generator
            .new_uri("lib_a/annotations.lua");
        workspace.def_file("lib_b/annotations.lua", ALIAS_SECOND);
        workspace.def_file("lib_a/annotations.lua", ALIAS_FIRST);

        let consumer = workspace.ty("Choice<string, integer>");
        assert!(workspace.check_type(&consumer, &crate::LuaType::String));

        // Removing the winning declaration exposes the shadowed survivor.
        workspace
            .analysis
            .remove_file_by_uri(&winner_uri)
            .0
            .expect("winner library file should exist");
        let survivor = workspace.ty("Choice<string, integer>");
        assert!(
            workspace.check_type(&survivor, &crate::LuaType::Integer),
            "surviving library alias must resolve Choice<string, integer> to integer"
        );

        // Reopening restores the earlier configured library as the winner.
        workspace
            .analysis
            .update_file_by_uri(&winner_uri, Some(ALIAS_FIRST.to_string()))
            .map(|(id, _)| id)
            .expect("winner library file should reopen");
        let restored = workspace.ty("Choice<string, integer>");
        assert!(
            workspace.check_type(&restored, &crate::LuaType::String),
            "reopened earlier library alias must resolve Choice<string, integer> to string"
        );
    }

    #[test]
    fn alias_declarations_keep_their_registration_namespace() {
        let mut workspace = VirtualWorkspace::new();
        workspace.def_file(
            "lua/autorun/shared/namespaces.lua",
            "---@alias Root string\n---@namespace First\n---@alias Value integer\n---@namespace Second\n---@alias Value boolean",
        );
        for (name, expected) in [
            ("Root", crate::LuaType::String),
            ("First.Value", crate::LuaType::Integer),
            ("Second.Value", crate::LuaType::Boolean),
        ] {
            let typ = workspace.ty(name);
            let db = workspace.analysis.compilation.get_db();
            assert_eq!(crate::resolve_alias_type(db, &typ).typ, expected, "{name}");
        }
    }

    #[test]
    fn using_namespace_does_not_redirect_alias_declaration() {
        let mut workspace = VirtualWorkspace::new();
        workspace.def_file(
            "lua/autorun/shared/imported.lua",
            "---@namespace Imported\n---@alias Value integer",
        );
        workspace.def_file(
            "lua/autorun/shared/own.lua",
            "---@using Imported\n---@alias Value string",
        );
        let imported = workspace.ty("Imported.Value");
        let own = workspace.ty("Value");
        let db = workspace.analysis.compilation.get_db();
        assert_eq!(
            crate::resolve_alias_type(db, &imported).typ,
            crate::LuaType::Integer
        );
        assert_eq!(
            crate::resolve_alias_type(db, &own).typ,
            crate::LuaType::String
        );
    }

    #[test]
    fn earlier_library_alias_constraint_governs_consumer_checks() {
        let mut workspace = workspace_with_libraries(&["lib_a", "lib_b"]);
        workspace.def_file("lib_a/annotations.lua", CONSTRAINED_ALIAS_FIRST);
        workspace.def_file("lib_b/annotations.lua", CONSTRAINED_ALIAS_SECOND);

        let satisfied = workspace.def_file(
            "consumer_ok.lua",
            r#"
            ---@type Choice<string>
            local ok
            "#,
        );
        let violated = workspace.def_file(
            "consumer_bad.lua",
            r#"
            ---@type Choice<integer>
            local bad
            "#,
        );

        assert_eq!(
            diagnostic_count(
                &mut workspace,
                satisfied,
                DiagnosticCode::GenericConstraintMismatch
            ),
            0,
            "Choice<string> must satisfy the earlier library's `T: string` constraint"
        );
        assert_eq!(
            diagnostic_count(
                &mut workspace,
                violated,
                DiagnosticCode::GenericConstraintMismatch
            ),
            1,
            "Choice<integer> must violate the earlier library's `T: string` constraint"
        );
    }

    /// Annotations load, and elect writes, ahead of the workspace, as the std
    /// lib does, wherever the two sit on disk: here the library's path sorts
    /// after the workspace's files.
    #[test]
    fn an_annotation_file_orders_before_every_workspace_file_wherever_it_lives() {
        let mut ws = VirtualWorkspace::new();
        let library_root = ws.virtual_url_generator.new_path("zz_annotations");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("gm.lua")).unwrap();
        let (library_file, _) = ws
            .analysis
            .update_file_by_uri(
                &library_uri,
                Some(
                    "---@class GM
"
                    .to_string(),
                ),
            )
            .expect("the library file is indexed");
        let workspace_file = ws.def_file(
            "gamemode/core/sv_hooks.lua",
            "local hooks = {}
",
        );

        let vfs = ws.analysis.compilation.get_db().get_vfs();
        assert!(vfs.file_order_key(&library_file) < vfs.file_order_key(&workspace_file));
    }
}
