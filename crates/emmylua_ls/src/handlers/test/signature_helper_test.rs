#[cfg(test)]
mod tests {
    use crate::handlers::test_lib::{ProviderVirtualWorkspace, VirtualSignatureHelp, check};
    use googletest::prelude::*;

    #[gtest]
    fn test_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_signature_helper(
            r#"
                ---@class Action
                ---@field id fun(self:Action, itemId:integer, ...:integer?):boolean
                ---@overload fun():Action
                Action = {}

                Action:id(1, <??>)
            "#,
            VirtualSignatureHelp {
                target_label: "Action:id(itemId: integer, ...: integer?): boolean".to_string(),
                active_signature: 0,
                active_parameter: 1,
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_1_manual_invoke_on_arg() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_signature_helper(
            r#"
                ---@class Action
                ---@field id fun(self:Action, itemId:integer, ...:integer?):boolean
                ---@overload fun():Action
                Action = {}

                Action:id(1, ar<??>)
            "#,
            VirtualSignatureHelp {
                target_label: "Action:id(itemId: integer, ...: integer?): boolean".to_string(),
                active_signature: 0,
                active_parameter: 1,
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_2() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new_with_init_std_lib();
        check!(ws.check_signature_helper(
            r#"
                ---@param path string
                local function readFile(path)
                end

                pcall(readFile, <??>)
            "#,
            VirtualSignatureHelp {
                target_label: "pcall(f: sync fun(path: string), path: string): boolean".to_string(),
                active_signature: 0,
                active_parameter: 1,
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_callback_signature_help() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_signature_helper(
            r#"
                ---@class EventData
                ---@field name string

                ---@class EventDispatcher
                ---@field pre fun(self:EventDispatcher,callback:fun(context:EventData))
                local EventDispatcher = {}

                EventDispatcher:pre(function(<??>)
                end)
            "#,
            VirtualSignatureHelp {
                target_label: "callback(context: EventData)".to_string(),
                active_signature: 0,
                active_parameter: 0,
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_callback_signature_help_2() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_signature_helper(
            r#"
                ---@class EventData
                ---@field name string

                ---@class EventDispatcher
                ---@field pre fun(self:EventDispatcher,callback:fun(context:EventData, other: string))
                local EventDispatcher = {}

                EventDispatcher:pre(function(context, <??>)
                end)
            "#,
            VirtualSignatureHelp {
                target_label: "callback(context: EventData, other: string)".to_string(),
                active_signature: 0,
                active_parameter: 1,
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_callback_signature_help_3() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_signature_helper(
            r#"
                ---@class EventData
                ---@field name string

                ---@class EventDispatcher
                ---@field pre fun(self:EventDispatcher,callback:fun(context:EventData, other: string))
                local EventDispatcher = {}

                EventDispatcher:pre(function(context, oth<??>)
                end)
            "#,
            VirtualSignatureHelp {
                target_label: "callback(context: EventData, other: string)".to_string(),
                active_signature: 0,
                active_parameter: 1,
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_gmod_hook_add_signature_help() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        check!(ws.check_signature_helper(
            r#"
                ---@class hook
                hook = {}

                ---@param eventName string
                ---@param identifier any
                ---@param func function
                function hook.Add(eventName, identifier, func) end

                ---@class GM
                GM = {}

                ---@hook PlayerInitialSpawn
                ---@param ply Player
                ---@param transition boolean
                function GM:PlayerInitialSpawn(ply, transition) end

                hook.Add("PlayerInitialSpawn", "id", function(ply, <??>)
                end)
            "#,
            VirtualSignatureHelp {
                target_label: "callback(ply: Player, transition: boolean)".to_string(),
                active_signature: 0,
                active_parameter: 1,
            },
        ));
        Ok(())
    }
}
