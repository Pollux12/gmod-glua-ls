#[cfg(test)]
mod test {
    use googletest::prelude::*;

    use crate::VirtualWorkspace;

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

        let result_ty = ws.expr_ty("ents.Create('sent_npc')");
        let expected = ws.ty("sent_npc");
        assert_eq!(result_ty, expected);
    }

    #[gtest]
    fn test_str_tpl_generic_falls_back_to_constraint_type() {
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
        let expected = ws.ty("Entity");
        assert_eq!(result_ty, expected);
    }
}
