#[cfg(test)]
mod test {

    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn test_table() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        table.concat({'', ''}, ' ')
        "#
        ));
    }

    #[test]
    fn test_sub_string() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::MissingParameter,
            r#"
        local t = ("m2"):sub(1)
        "#
        ));
    }

    #[test]
    fn test_class_default_constructor() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"

            ---@generic T
            ---@[constructor("__init")]
            ---@param name `T`
            ---@return T
            function meta(name)
            end
        "#,
        );

        ws.def(
            r#"
        ---@class MyClass
        local M = meta("MyClass")

        function M:__init()
        end

        A = M()
        "#,
        );

        let ty = ws.expr_ty("A");
        let expected = ws.ty("MyClass");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_class_default_constructor_via_local_alias() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"

            ---@generic T
            ---@[constructor("__init")]
            ---@param name `T`
            ---@return T
            function meta(name)
            end
        "#,
        );

        ws.def(
            r#"
        local meta_alias = meta

        ---@class MyAliasClass
        local M = meta_alias("MyAliasClass")

        function M:__init()
        end

        A = M()
        "#,
        );

        let ty = ws.expr_ty("A");
        let expected = ws.ty("MyAliasClass");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_class_default_constructor_via_member_alias() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"

            ---@generic T
            ---@[constructor("__init")]
            ---@param name `T`
            ---@return T
            function meta(name)
            end
        "#,
        );

        ws.def(
            r#"
        local registry = {}
        registry.new = meta

        ---@class MyMemberAliasClass
        local M = registry.new("MyMemberAliasClass")

        function M:__init()
        end

        A = M()
        "#,
        );

        let ty = ws.expr_ty("A");
        let expected = ws.ty("MyMemberAliasClass");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_class_default_constructor_via_member_read_alias() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"

            ---@generic T
            ---@[constructor("__init")]
            ---@param name `T`
            ---@return T
            function meta(name)
            end
        "#,
        );

        ws.def(
            r#"
        local registry = {}
        registry.new = meta
        local ctor = registry.new

        ---@class MyMemberReadAliasClass
        local M = ctor("MyMemberReadAliasClass")

        function M:__init()
        end

        A = M()
        "#,
        );

        let ty = ws.expr_ty("A");
        let expected = ws.ty("MyMemberReadAliasClass");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_class_default_constructor_via_cross_file_local_alias() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_files(vec![
            (
                "a_use.lua",
                r#"
                local meta_alias = meta

                ---@class MyCrossFileAliasClass
                local M = meta_alias("MyCrossFileAliasClass")

                function M:__init()
                end

                A = M()
                "#,
            ),
            (
                "z_defs.lua",
                r#"

                ---@generic T
                ---@[constructor("__init")]
                ---@param name `T`
                ---@return T
                function meta(name)
                end
            "#,
            ),
        ]);

        let ty = ws.expr_ty("A");
        let expected = ws.ty("MyCrossFileAliasClass");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_class_default_constructor_via_wrapped_call_operator() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"

            ---@generic T
            ---@[constructor("__init")]
            ---@param name `T`
            ---@return T
            function meta(name)
            end
        "#,
        );

        ws.def(
            r#"
        local wrapper = setmetatable({}, { __call = meta })

        ---@class MyWrappedClass
        local M = wrapper("MyWrappedClass")

        function M:__init()
        end

        A = M()
        "#,
        );

        let ty = ws.expr_ty("A");
        let expected = ws.ty("MyWrappedClass");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_class_default_constructor_via_wrapped_member_read_alias() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"

            ---@generic T
            ---@[constructor("__init")]
            ---@param name `T`
            ---@return T
            function meta(name)
            end
        "#,
        );

        ws.def(
            r#"
        local registry = {}
        registry.new = meta
        local ctor = registry.new
        local wrapper = setmetatable({}, { __call = ctor })

        ---@class MyWrappedMemberReadClass
        local M = wrapper("MyWrappedMemberReadClass")

        function M:__init()
        end

        A = M()
        "#,
        );

        let ty = ws.expr_ty("A");
        let expected = ws.ty("MyWrappedMemberReadClass");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_class_default_constructor_via_member_backed_wrapped_call_operator() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"

            ---@generic T
            ---@[constructor("__init")]
            ---@param name `T`
            ---@return T
            function meta(name)
            end
        "#,
        );

        ws.def(
            r#"
        local registry = {}
        registry.wrapper = setmetatable({}, { __call = meta })

        ---@class MyMemberWrappedClass
        local M = registry.wrapper("MyMemberWrappedClass")

        function M:__init()
        end

        A = M()
        "#,
        );

        let ty = ws.expr_ty("A");
        let expected = ws.ty("MyMemberWrappedClass");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_workspace_scoped_direct_matcher_does_not_leak_library_name() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let library_root = ws.virtual_url_generator.base.join("library");
        ws.analysis.add_library_workspace(library_root);

        ws.def_file(
            "library/meta.lua",
            r#"
            ---@generic T
            ---@[constructor("__init")]
            ---@param name `T`
            ---@return T
            function meta(name)
            end
            "#,
        );

        ws.def_file(
            "virtual_0.lua",
            r#"
            if false then
                local _ = meta
            end
            "#,
        );

        ws.def(
            r#"
        ---@class PlainLocal
        local meta = function(name)
            return {}
        end

        local M = meta("PlainLocal")
        A = M()
        "#,
        );

        let ty = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(ty), "any");
    }

    #[test]
    fn test_workspace_scoped_global_shadow_does_not_leak_library_name() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let library_root = ws.virtual_url_generator.base.join("library");
        ws.analysis.add_library_workspace(library_root);

        ws.def_file(
            "library/meta.lua",
            r#"
            ---@generic T
            ---@[constructor("__init")]
            ---@param name `T`
            ---@return T
            function meta(name)
            end
            "#,
        );

        ws.def_files(vec![
            (
                "a_use.lua",
                r#"
                local M = meta("PlainGlobal")
                A = M()
                "#,
            ),
            (
                "z_defs.lua",
                r#"
                ---@class PlainGlobal
                function meta(name)
                    return {}
                end
                "#,
            ),
        ]);

        let ty = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(ty), "unknown");
    }

    #[test]
    fn test_wrapped_local_shadow_does_not_leak_library_special_call() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let library_root = ws.virtual_url_generator.base.join("library");
        ws.analysis.add_library_workspace(library_root);

        ws.def_file(
            "library/meta.lua",
            r#"
            ---@generic T
            ---@[constructor("__init")]
            ---@param name `T`
            ---@return T
            function meta(name)
            end
            "#,
        );

        ws.def(
            r#"
        ---@class PlainWrappedLocal
        local meta = function(name)
            return {}
        end

        local wrapper = setmetatable({}, { __call = meta })
        local M = wrapper("PlainWrappedLocal")
        A = M()
        "#,
        );

        let ty = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(ty), "any");
    }

    #[test]
    fn test_workspace_scoped_member_shadow_does_not_leak_library_special_call() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let library_root = ws.virtual_url_generator.base.join("library");
        ws.analysis.add_library_workspace(library_root);

        ws.def_file(
            "library/registry.lua",
            r#"
            registry = {}

            ---@generic T
            ---@[constructor("__init")]
            ---@param name `T`
            ---@return T
            function registry.new(name)
            end
            "#,
        );

        ws.def_files(vec![
            (
                "a_use.lua",
                r#"
                local M = registry.new("PlainMember")
                A = M()
                "#,
            ),
            (
                "z_defs.lua",
                r#"
                ---@class PlainMember
                registry = {}

                function registry.new(name)
                    return {}
                end
                "#,
            ),
        ]);

        let ty = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(ty), "unknown");
    }

    #[test]
    fn test_wrapped_constructor_respects_branch_realm() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def_file(
            "addons/test/lua/autorun/sh_meta.lua",
            r#"
            if SERVER then
                ---@generic T
                ---@[constructor("__init")]
                ---@param name `T`
                ---@return T
                function meta(name)
                end
            end

            if CLIENT then
                local wrapper = setmetatable({}, { __call = meta })
                local M = wrapper("ClientBranchCtor")
                A = M()
            end
            "#,
        );

        let ty = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(ty), "any");
    }

    #[test]
    fn test_wrapped_constructor_allows_same_branch_realm() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def_file(
            "addons/test/lua/autorun/client/cl_meta.lua",
            r#"
            if CLIENT then
                ---@generic T
                ---@[constructor("__init")]
                ---@param name `T`
                ---@return T
                function meta(name)
                end

                local wrapper = setmetatable({}, { __call = meta })

                ---@class ClientBranchCtor
                local M = wrapper("ClientBranchCtor")

                function M:__init()
                end

                A = M()
            end
            "#,
        );

        let ty = ws.expr_ty("A");
        let expected = ws.ty("ClientBranchCtor");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_wrapped_constructor_cycle_does_not_infer_special_call() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        local a, b
        a = setmetatable({}, { __call = b })
        b = setmetatable({}, { __call = a })

        local M = a("CycleCtor")
        A = M()
        "#,
        );

        let ty = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(ty), "unknown");
    }

    #[test]
    fn test_issue_770() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
        local table = {1,2}
        if next(table, 2) == '2' then
            print('ok')
        end
        "#
        ));
    }
}
