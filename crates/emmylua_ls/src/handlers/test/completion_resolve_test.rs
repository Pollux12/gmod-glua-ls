#[cfg(test)]
mod tests {
    use crate::handlers::test_lib::{
        ProviderVirtualWorkspace, VirtualCompletionResolveItem, check,
    };
    use googletest::prelude::*;

    #[gtest]
    fn test_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();

        check!(ws.check_completion_resolve(
            r#"
                ---@overload fun(event: "AAA", callback: fun(trg: string, data: number)): number
                ---@overload fun(event: "BBB", callback: fun(trg: string, data: string)): string
                ---@param event string
                ---@param callback fun(trg: string, data: number)
                ---@return number
                local function test(event, callback)
                    if event == "" then
                    end
                end

                test<??>
            "#,
            VirtualCompletionResolveItem {
                detail:
                    "local function test(event: string, callback: fun(trg: string, data: number)) -> number (+2 overloads)"
                        .to_string(),
                documentation: None,
            },
        ));
        Ok(())
    }
    #[gtest]
    fn test_2() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();

        check!(ws.check_completion_resolve(
            r#"
                ---@class Hover.Test2
                ---@field event fun(event: "游戏-初始化")
                ---@field event fun(event: "游戏-恢复", key: string)
                local Test2 = {}

                Test2.<??>
            "#,
            VirtualCompletionResolveItem {
                detail: "(field) Test2.event(event: \"游戏-初始化\") (+1 overloads)".to_string(),
                documentation: None,
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_table_field_function_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_completion_resolve(
            r#"
                ---@class T
                ---@field func fun(self:string) 注释注释

                ---@type T
                local t = {
                    <??>
                }
            "#,
            VirtualCompletionResolveItem {
                detail: "(field) T.func(self: string)".to_string(),
                documentation: Some("\n注释注释".to_string()),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_table_field_function_2() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_completion_resolve(
            r#"
                ---@class T
                ---@field func fun(self: T) 注释注释

                ---@type T
                local t = {
                    <??>
                }
            "#,
            VirtualCompletionResolveItem {
                detail: "(method) T:func()".to_string(),
                documentation: Some("\n注释注释".to_string()),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_intersection() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Matchers
            ---@field toBe fun(self: Assertion, expected: any) -- 测试

            ---@class Inverse<T>
            ---@field not_ T

            ---@class Assertion<T>: Matchers<T>
        "#,
        );
        check!(ws.check_completion_resolve(
            r#"

            ---@type Assertion<any>
            local expect
            expect:<??>
            "#,
            VirtualCompletionResolveItem {
                detail: "(method) Matchers:toBe(expected: any)".to_string(),
                documentation: Some("\n测试".to_string()),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_gmod_sandbox_completion_prefers_annotation_docs() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "library/lua/includes/extensions/sandbox_hooks.lua",
            r#"
                ---@class GM
                ---@type GM
                GM = GM or {}

                ---@class SANDBOX
                ---@type SANDBOX
                SANDBOX = SANDBOX or {}

                ---Called when a player attempts to spawn a SENT.
                function SANDBOX:PlayerSpawnSENT(ply, class)
                end
            "#,
        );

        check!(ws.check_completion_resolve(
            r#"
                function GM:PlayerSpawnSENT(ply, class_name)
                end

                function SANDBOX:<??>
                end
            "#,
            VirtualCompletionResolveItem {
                detail: "(method) SANDBOX:PlayerSpawnSENT(ply, class)".to_string(),
                documentation: Some(
                    "\n![(Shared)](https://github.com/user-attachments/assets/a356f942-57d7-4915-a8cc-559870a980fc)\n\nCalled when a player attempts to spawn a SENT."
                        .to_string(),
                ),
            },
        ));

        Ok(())
    }

    #[gtest]
    fn test_gmod_completion_shows_realm_badge_without_description() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "library/lua/includes/extensions/sandbox_hooks.lua",
            r#"
                ---@class SANDBOX
                ---@type SANDBOX
                SANDBOX = SANDBOX or {}

                function SANDBOX:PlayerSpawnSENT(ply, class)
                end
            "#,
        );

        check!(ws.check_completion_resolve(
            r#"
                function SANDBOX:<??>
                end
            "#,
            VirtualCompletionResolveItem {
                detail: "(method) SANDBOX:PlayerSpawnSENT(ply, class)".to_string(),
                documentation: Some(
                    "\n![(Shared)](https://github.com/user-attachments/assets/a356f942-57d7-4915-a8cc-559870a980fc)"
                        .to_string(),
                ),
            },
        ));

        Ok(())
    }
}
