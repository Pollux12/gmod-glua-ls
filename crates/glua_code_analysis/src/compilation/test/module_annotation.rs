#[cfg(test)]
mod test {
    use crate::VirtualWorkspace;

    #[test]
    fn test_module_annotation() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def_files(vec![(
            "a.lua",
            r#"
                local a = {
                }
                return a
                "#,
        )]);

        ws.def(
            r#"
            ---@module "a"
            aaa = {}
            "#,
        );

        let aaa_ty = ws.expr_ty("aaa");
        assert!(aaa_ty.is_module_ref());
    }

    #[test]
    fn module_annotation_overrides_implicit_class_owner_type() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def_files(vec![(
            "a.lua",
            r#"
                return {}
                "#,
        )]);

        ws.def(
            r#"
            ---@class LocalShape
            ---@module "a"
            value = {}
            "#,
        );

        let value_ty = ws.expr_ty("value");
        assert!(value_ty.is_module_ref());
        assert_ne!(value_ty, ws.ty("LocalShape"));
    }
}
