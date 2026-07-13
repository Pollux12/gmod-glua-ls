#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};

    #[test]
    fn test_shadowed_library_accessor_func_field_is_not_duplicate() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        let library_root = ws.virtual_url_generator.base.join("library");
        ws.analysis.add_library_workspace(library_root);
        ws.def_file(
            "library/gamemodes/terrortown/gamemode/cl_init.lua",
            r#"include("vgui/simpleicon.lua")"#,
        );
        ws.def_file(
            "library/gamemodes/terrortown/gamemode/vgui/simpleicon.lua",
            r#"
            local PANEL = {}
            AccessorFunc(PANEL, "m_iIconSize", "IconSize")
            vgui.Register("SimpleIcon", PANEL, "Panel")
            "#,
        );
        ws.def_file(
            "gamemodes/terrortown/gamemode/cl_init.lua",
            r#"include("vgui/simpleicon.lua")"#,
        );

        assert!(ws.check_file_for(
            DiagnosticCode::DuplicateDocField,
            "gamemodes/terrortown/gamemode/vgui/simpleicon.lua",
            r#"
            local PANEL = {}
            AccessorFunc(PANEL, "m_iIconSize", "IconSize")
            vgui.Register("SimpleIcon", PANEL, "Panel")
            "#,
        ));
    }

    #[test]
    fn test_duplicate_field() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::DuplicateDocField,
            r#"
            ---@class Test
            ---@field name string
            ---@field name string
            local Test = {}

            Test.name = 1
            "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::DuplicateDocField,
            r#"
            ---@class Test
            ---@field name string
            ---@field age number
            local Test = {}
            "#
        ));

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::DuplicateDocField,
            r#"
            ---@class Test
            ---@field name string
            ---@field name number
            local Test = {}
            "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::DuplicateDocField,
            r#"
            ---@class Test1
            ---@field name string

            ---@class Test2
            ---@field name string
            "#
        ));
    }

    #[test]
    fn test_duplicate_function_1() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::DuplicateDocField,
            r#"
            ---@class Test
            ---@field a fun()
            local Test = {}

            function Test.a()
            end
            "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::DuplicateDocField,
            r#"
            ---@class Test
            ---@field a fun()
            ---@field a fun()
            local Test = {}

            function Test.a()
            end
            "#
        ));

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::DuplicateSetField,
            r#"
            ---@class Test
            ---@field a fun()
            local Test = {}

            function Test.a()
            end

            function Test.a()
            end
            "#
        ));
    }

    // remove this test
    #[test]
    fn test_duplicate_function_2() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "1.lua",
            r#"
                ---@class D31.A
                local A = {}

                ---@param ... any
                ---@return any, any, any, any
                function A:execute(...)
                end

                return A
            "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::DuplicateSetField,
            r#"
            local A = require("1")

            A.execute = function(trg, ...)
            end
        "#
        ));
    }

    #[test]
    fn test_duplicate_function_3() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::DuplicateSetField,
            r#"
                ---@class D31.A
                local A = {}
                A.a = function() end

                function A:init()
                    self.a = function()
                    end
                end
        "#
        ));
    }

    #[test]
    fn test_duplicate_function_4() {
        let mut ws = VirtualWorkspace::new();
        // 如果是 .member = 参数, 则不报错
        assert!(ws.check_code_for(
            DiagnosticCode::DuplicateSetField,
            r#"
                ---@class D31.A
                local A = {}
                A.a = function() end

                ---@param a fun()
                function A:init(a)
                    self.a = a
                end
        "#
        ));
    }

    #[test]
    fn test_return_self() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::DuplicateSetField,
            r#"
                ---@class test
                local A

                ---@return self
                function A.new()
                end

                function A:stop()
                end

                local a = A.new()

                a.stop = function()

                end
        "#
        ));
    }
}
