#[cfg(test)]
mod test {
    use googletest::prelude::*;
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{DiagnosticCode, LuaType, LuaTypeDeclId, VirtualWorkspace};

    #[test]
    fn test_str_tpl_type() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class aaa.xxx.bbb

            ---@generic T
            ---@param a aaa.`T`.bbb
            ---@return T
            function get_type(a)
            end
            "#,
        );

        let string_ty = ws.expr_ty("get_type('xxx')");
        let expected = ws.ty("aaa.xxx.bbb");
        assert_eq!(string_ty, expected);
    }

    #[gtest]
    fn test_str_tpl_generic_returns_declared_type() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity
                ---@class sent_npc: Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end
            "#,
        );

        ws.def(
            r#"
                ent = ents.Create('sent_custom')
            "#,
        );

        let result_ty = ws.expr_ty("ents.Create('sent_npc')");
        let expected = ws.ty("sent_npc");
        assert_eq!(result_ty, expected);
    }

    #[gtest]
    fn test_str_tpl_generic_auto_creates_missing_class_from_constraint() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end
            "#,
        );

        let result_ty = ws.expr_ty("ents.Create('sent_custom')");
        let expected = ws.ty("sent_custom");
        assert_eq!(result_ty, expected);

        let super_types: Vec<_> = ws
            .get_db_mut()
            .get_type_index()
            .get_super_types_iter(&LuaTypeDeclId::global("sent_custom"))
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("Entity"))),
            "expected `sent_custom` to inherit `Entity`, got {super_types:?}"
        );
    }

    #[gtest]
    fn test_str_tpl_generic_undefined_and_defined_class_paths() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@generic T : Entity
                ---@param class `T`
                ---@return T
                function ents_Create(class) end

                ---@class Entity
                local Entity = {}

                ---@class Player : Entity
                local Player = {}

                ---@return Player
                function Player_func() end

                ---@class my_entity : Entity

            "#,
        );

        ws.def(
            r#"
                ent = ents_Create("prop_physics")
                ply = Player_func()
                my_ent = ents_Create("my_entity")
            "#,
        );

        assert_eq!(
            ws.expr_ty("ents_Create(\"prop_physics\")"),
            ws.ty("prop_physics")
        );
        assert_eq!(ws.expr_ty("Player_func()"), ws.ty("Player"));
        assert_eq!(ws.expr_ty("ents_Create(\"my_entity\")"), ws.ty("my_entity"));

        let super_types: Vec<_> = ws
            .get_db_mut()
            .get_type_index()
            .get_super_types_iter(&LuaTypeDeclId::global("prop_physics"))
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("Entity"))),
            "expected `prop_physics` to inherit `Entity`, got {super_types:?}"
        );
    }

    #[gtest]
    fn test_user_full_scenario_type_and_field_resolution() {
        let mut ws = VirtualWorkspace::new();

        ws.def_file(
            "annotations.lua",
            r#"
                ---@class Entity
                local Entity = {}

                ---@class Player : Entity
                local Player = {}

                ---@class Panel
                local Panel = {}

                ---@class DPanel : Panel
                local DPanel = {}

                ---@generic T : Entity
                ---@param class `T`
                ---@return T
                function ents_Create(class) end

                ---@generic T : Panel
                ---@param classname `T`
                ---@return T
                function vgui_Create(classname) end

                ---@param playerIndex number
                ---@return Player
                function Player_func(playerIndex) end
            "#,
        );

        ws.enable_check(DiagnosticCode::UndefinedField);
        let scenario_file_id = ws.def_file(
            "scenario.lua",
            r#"
                local tbl = {}
                tbl.testVar = true

                local ent = ents_Create("prop_physics")
                ent.testVar = true

                local row = vgui_Create("DPanel")
                row.testVar = true

                local ply = Player_func(1)
                ply.testVar = true

                scenario_tbl = tbl
                scenario_ent = ent
                scenario_row = row
                scenario_ply = ply

                scenario_tbl_test_var = tbl.testVar
                scenario_ent_test_var = ent.testVar
                scenario_row_test_var = row.testVar
                scenario_ply_test_var = ply.testVar
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(scenario_file_id, CancellationToken::new())
            .unwrap_or_default();
        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == undefined_field_code),
            "unexpected undefined-field diagnostics: {diagnostics:?}"
        );

        let tbl_type = ws.expr_ty("scenario_tbl");
        let table_type = ws.ty("table");
        assert!(ws.check_type(&tbl_type, &table_type));

        let ent_expected = ws.ty("prop_physics");
        let ent_type = ws.expr_ty("scenario_ent");
        assert_eq!(ent_type, ent_expected);

        let row_expected = ws.ty("DPanel");
        let row_type = ws.expr_ty("scenario_row");
        assert_eq!(row_type, row_expected);

        let ply_expected = ws.ty("Player");
        let ply_type = ws.expr_ty("scenario_ply");
        assert_eq!(ply_type, ply_expected);

        let bool_type = ws.ty("boolean");
        let tbl_field_type = ws.expr_ty("scenario_tbl_test_var");
        assert!(ws.check_type(&tbl_field_type, &bool_type));

        let ent_field_type = ws.expr_ty("scenario_ent_test_var");
        assert!(ws.check_type(&ent_field_type, &bool_type));

        let row_field_type = ws.expr_ty("scenario_row_test_var");
        assert!(ws.check_type(&row_field_type, &bool_type));

        let ply_field_type = ws.expr_ty("scenario_ply_test_var");
        assert!(ws.check_type(&ply_field_type, &bool_type));
    }
}
