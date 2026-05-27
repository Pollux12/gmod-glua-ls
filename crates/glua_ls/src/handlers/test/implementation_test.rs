#[cfg(test)]
mod tests {
    use crate::handlers::test_lib::{ProviderVirtualWorkspace, VirtualLocation, check};
    use glua_code_analysis::Emmyrc;
    use googletest::prelude::*;

    #[gtest]
    fn test_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "2.lua",
            r#"
               delete = require("virtual_0").delete
               delete()
            "#,
        );
        ws.def_file(
            "3.lua",
            r#"
               delete = require("virtual_0").delete
               delete()
            "#,
        );
        check!(ws.check_implementation(
            r#"
                local M = {}
                function M.de<??>lete(a)
                end
                return M
            "#,
            vec![VirtualLocation {
                file: "".to_string(),
                line: 2,
            }],
        ));
        Ok(())
    }

    #[gtest]
    fn test_2() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "1.lua",
            r#"
                ---@class (partial) Test
                test = {}

                test.a = 1
            "#,
        );
        ws.def_file(
            "2.lua",
            r#"
                ---@class (partial) Test
                test = {}
                test.a = 1
            "#,
        );
        ws.def_file(
            "3.lua",
            r#"
                local a = test.a
            "#,
        );
        check!(ws.check_implementation(
            r#"
                t<??>est
            "#,
            vec![
                VirtualLocation {
                    file: "".to_string(),
                    line: 1,
                },
                VirtualLocation {
                    file: "1.lua".to_string(),
                    line: 2,
                },
                VirtualLocation {
                    file: "2.lua".to_string(),
                    line: 2,
                }
            ],
        ));
        Ok(())
    }

    #[gtest]
    fn test_3() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "1.lua",
            r#"
                ---@class YYY
                ---@field a number
                yyy = {}

                if false then
                    yyy.a = 1
                    if yyy.a then
                    end
                end
            "#,
        );
        check!(ws.check_implementation(
            r#"
                yyy.<??>a = 2
            "#,
            vec![
                VirtualLocation {
                    file: "".to_string(),
                    line: 1,
                },
                VirtualLocation {
                    file: "1.lua".to_string(),
                    line: 2,
                },
                VirtualLocation {
                    file: "1.lua".to_string(),
                    line: 6,
                },
            ],
        ));
        Ok(())
    }

    #[gtest]
    fn test_table_field_definition_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_implementation(
            r#"
                ---@class T
                ---@field func fun(self: T) 注释注释

                ---@type T
                local t = {
                    func = function(self)
                    end,
                }

                t:fun<??>c()
            "#,
            vec![
                VirtualLocation {
                    file: "".to_string(),
                    line: 2,
                },
                VirtualLocation {
                    file: "".to_string(),
                    line: 6,
                },
            ],
        ));
        Ok(())
    }

    #[gtest]
    fn test_table_field_definition_2() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_implementation(
            r#"
                ---@class T
                ---@field func fun(self: T) 注释注释

                ---@type T
                local t = {
                    f<??>unc = function(self)
                    end,
                }
            "#,
            vec![
                VirtualLocation {
                    file: "".to_string(),
                    line: 2,
                },
                VirtualLocation {
                    file: "".to_string(),
                    line: 6,
                },
            ],
        ));
        Ok(())
    }

    #[gtest]
    fn test_dynamic_key_table_field_implementation() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.analysis.update_config(emmyrc.into());

        check!(ws.check_implementation(
            r#"
                local rec = { data = {} }
                local data = rec.data
                for key, value in pairs({ forwardSlip = 1, sideSlip = 2 }) do
                    data[key] = value
                end
                local d = rec.data
                math.abs(d.forw<??>ardSlip or 0)
            "#,
            vec![VirtualLocation {
                file: "".to_string(),
                line: 4,
            }],
        ));
        Ok(())
    }

    #[gtest]
    fn test_separation_of_define_and_impl() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_implementation(
            r#"
                local a<??>bc

                abc = function()
                end

                local _a = abc
                local _b = abc()

                abc = function()
                end
            "#,
            vec![
                VirtualLocation {
                    file: "".to_string(),
                    line: 1,
                },
                VirtualLocation {
                    file: "".to_string(),
                    line: 3,
                },
                VirtualLocation {
                    file: "".to_string(),
                    line: 9,
                },
            ],
        ));
        Ok(())
    }

    #[gtest]
    fn test_member_variable_implementation_with_ref_prefix() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_implementation(
            r#"
                ---@class MyClass
                ---@field myField number

                ---@type MyClass
                local MyClass = {}
                MyClass.myField = 1

                ---@type MyClass
                local obj
                obj.my<??>Field = 2
            "#,
            vec![
                VirtualLocation {
                    file: "".to_string(),
                    line: 2,
                },
                VirtualLocation {
                    file: "".to_string(),
                    line: 6,
                },
                VirtualLocation {
                    file: "".to_string(),
                    line: 10,
                },
            ],
        ));
        Ok(())
    }

    /// Test C: the prefix-name fallback must resolve the prefix semantically,
    /// not just by name text. An unrelated local named the same as the class
    /// but typed as a different class must NOT match via the prefix fallback.
    #[gtest]
    fn test_implementation_prefix_fallback_requires_semantic_resolution() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        // `MyClass` is a class with field `myField`. A separate file has a
        // local also named `MyClass` but annotated as `@type OtherClass`.
        // The prefix fallback should NOT match because the local's type
        // doesn't correspond to MyClass.
        ws.def_file(
            "other.lua",
            r#"
                ---@class OtherClass
                ---@field myField string

                ---@type OtherClass
                local MyClass = {}

                MyClass.myField = "hello"
            "#,
        );
        check!(ws.check_implementation(
            r#"
                ---@class MyClass
                ---@field my<??>Field number
            "#,
            vec![
                // Only the @field declaration. Neither the untyped local
                // in the main file nor the OtherClass-typed local in
                // other.lua should appear.
                VirtualLocation {
                    file: "".to_string(),
                    line: 2,
                },
            ],
        ));
        Ok(())
    }

    /// Regression: an untyped `local MyClass = {}; MyClass.myField = 999`
    /// must NOT be included in implementations for a @class field of the
    /// same name, because the table has no semantic tie to the class
    /// (no @type annotation). The TableConst name-text fallback is removed.
    #[gtest]
    fn test_implementation_unrelated_untyped_table_const_excluded() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        // Separate file with an untyped local that shares the class name.
        ws.def_file(
            "unrelated.lua",
            r#"
                local MyClass = {}
                MyClass.myField = 999
            "#,
        );
        check!(ws.check_implementation(
            r#"
                ---@class MyClass
                ---@field my<??>Field number
            "#,
            vec![
                // Only the @field declaration. The untyped table in
                // unrelated.lua must NOT appear.
                VirtualLocation {
                    file: "".to_string(),
                    line: 2,
                },
            ],
        ));
        Ok(())
    }
}
