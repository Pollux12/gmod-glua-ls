#[cfg(test)]
mod tests {
    use crate::handlers::rename::{prepare_rename, rename};
    use crate::handlers::test_lib::{ProviderVirtualWorkspace, check};
    use googletest::prelude::*;
    use lsp_types::{Position, Range, TextEdit};

    /// A written `self` has a declaration to carry the new name; only a colon
    /// method's implicit receiver does not.
    #[gtest]
    fn test_rename_self_only_refused_for_the_implicit_receiver() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_rename(
            r#"
                local <??>self = 1
                print(self)
            "#,
            "captured".to_string(),
            vec![(
                "virtual_0.lua".to_string(),
                vec![
                    TextEdit {
                        range: Range::new(Position::new(1, 22), Position::new(1, 26)),
                        new_text: "captured".to_string(),
                    },
                    TextEdit {
                        range: Range::new(Position::new(2, 22), Position::new(2, 26)),
                        new_text: "captured".to_string(),
                    },
                ],
            )]
        ));

        let mut ws = ProviderVirtualWorkspace::new();
        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                local Class = {}
                function Class:Method()
                    return <??>self
                end
            "#,
        )?;
        let file_id = ws.def(&content);
        verify_that!(
            rename(&ws.analysis, file_id, position, "renamed".to_string()).is_none(),
            eq(true)
        )?;
        // prepareRename must agree, or the client opens a rename box for an
        // edit that never arrives.
        verify_that!(
            prepare_rename(&ws.analysis, file_id, position).is_none(),
            eq(true)
        )?;
        Ok(())
    }

    /// `DEFINE_BASECLASS` writes no `BaseClass` name to carry a new one, so
    /// renaming it would only overwrite the macro's argument.
    #[gtest]
    fn test_rename_refused_for_define_baseclass_local() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = glua_code_analysis::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                DEFINE_BASECLASS("Panel")
                local _ = <??>BaseClass
            "#,
        )?;
        let file_id = ws.def_file("lua/vgui/cl_panel.lua", &content);
        verify_that!(
            rename(&ws.analysis, file_id, position, "renamed".to_string()).is_none(),
            eq(true)
        )?;
        verify_that!(
            prepare_rename(&ws.analysis, file_id, position).is_none(),
            eq(true)
        )?;
        Ok(())
    }

    /// The request position and the returned edit ranges count UTF-16 code
    /// units, so a name after an emoji sits one column further right than its
    /// character count.
    #[gtest]
    fn rename_after_an_emoji_uses_utf16_columns() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let file_id = ws.def("local s = \"\u{1F642}\" local val = 1\nprint(val)\n");
        let result = rename(
            &ws.analysis,
            file_id,
            Position::new(0, 22),
            "renamed".to_string(),
        )
        .or_fail()?;
        let mut edits = result
            .changes
            .or_fail()?
            .into_values()
            .flatten()
            .collect::<Vec<_>>();
        edits.sort_by_key(|edit| edit.range.start);
        verify_eq!(
            edits,
            vec![
                TextEdit {
                    range: Range::new(Position::new(0, 21), Position::new(0, 24)),
                    new_text: "renamed".to_string(),
                },
                TextEdit {
                    range: Range::new(Position::new(1, 6), Position::new(1, 9)),
                    new_text: "renamed".to_string(),
                },
            ]
        )
    }

    #[gtest]
    fn test_int_key() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_rename(
            r#"
                local export = {
                    [<??>1] = 1,
                }

                export[1] = 2
            "#,
            "2".to_string(),
            vec![(
                "virtual_0.lua".to_string(),
                vec![
                    TextEdit {
                        range: Range::new(Position::new(2, 21), Position::new(2, 22),),
                        new_text: "2".to_string(),
                    },
                    TextEdit {
                        range: Range::new(Position::new(5, 23), Position::new(5, 24),),
                        new_text: "2".to_string(),
                    },
                ],
            )]
        ));
        check!(ws.check_rename(
            r#"
                local export = {
                    [1] = 1,
                }

                export[<??>1] = 2
            "#,
            "2".to_string(),
            vec![(
                "virtual_1.lua".to_string(),
                vec![
                    TextEdit {
                        range: Range::new(Position::new(2, 21), Position::new(2, 22),),
                        new_text: "2".to_string(),
                    },
                    TextEdit {
                        range: Range::new(Position::new(5, 23), Position::new(5, 24),),
                        new_text: "2".to_string(),
                    },
                ],
            )]
        ));
        Ok(())
    }

    #[gtest]
    fn test_int_key_in_class() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_rename(
            r#"
                ---@class Test
                ---@field [<??>1] number
                local Test = {}

                Test[1] = 2
            "#,
            "2".to_string(),
            vec![(
                "virtual_0.lua".to_string(),
                vec![
                    TextEdit {
                        range: Range::new(Position::new(2, 27), Position::new(2, 28)),
                        new_text: "2".to_string(),
                    },
                    TextEdit {
                        range: Range::new(Position::new(5, 21), Position::new(5, 22)),
                        new_text: "2".to_string(),
                    },
                ],
            )],
        ));
        Ok(())
    }

    #[gtest]
    fn test_rename_class_field() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_rename(
            r#"
                ---@class AnonymousObserver
                local AnonymousObserver

                function AnonymousObserver:__init(next)
                    self.ne<??>xt = next
                end

                function AnonymousObserver:onNextCore(value)
                    self.next(value)
                end
            "#,
            "_next".to_string(),
            vec![(
                "virtual_0.lua".to_string(),
                vec![
                    TextEdit {
                        range: Range::new(Position::new(5, 25), Position::new(5, 29),),
                        new_text: "_next".to_string(),
                    },
                    TextEdit {
                        range: Range::new(Position::new(9, 25), Position::new(9, 29),),
                        new_text: "_next".to_string(),
                    },
                ],
            )]
        ));
        Ok(())
    }

    #[gtest]
    fn test_rename_generic_type() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_rename(
            r#"
                ---@class Params<T>

                ---@type Para<??>ms<number>
            "#,
            "Params1".to_string(),
            vec![(
                "virtual_0.lua".to_string(),
                vec![
                    TextEdit {
                        range: Range::new(Position::new(3, 25), Position::new(3, 31)),
                        new_text: "Params1".to_string(),
                    },
                    TextEdit {
                        range: Range::new(Position::new(1, 26), Position::new(1, 32)),
                        new_text: "Params1".to_string(),
                    },
                ],
            )]
        ));
        Ok(())
    }

    #[gtest]
    fn test_rename_class_field_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_rename(
            r#"
                ---@class ABC
                local ABC = {}

                local function test()
                end
                ABC.te<??>st = test

                ABC.test()
            "#,
            "test1".to_string(),
            vec![(
                "virtual_0.lua".to_string(),
                vec![
                    TextEdit {
                        range: Range::new(Position::new(8, 20), Position::new(8, 24)),
                        new_text: "test1".to_string(),
                    },
                    TextEdit {
                        range: Range::new(Position::new(6, 20), Position::new(6, 24)),
                        new_text: "test1".to_string(),
                    },
                ],
            )]
        ));
        Ok(())
    }

    #[gtest]
    fn test_doc_param() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        {
            check!(ws.check_rename(
                r#"
                    ---@param aaa<??> number
                    local function test(aaa)
                        local b = aaa
                    end
                "#,
                "aaa1".to_string(),
                vec![(
                    "virtual_0.lua".to_string(),
                    vec![
                        TextEdit {
                            range: Range::new(Position::new(1, 30), Position::new(1, 33)),
                            new_text: "aaa1".to_string(),
                        },
                        TextEdit {
                            range: Range::new(Position::new(2, 40), Position::new(2, 43)),
                            new_text: "aaa1".to_string(),
                        },
                        TextEdit {
                            range: Range::new(Position::new(3, 34), Position::new(3, 37)),
                            new_text: "aaa1".to_string(),
                        },
                    ],
                )]
            ));
        }
        {
            check!(ws.check_rename(
                r#"
                    ---@param aaa<??> number
                    function testA(aaa)
                        local b = aaa
                    end
                "#,
                "aaa1".to_string(),
                vec![(
                    "virtual_1.lua".to_string(),
                    vec![
                        TextEdit {
                            range: Range::new(Position::new(1, 30), Position::new(1, 33),),
                            new_text: "aaa1".to_string(),
                        },
                        TextEdit {
                            range: Range::new(Position::new(2, 35), Position::new(2, 38),),
                            new_text: "aaa1".to_string(),
                        },
                        TextEdit {
                            range: Range::new(Position::new(3, 34), Position::new(3, 37),),
                            new_text: "aaa1".to_string(),
                        },
                    ],
                )]
            ));
        }
        Ok(())
    }

    #[gtest]
    fn test_namespace_class() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "a.lua",
            r#"
                ---@param a Luakit.Test.Abc
                local function Of(a)
                end
            "#,
        );
        check!(ws.check_rename(
            r#"
                ---@namespace Luakit
                ---@class Test.Abc<??>
                local Test = {}
            "#,
            "Abc".to_string(),
            vec![
                (
                    "virtual_0.lua".to_string(),
                    vec![TextEdit {
                        range: Range::new(Position::new(2, 26), Position::new(2, 34)),
                        new_text: "Abc".to_string(),
                    }]
                ),
                (
                    "a.lua".to_string(),
                    vec![TextEdit {
                        range: Range::new(Position::new(1, 28), Position::new(1, 43)),
                        new_text: "Luakit.Abc".to_string(),
                    }],
                ),
            ]
        ));
        Ok(())
    }

    #[gtest]
    fn test_namespace_class_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_rename(
            r#"
                ---@namespace Luakit
                ---@class Abc
                local Test = {}

                ---@type Abc<??>
                local a = Test
            "#,
            "AAA".to_string(),
            vec![(
                "virtual_0.lua".to_string(),
                vec![
                    TextEdit {
                        range: Range::new(Position::new(5, 25), Position::new(5, 28)),
                        new_text: "AAA".to_string(),
                    },
                    TextEdit {
                        range: Range::new(Position::new(2, 26), Position::new(2, 29)),
                        new_text: "AAA".to_string(),
                    },
                ],
            )]
        ));
        Ok(())
    }
}
