#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::handlers::references::references;
    use crate::handlers::test_lib::{ProviderVirtualWorkspace, VirtualLocation, check};
    use glua_code_analysis::Emmyrc;
    use googletest::prelude::*;
    use tokio_util::sync::CancellationToken;

    #[gtest]
    fn test_function_references() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_references(
            r#"
                local export = {}
                local function fl<??>ush()
                end
                export.flush = flush
                return export
            "#,
            vec![(
                "1.lua",
                r#"
                    local flush = require("virtual_0").flush
                    flush()
                "#,
            )],
            vec![
                VirtualLocation {
                    file: "".to_string(),
                    line: 2,
                },
                VirtualLocation {
                    file: "".to_string(),
                    line: 4,
                },
                VirtualLocation {
                    file: "1.lua".to_string(),
                    line: 1,
                },
                VirtualLocation {
                    file: "1.lua".to_string(),
                    line: 1,
                },
                VirtualLocation {
                    file: "1.lua".to_string(),
                    line: 2,
                },
                VirtualLocation {
                    file: "1.lua".to_string(),
                    line: 1,
                },
                VirtualLocation {
                    file: "".to_string(),
                    line: 4,
                },
            ]
        ));
        Ok(())
    }

    #[gtest]
    fn test_function_references_2() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_references(
            r#"
                local function fl<??>ush()
                end
                return {
                    flush = flush,
                }
            "#,
            vec![(
                "1.lua",
                r#"
                    local flush = require("virtual_0").flush
                    flush()
                "#,
            )],
            vec![
                VirtualLocation {
                    file: "".to_string(),
                    line: 1,
                },
                VirtualLocation {
                    file: "".to_string(),
                    line: 4,
                },
                VirtualLocation {
                    file: "1.lua".to_string(),
                    line: 1,
                },
                VirtualLocation {
                    file: "1.lua".to_string(),
                    line: 1,
                },
                VirtualLocation {
                    file: "1.lua".to_string(),
                    line: 2,
                },
                VirtualLocation {
                    file: "1.lua".to_string(),
                    line: 1,
                },
                VirtualLocation {
                    file: "".to_string(),
                    line: 4,
                },
            ]
        ));
        Ok(())
    }

    #[gtest]
    fn test_module_return() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();

        check!(ws.check_references(
            r#"
                local function init()
                end
                return in<??>it
            "#,
            vec![(
                "a.lua",
                r#"
                local init = require("virtual_0")
                init()
            "#,
            )],
            vec![
                VirtualLocation {
                    file: "virtual_0.lua".to_string(),
                    line: 1,
                },
                VirtualLocation {
                    file: "a.lua".to_string(),
                    line: 1,
                },
                VirtualLocation {
                    file: "a.lua".to_string(),
                    line: 2,
                },
                VirtualLocation {
                    file: "virtual_0.lua".to_string(),
                    line: 3,
                },
            ],
        ));
        Ok(())
    }

    #[gtest]
    fn test_module_return_2() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "a.lua",
            r#"
            local function getA()
            end
            return {
                getA = getA
            }
        "#,
        );

        check!(ws.check_references(
            r#"
                local AModule = require("a")
                AMo<??>dule.getA()
            "#,
            vec![],
            vec![
                VirtualLocation {
                    file: "virtual_0.lua".to_string(),
                    line: 1,
                },
                VirtualLocation {
                    file: "virtual_0.lua".to_string(),
                    line: 2,
                },
            ],
        ));
        Ok(())
    }

    #[gtest]
    fn test_member_references_alias_cycle_does_not_stack_overflow() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();

        let (main_content, position) = check!(ProviderVirtualWorkspace::handle_file_content(
            r#"
                local t = {}
                t.m<??> = function() end
                local x = t.m
                t.m = x
            "#,
        ));
        let file_id = ws.def(&main_content);

        let result = references(
            &ws.analysis,
            file_id,
            position,
            &CancellationToken::new(),
            true,
        )
        .ok_or("failed to get references")
        .or_fail()?;

        let lines: HashSet<u32> = result.iter().map(|l| l.range.start.line).collect();
        assert!(lines.contains(&2));
        assert!(lines.contains(&3));
        assert!(lines.contains(&4));
        Ok(())
    }

    #[gtest]
    fn test_gmod_vgui_panel_string_references() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.analysis.update_config(emmyrc.into());

        check!(ws.check_references(
            r#"
                local parent = vgui.Create("MyPa<??>nel")


                parent:Add("MyPanel")
            "#,
            vec![
                (
                    "defs.lua",
                    "\n\n\n\n\n\n\n\nvgui.Register(\"MyPanel\", PANEL, \"DPanel\")\n",
                ),
                (
                    "usage.lua",
                    "\n\n\n\n\n\n\n\n\n\n\n\nlocal created = vgui.Create(\"MyPanel\")\n",
                ),
            ],
            vec![
                VirtualLocation {
                    file: "virtual_0.lua".to_string(),
                    line: 1,
                },
                VirtualLocation {
                    file: "virtual_0.lua".to_string(),
                    line: 4,
                },
                VirtualLocation {
                    file: "defs.lua".to_string(),
                    line: 8,
                },
                VirtualLocation {
                    file: "usage.lua".to_string(),
                    line: 12,
                },
            ],
        ));

        Ok(())
    }

    #[gtest]
    fn test_gmod_net_message_string_references() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.analysis.update_config(emmyrc.into());

        check!(ws.check_references(
            r#"
                net.Receive("MyMe<??>ssage", function() end)
            "#,
            vec![(
                "send.lua",
                "\n\n\n\n\n\n\nnet.Start(\"MyMessage\")\nnet.Broadcast()\n",
            )],
            vec![
                VirtualLocation {
                    file: "virtual_0.lua".to_string(),
                    line: 1,
                },
                VirtualLocation {
                    file: "send.lua".to_string(),
                    line: 7,
                },
            ],
        ));

        Ok(())
    }

    #[gtest]
    fn test_member_variable_references_include_usages() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_references(
            r#"
                ---@class MyClass
                ---@field myField number

                ---@type MyClass
                local obj

                obj.my<??>Field = 42
                print(obj.myField)
            "#,
            vec![],
            vec![
                VirtualLocation {
                    file: "".to_string(),
                    line: 2,
                },
                VirtualLocation {
                    file: "".to_string(),
                    line: 7,
                },
                VirtualLocation {
                    file: "".to_string(),
                    line: 8,
                },
            ],
        ));
        Ok(())
    }

    #[gtest]
    fn test_member_variable_references_exclude_declaration_when_flag_false() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        // With include_declaration=false, the @field definition (line 2)
        // should be excluded; only usage sites should appear.
        let (main_content, position) = check!(ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@class MyClass
                ---@field myField number

                ---@type MyClass
                local obj

                obj.my<??>Field = 42
                print(obj.myField)
            "#,
        ));
        let file_id = ws.def(&main_content);
        let result_with_decl = references(
            &ws.analysis,
            file_id,
            position.clone(),
            &CancellationToken::new(),
            true,
        )
        .ok_or("failed to get references")
        .or_fail()?;
        let result_without_decl = references(
            &ws.analysis,
            file_id,
            position,
            &CancellationToken::new(),
            false,
        )
        .ok_or("failed to get references")
        .or_fail()?;

        // With include_declaration=true, the @field line should be present
        let lines_with_decl: HashSet<u32> = result_with_decl
            .iter()
            .map(|l| l.range.start.line)
            .collect();
        assert!(
            lines_with_decl.contains(&2),
            "expected @field definition (line 2) with include_declaration=true, got lines: {lines_with_decl:?}"
        );

        // With include_declaration=false, the @field line should be absent
        let lines_without_decl: HashSet<u32> = result_without_decl
            .iter()
            .map(|l| l.range.start.line)
            .collect();
        assert!(
            !lines_without_decl.contains(&2),
            "expected @field definition (line 2) to be excluded with include_declaration=false, got lines: {lines_without_decl:?}"
        );

        // Usage sites should be present in both
        assert!(
            lines_without_decl.contains(&7),
            "expected usage site (line 7) with include_declaration=false"
        );
        assert!(
            lines_without_decl.contains(&8),
            "expected usage site (line 8) with include_declaration=false"
        );

        Ok(())
    }

    #[gtest]
    fn test_dynamic_key_table_field_references_include_key_source() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.analysis.update_config(emmyrc.into());

        check!(ws.check_references(
            r#"
                local rec = { data = {} }
                local data = rec.data
                for key, value in pairs({ forwardSlip = 1, sideSlip = 2 }) do
                    data[key] = value
                end
                local d = rec.data
                math.abs(d.forw<??>ardSlip or 0)
                print(d.forwardSlip)
            "#,
            vec![],
            vec![
                VirtualLocation {
                    file: "".to_string(),
                    line: 3,
                },
                VirtualLocation {
                    file: "".to_string(),
                    line: 7,
                },
                VirtualLocation {
                    file: "".to_string(),
                    line: 8,
                },
            ],
        ));
        Ok(())
    }

    /// Test A: references includeDeclaration=false for globals
    #[gtest]
    fn test_global_references_exclude_declaration_when_flag_false() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let (main_content, position) = check!(ProviderVirtualWorkspace::handle_file_content(
            r#"
                MyGl<??>obal = 42
                print(MyGlobal)
                MyGlobal = 99
            "#,
        ));
        let file_id = ws.def(&main_content);

        let result_with_decl = references(
            &ws.analysis,
            file_id,
            position.clone(),
            &CancellationToken::new(),
            true,
        )
        .ok_or("failed to get references")
        .or_fail()?;

        let result_without_decl = references(
            &ws.analysis,
            file_id,
            position,
            &CancellationToken::new(),
            false,
        )
        .ok_or("failed to get references")
        .or_fail()?;

        let lines_with_decl: HashSet<u32> = result_with_decl
            .iter()
            .map(|l| l.range.start.line)
            .collect();
        let lines_without_decl: HashSet<u32> = result_without_decl
            .iter()
            .map(|l| l.range.start.line)
            .collect();

        // With include_declaration=true, the declaration (line 1) should be present
        assert!(
            lines_with_decl.contains(&1),
            "expected declaration (line 1) with include_declaration=true, got lines: {lines_with_decl:?}"
        );

        // With include_declaration=false, the declaration (line 1) should be absent
        assert!(
            !lines_without_decl.contains(&1),
            "expected declaration (line 1) to be excluded with include_declaration=false, got lines: {lines_without_decl:?}"
        );

        // Usage sites should be present in both
        assert!(
            lines_without_decl.contains(&2),
            "expected usage (line 2) with include_declaration=false"
        );
        assert!(
            lines_without_decl.contains(&3),
            "expected write usage (line 3) with include_declaration=false"
        );

        Ok(())
    }

    /// Test B: member references declaration + usage on same line — ensure usage
    /// is kept when includeDeclaration=false
    #[gtest]
    fn test_member_references_same_line_decl_and_usage() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        // The @field declaration and a usage on the same logical line should
        // not cause the usage to be filtered when include_declaration=false.
        // We use a pattern where the declaration is an @field and the usage
        // is an index expression on the same source line.
        let (main_content, position) = check!(ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@class MyClass
                ---@field myField number

                ---@type MyClass
                local obj

                obj.my<??>Field = 42; print(obj.myField)
            "#,
        ));
        let file_id = ws.def(&main_content);

        let result_without_decl = references(
            &ws.analysis,
            file_id,
            position,
            &CancellationToken::new(),
            false,
        )
        .ok_or("failed to get references")
        .or_fail()?;

        let lines_without_decl: HashSet<u32> = result_without_decl
            .iter()
            .map(|l| l.range.start.line)
            .collect();

        // The @field declaration (line 2) should be excluded
        assert!(
            !lines_without_decl.contains(&2),
            "expected @field definition (line 2) to be excluded with include_declaration=false, got lines: {lines_without_decl:?}"
        );

        // Usage on line 7 should be present (both the write and the read)
        assert!(
            lines_without_decl.contains(&7),
            "expected usage site (line 7) with include_declaration=false, got lines: {lines_without_decl:?}"
        );

        let same_line_ranges: HashSet<_> = result_without_decl
            .iter()
            .filter(|location| location.range.start.line == 7)
            .map(|location| {
                (
                    location.range.start.line,
                    location.range.start.character,
                    location.range.end.line,
                    location.range.end.character,
                )
            })
            .collect();
        assert_eq!(
            same_line_ranges.len(),
            2,
            "expected two distinct same-line member references on line 7 (write + read), got: {result_without_decl:?}"
        );

        Ok(())
    }
}
