#[cfg(test)]
mod test {
    use googletest::prelude::*;

    use crate::{LuaType, LuaTypeDeclId, VirtualWorkspace};

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
}
