#[cfg(test)]
mod test {
    use crate::{LuaType, VirtualWorkspace};

    #[test]
    fn test_closure_param_infer() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"

        ---@class EventData
        ---@field name string

        ---@class EventDispatcher
        ---@field pre fun(self:EventDispatcher,callback:fun(context:EventData))
        local EventDispatcher = {}

        EventDispatcher:pre(function(context)
            b = context
        end)
        "#,
        );

        let ty = ws.expr_ty("b");
        let expected = ws.ty("EventData");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_function_param_inherit() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@alias Outfit_t table

        ---@class Creature
        ---@field onChangeOutfit fun(self:Creature, outfit:Outfit_t):boolean
        ---@overload fun(id:integer):Creature?
        Creature = {}

        function Creature:onChangeOutfit(outfit)
            a = outfit
        end

        "#,
        );

        let ty = ws.expr_ty("a");
        let expected = ws.ty("Outfit_t");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_table_field_function_param() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias ProxyHandler.Getter fun(self: self, raw: any, key: any, receiver: table): any

            ---@class ProxyHandler
            ---@field get ProxyHandler.Getter
        "#,
        );

        ws.def(
            r#"

        ---@class A: ProxyHandler
        local A

        function A:get(target, key, receiver, name)
            a = self
        end
                "#,
        );
        let ty = ws.expr_ty("a");
        let expected = ws.ty("A");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));

        ws.def(
            r#"

        ---@class B: ProxyHandler
        local B

        B.get = function(self, target, key, receiver, name)
            b = self
        end
                "#,
        );
        let ty = ws.expr_ty("b");
        let expected = ws.ty("B");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));

        ws.def(
            r#"
        ---@class C: ProxyHandler
        local C = {
            get = function(self, target, key, receiver, name)
                c = self
            end,
        }
                "#,
        );
        let ty = ws.expr_ty("c");
        let expected = ws.ty("C");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_table_field_function_param_2() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class ProxyHandler
            local P

            ---@param raw any
            ---@param key any
            ---@param receiver table
            ---@return any
            function P:get(raw, key, receiver) end
            "#,
        );

        ws.def(
            r#"
            ---@class A: ProxyHandler
            local A

            function A:get(raw, key, receiver)
                a = receiver
            end
            "#,
        );
        let ty = ws.expr_ty("a");
        let expected = ws.ty("table");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_table_field_function_param_3() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class SimpleClass.Meta
            ---@field __defineSet fun(self: self, key: string, f: fun(self: self, value: any))

            ---@class Dep:  SimpleClass.Meta
            local Dep
            Dep:__defineSet('subs', function(self, value)
                a  = self
            end)
            "#,
        );
        let ty = ws.expr_ty("a");
        let expected = ws.ty("Dep");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_table_field_function_param_4() {
        let mut ws = VirtualWorkspace::new();
        ws.def(r#"
                ---@alias ProxyHandler.Getter fun(self: self, raw: any, key: any, receiver: table): any

                ---@class ProxyHandler
                ---@field get? ProxyHandler.Getter
            "#
        );

        ws.def(
            r#"
            ---@class ShallowUnwrapHandlers: ProxyHandler
            local ShallowUnwrapHandlers = {
                get = function(self, target, key, receiver)
                    a = self
                end,
            }
            "#,
        );
        let ty = ws.expr_ty("a");
        let expected = ws.ty("ShallowUnwrapHandlers");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_issue_350() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                --- @param x string|fun(args: string[])
                function cmd(x) end
            "#,
        );

        ws.def(
            r#"
                cmd(function(args)
                a = args -- should be string[]
                end)
            "#,
        );
        let ty = ws.expr_ty("a");
        let expected = ws.ty("string[]");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_callback_union_order_selects_deterministic_doc_function_variant() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@alias CallbackString fun(arg: string)
                ---@alias CallbackInteger fun(arg: integer)

                ---@param cb CallbackString|CallbackInteger
                function takes_union_first(cb) end

                ---@param cb CallbackInteger|CallbackString
                function takes_union_second(cb) end

                takes_union_first(function(arg)
                    callback_union_first = arg
                end)

                takes_union_second(function(arg)
                    callback_union_second = arg
                end)
            "#,
        );

        let first_ty = ws.expr_ty("callback_union_first");
        let second_ty = ws.expr_ty("callback_union_second");
        assert_eq!(
            ws.humanize_type(first_ty),
            ws.humanize_type(second_ty),
            "callback inference should be independent of union member order"
        );
    }

    #[test]
    fn test_field_doc_function() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class ClosureTest
            ---@field e fun(a: string, b: boolean)
            ---@field e fun(a: number, b: boolean)
            local Test

            function Test.e(a, b)
            end
            A = Test.e
            "#,
        );
        // 必须要这样写, 无法直接`A = a`拿到`a`的实际类型, `A`的推断目前是独立的且在`Test.e`推断之前缓存
        let ty = ws.expr_ty("A");
        let expected_a = ws.ty("string|number");
        // let expected_a_str = ws.humanize_type(expected_a);

        match ty {
            LuaType::Union(union) => {
                let types = union.into_vec();
                let signature = types
                    .iter()
                    .last()
                    .and_then(|t| match t {
                        LuaType::Signature(id) => {
                            ws.get_db_mut().get_signature_index_mut().get_mut(id)
                        }
                        _ => None,
                    })
                    .expect("Expected a function type");

                let param_type = signature
                    .get_param_info_by_name("a")
                    .map(|p| p.type_ref.clone())
                    .expect("Parameter 'a' not found");

                assert_eq!(param_type, expected_a);
            }
            _ => panic!("Expected a union type"),
        }
    }

    #[test]
    fn test_field_doc_function_2() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class ClosureTest
            local Test

            ---@overload fun(a: string, b: number)
            ---@overload fun(a: number, b: number)
            function Test.e(a, b)
                A = a
                B = b
            end
            "#,
        );

        {
            let ty = ws.expr_ty("A");
            let expected = ws.ty("string|number");
            assert_eq!(ty, expected);
        }

        {
            let ty = ws.expr_ty("B");
            let expected = ws.ty("number");
            assert_eq!(ty, expected);
        }
    }

    #[test]
    fn test_field_doc_function_3() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class ClosureTest
            ---@field e fun(a: string, b: number) -- 不在 overload 时必须声明 self 才被视为方法
            ---@field e fun(a: number, b: number)
            local Test

            function Test:e(a, b) -- `:`声明
                A = a
            end
            "#,
        );
        let ty = ws.expr_ty("A");
        let expected = ws.ty("number");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_issue_416() {
        let mut ws = VirtualWorkspace::new();
        ws.def_files(vec![
            (
                "test.lua",
                r#"
                ---@class CustomEvent
                ---@field private custom_event_manager? EventManager
                local M = {}

                ---@return EventManager
                function newEventManager()
                end

                function M:event_on()
                    if not self.custom_event_manager then
                        self.custom_event_manager = newEventManager()
                    end
                    B = self.custom_event_manager
                    local trigger = self.custom_event_manager:get_trigger()
                    A = trigger
                    return trigger
                end
            "#,
            ),
            (
                "test2.lua",
                r#"
                require "test1"
                ---@class Trigger

                ---@class EventManager
                local EventManager

                ---@return Trigger
                function EventManager:get_trigger()
                end
            "#,
            ),
        ]);

        let ty = ws.expr_ty("A");
        let expected = ws.ty("Trigger");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_field_doc_function_4() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@alias Trigger.CallBack fun(trg: Trigger, ...): any, any, any, any

                ---@class CustomEvent1
                ---@field event_on fun(self: self, event_name:string, callback:Trigger.CallBack):Trigger
                ---@field event_on fun(self: self, event_name:string, args:any[] | any, callback:Trigger.CallBack):Trigger
                local M


                function M:event_on(...)
                    local event_name, args, callback = ...
                    A = args
                end

            "#,
        );
        let ty = ws.expr_ty("A");
        let expected = ws.ty("any");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_field_doc_function_5() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@alias Trigger.CallBack fun(trg: Trigger, ...): any, any, any, any

                ---@class CustomEvent1
                local M

                ---@overload fun(self: self, event_name:string, callback:Trigger.CallBack):Trigger
                ---@overload fun(self: self, event_name:string, args:any[] | any, callback:Trigger.CallBack):Trigger
                function M:event_on(...)
                    local event_name, args, callback = ...
                    A = args
                end

            "#,
        );
        let ty = ws.expr_ty("A");
        let expected = ws.ty("any");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_issue_498() {
        let mut ws = VirtualWorkspace::new();
        ws.def_files(vec![
            (
                "test.lua",
                r#"
                ---@class CustomEvent
                ---@field private custom_event_manager? EventManager
                local M = {}

                function M:event_on()
                    if not self.custom_event_manager then
                        self.custom_event_manager = New 'EventManager' (self)
                    end
                    local trigger = self.custom_event_manager:get_trigger()
                    A = trigger
                    return trigger
                end
            "#,
            ),
            (
                "test2.lua",
                r#"
                ---@class Trigger

                ---@class EventManager
                ---@overload fun(object?: table): self
                local EventManager

                ---@return Trigger
                function EventManager:get_trigger()
                end
            "#,
            ),
            (
                "class.lua",
                r#"
                local M = {}

                ---@generic T: string
                ---@param name `T`
                ---@param tbl? table
                ---@return T
                function M.declare(name, tbl)
                end
                return M
            "#,
            ),
            (
                "init.lua",
                r#"
                New = require "class".declare
            "#,
            ),
        ]);
        let ty = ws.expr_ty("A");
        let expected = ws.ty("Trigger");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_param_function_is_alias() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class LocalTimer
            ---@alias LocalTimer.OnTimer fun(timer: LocalTimer, count: integer, ...: any)

            ---@param on_timer LocalTimer.OnTimer
            ---@return LocalTimer
            function loop_count(on_timer)
            end

            loop_count(function(timer, count)
                A = timer
            end)
            "#,
        );
        let ty = ws.expr_ty("A");
        let expected = ws.ty("LocalTimer");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_issue_791() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias HookAlias fun(a:integer)

            ---@class TypeA
            ---@field hook HookAlias

            ---@class TypeB
            ---@field hook fun(a:integer)

            ---@param d TypeA
            function fnA(d) end

            ---@param d TypeB
            function fnB(d) end

            fnA({ hook = function(obj) a = obj end }) -- obj is any, not integer
            "#,
        );
        let ty = ws.expr_ty("a");
        let expected = ws.ty("integer");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_dot_function_param_inherit() {
        // Tests that dot-style function definitions inherit param types from
        // annotated Signatures (e.g. TOOL.BuildCPanel pattern in Garry's Mod)
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class ControlPanel
            ---@field AddControl fun(self: ControlPanel, type: string, controlinfo: table): Panel

            ---@class Tool
            ---@field BuildCPanel fun(panel: ControlPanel)

            ---@class TOOL : Tool
            TOOL = {}

            ---@param panel ControlPanel
            function TOOL.BuildCPanel(panel) end
            "#,
        );

        ws.def(
            r#"
            function TOOL.BuildCPanel(panel)
                a = panel
            end
            "#,
        );

        let ty = ws.expr_ty("a");
        let expected = ws.ty("ControlPanel");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_gmod_hook_add_callback_params_infer_from_hook_name() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def(
            r#"
            ---@class Entity
            local Entity = {}

            ---@class GM
            GM = {}

            ---@hook AcceptInput
            ---@param ent Entity
            ---@param input string
            ---@param activator Entity
            ---@param caller Entity
            ---@param value any
            ---@return boolean
            function GM:AcceptInput(ent, input, activator, caller, value) end

            hook = {}
            ---@param eventName string
            ---@param identifier any
            ---@param func function
            function hook.Add(eventName, identifier, func) end

            hook.Add("AcceptInput", "Test", function(ent, input, activator, caller, value)
                gmod_hook_ent = ent
                gmod_hook_input = input
                gmod_hook_activator = activator
                gmod_hook_caller = caller
                gmod_hook_value = value
            end)
            "#,
        );

        let hook_ent = ws.expr_ty("gmod_hook_ent");
        let hook_input = ws.expr_ty("gmod_hook_input");
        let hook_activator = ws.expr_ty("gmod_hook_activator");
        let hook_caller = ws.expr_ty("gmod_hook_caller");
        let hook_value = ws.expr_ty("gmod_hook_value");
        let entity_type = ws.ty("Entity");
        let string_type = ws.ty("string");
        let any_type = ws.ty("any");

        assert_eq!(
            ws.humanize_type(hook_ent),
            ws.humanize_type(entity_type.clone())
        );
        assert_eq!(ws.humanize_type(hook_input), ws.humanize_type(string_type));
        assert_eq!(
            ws.humanize_type(hook_activator),
            ws.humanize_type(entity_type.clone())
        );
        assert_eq!(ws.humanize_type(hook_caller), ws.humanize_type(entity_type));
        assert_eq!(ws.humanize_type(hook_value), ws.humanize_type(any_type));
    }
}
