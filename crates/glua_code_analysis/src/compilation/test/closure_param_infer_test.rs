#[cfg(test)]
mod test {
    use crate::{
        Emmyrc, GmodHookKind, GmodRealm, LuaMemberKey, LuaType, LuaTypeDeclId, VirtualWorkspace,
        semantic::find_members_with_key_in_workspace_for_file,
    };
    use glua_parser::{LuaAstNode, LuaExpr, LuaIndexExpr, LuaNameExpr};

    fn index_expr_type(ws: &VirtualWorkspace, file_id: crate::FileId, text: &str) -> LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let index_expr = semantic_model
            .get_root()
            .descendants::<LuaIndexExpr>()
            .find(|expr| expr.syntax().text() == text)
            .expect("index expression");
        semantic_model
            .infer_expr(LuaExpr::IndexExpr(index_expr))
            .expect("index expression type")
    }

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
    fn unannotated_function_callback_infers_table_param_from_direct_invocations() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let file_ids = ws.def_files(vec![
            (
                "lua/autorun/shared/api.lua",
                r#"
                ---@class CallbackEntity
                CallbackApi = {}

                function CallbackApi.Read(callback)
                    ---@type CallbackEntity
                    local proc
                    local data = { proc = proc }
                    callback(false, data, "failed")
                    callback(true, data)
                end
                "#,
            ),
            (
                "lua/autorun/shared/consumer.lua",
                r#"
                CallbackApi.Read(function(ok, data, err)
                    callback_ok = ok
                    callback_proc = data.proc
                    callback_err = err
                end)
                "#,
            ),
        ]);

        let expected_entity = ws.ty("CallbackEntity");
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_ids[1])
            .expect("consumer semantic model");
        let param_type = |name: &str| {
            let param = semantic_model
                .get_root()
                .descendants::<LuaNameExpr>()
                .find(|param| param.get_name_text().as_deref() == Some(name))
                .expect("callback parameter use");
            semantic_model
                .get_semantic_info(param.syntax().clone().into())
                .expect("callback parameter semantic info")
                .display_typ()
                .clone()
        };

        assert_eq!(param_type("ok"), LuaType::Unknown);
        let proc_expr = semantic_model
            .get_root()
            .descendants::<LuaIndexExpr>()
            .find(|expr| expr.syntax().text() == "data.proc")
            .expect("callback data.proc expression");
        assert_eq!(
            semantic_model
                .infer_expr(LuaExpr::IndexExpr(proc_expr))
                .expect("callback proc type"),
            expected_entity
        );
        assert_eq!(param_type("err"), LuaType::Unknown);
    }

    #[test]
    fn callback_table_param_refreshes_after_callee_edit_and_reopen() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "lua/autorun/shared/types.lua",
            "---@class CallbackEntityA\n---@class CallbackEntityB\n",
        );
        let api_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/shared/callback_api.lua");
        let api_source = |entity_type: &str| {
            format!(
                r#"
                CallbackApi = {{}}
                function CallbackApi.Read(callback)
                    ---@type {entity_type}
                    local proc
                    local data = {{}}
                    data.proc = proc
                    callback(data)
                end
                "#,
            )
        };
        ws.analysis
            .update_file_by_uri(&api_uri, Some(api_source("CallbackEntityA")))
            .expect("initial callback API");
        let consumer_file_id = ws.def_file(
            "lua/autorun/shared/callback_consumer.lua",
            r#"
            CallbackApi.Read(function(data)
                callback_proc = data.proc
            end)
            "#,
        );

        assert_eq!(
            index_expr_type(&ws, consumer_file_id, "data.proc"),
            ws.ty("CallbackEntityA")
        );

        ws.analysis
            .update_file_by_uri(&api_uri, Some(api_source("CallbackEntityB")))
            .expect("edited callback API");
        assert_eq!(
            index_expr_type(&ws, consumer_file_id, "data.proc"),
            ws.ty("CallbackEntityB")
        );

        ws.analysis
            .remove_file_by_uri(&api_uri)
            .expect("removed callback API");
        ws.analysis
            .update_file_by_uri(&api_uri, Some(api_source("CallbackEntityA")))
            .expect("reopened callback API");
        assert_eq!(
            index_expr_type(&ws, consumer_file_id, "data.proc"),
            ws.ty("CallbackEntityA")
        );
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
    fn test_local_table_field_function_param_infers_from_call_argument_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class ModelEntity
            ---@field GetModel fun(self: ModelEntity): string

            ---@class SkeletonConvertor
            ---@field IsApplicable fun(self: SkeletonConvertor, ent: ModelEntity): boolean

            ---@param builder SkeletonConvertor
            function register(builder) end

            local Builder = {
                IsApplicable = function(self, ent)
                    A = ent
                    B = ent:GetModel()
                    local model = ent:GetModel()
                    C = model
                    return true
                end
            }

            register(Builder)
            "#,
        );
        let ty = ws.expr_ty("A");
        let expected = ws.ty("ModelEntity");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
        let ty = ws.expr_ty("B");
        let expected = ws.ty("string");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
        let ty = ws.expr_ty("C");
        let expected = ws.ty("string");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_local_table_field_function_param_infers_from_overload_call_argument_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class ModelEntity
            ---@field GetModel fun(self: ModelEntity): string

            ---@class SkeletonConvertor
            ---@field IsApplicable fun(self: SkeletonConvertor, ent: ModelEntity): boolean

            list = {}

            ---@overload fun(identifier: "SkeletonConvertor", key: string, item: SkeletonConvertor)
            ---@param identifier string
            ---@param key any
            ---@param item any
            function list.Set(identifier, key, item) end

            local Builder = {
                IsApplicable = function(self, ent)
                    A = ent
                    B = ent:GetModel()
                    local model = ent:GetModel()
                    C = model
                    return true
                end
            }

            list.Set("SkeletonConvertor", "TF2_engineer", Builder)
            "#,
        );
        let ty = ws.expr_ty("A");
        let expected = ws.ty("ModelEntity");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
        let ty = ws.expr_ty("B");
        let expected = ws.ty("string");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
        let ty = ws.expr_ty("C");
        let expected = ws.ty("string");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_local_table_field_function_param_infers_from_cross_file_overload_call_argument_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class ModelEntity
            ---@field GetModel fun(self: ModelEntity): string

            ---@class SkeletonConvertor
            ---@field IsApplicable fun(self: SkeletonConvertor, ent: ModelEntity): boolean

            list = {}

            ---@overload fun(identifier: "SkeletonConvertor", key: string, item: SkeletonConvertor)
            ---@param identifier string
            ---@param key any
            ---@param item any
            function list.Set(identifier, key, item) end
            "#,
        );
        ws.def(
            r#"
            local Builder = {
                IsApplicable = function(self, ent)
                    A = ent
                    B = ent:GetModel()
                    local model = ent:GetModel()
                    C = model
                    return true
                end
            }

            list.Set("SkeletonConvertor", "TF2_engineer", Builder)
            "#,
        );

        let ty = ws.expr_ty("A");
        let expected = ws.ty("ModelEntity");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
        let ty = ws.expr_ty("B");
        let expected = ws.ty("string");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
        let ty = ws.expr_ty("C");
        let expected = ws.ty("string");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
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
    fn test_stool_buildcpanel_param_inherits_global_tool_contract() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let library_root = ws.virtual_url_generator.base.join("library");
        ws.analysis.add_library_workspace(library_root);
        ws.def_file(
            "library/tool.lua",
            r#"
            ---@meta
            ---@class ControlPanel
            ---@type fun(panel: ControlPanel, ...any)
            TOOL.BuildCPanel = nil
            "#,
        );
        ws.def_file(
            "lua/weapons/gmod_tool/stools/context_test.lua",
            r#"
            function TOOL.BuildCPanel(panel)
                inferred_panel = panel
            end
            "#,
        );

        let ty = ws.expr_ty("inferred_panel");
        let expected = ws.ty("ControlPanel");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_explicit_member_function_type_overrides_inherited_contract() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class ExplicitParent
            ---@field Run fun(value: string)
            local ExplicitParent = {}

            ---@class ExplicitChild : ExplicitParent
            local ExplicitChild = {}

            ---@type fun(value: number)
            ExplicitChild.Run = function(value)
                explicit_member_value = value
            end
            "#,
        );

        let ty = ws.expr_ty("explicit_member_value");
        let expected = ws.ty("number");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_inherited_member_contract_respects_main_workspace_visibility() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.workspace.enable_isolation = true;
        ws.update_emmyrc(emmyrc);
        let other_workspace = ws
            .virtual_url_generator
            .base
            .parent()
            .expect("virtual workspace parent")
            .join("other_workspace");
        ws.analysis.add_main_workspace(other_workspace.clone());
        let other_uri =
            lsp_types::Uri::parse_from_file_path(&other_workspace.join("base.lua")).unwrap();
        let other_file_id = ws
            .analysis
            .update_file_by_uri(
                &other_uri,
                Some(
                    r#"
            ---@class WorkspaceBase
            ---@field Run fun(value: number)
            "#
                    .to_string(),
                ),
            )
            .expect("other workspace file");
        let main_file_id = ws.def_file(
            "base.lua",
            r#"
            ---@class WorkspaceBase
            ---@field Run fun(value: string)
            "#,
        );
        let caller_file_id = ws.def_file(
            "override.lua",
            r#"
            ---@class WorkspaceChild : WorkspaceBase
            local WorkspaceChild = {}

            function WorkspaceChild.Run(value)
                workspace_member_value = value
            end
            "#,
        );

        let expected_contract = ws.ty("fun(value: string)");
        let module_index = ws.analysis.compilation.get_db().get_module_index();
        assert_ne!(
            module_index.get_workspace_id(other_file_id),
            module_index.get_workspace_id(main_file_id)
        );
        assert_eq!(
            module_index.get_workspace_id(main_file_id),
            module_index.get_workspace_id(caller_file_id)
        );
        let caller_workspace_id = module_index
            .get_workspace_id(caller_file_id)
            .expect("caller workspace");
        let inherited_members = find_members_with_key_in_workspace_for_file(
            ws.analysis.compilation.get_db(),
            &LuaType::Ref(LuaTypeDeclId::global("WorkspaceBase")),
            LuaMemberKey::Name("Run".into()),
            false,
            caller_workspace_id,
            caller_file_id,
        )
        .expect("visible inherited member");
        assert_eq!(
            ws.humanize_type(inherited_members[0].typ.clone()),
            ws.humanize_type(expected_contract)
        );

        let ty = ws.expr_ty("workspace_member_value");
        let expected = ws.ty("string");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    fn infer_scripted_class_param_from_workspace_super_edge(
        enable_isolation: Option<bool>,
        current_super: Option<(&str, &str)>,
    ) -> LuaType {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        if let Some(enable_isolation) = enable_isolation {
            emmyrc.workspace.enable_isolation = enable_isolation;
        }
        ws.update_emmyrc(emmyrc);

        let other_workspace = ws
            .virtual_url_generator
            .base
            .parent()
            .expect("virtual workspace parent")
            .join("other_workspace");
        ws.analysis.add_main_workspace(other_workspace.clone());
        let other_uri = lsp_types::Uri::parse_from_file_path(
            &other_workspace.join("lua/weapons/gmod_tool/stools/context_test.lua"),
        )
        .expect("other workspace uri");
        ws.analysis.update_file_by_uri(
            &other_uri,
            Some(
                r#"
                ---@class TOOL.context_test : WrongBase
                TOOL = {}
                "#
                .to_string(),
            ),
        );

        let (base_contract, class_contract) = current_super.map_or_else(
            || (
                "---@class WrongBase\n---@field CustomRun fun(value: string)\nlocal WrongBase = {}"
                    .to_string(),
                "---@class TOOL.context_test".to_string(),
            ),
            |(base, param_type)| {
                (
                    format!(
                        "---@class WrongBase\n---@field CustomRun fun(value: string)\nlocal WrongBase = {{}}\n---@class {base}\n---@field CustomRun fun(value: {param_type})\nlocal {base} = {{}}"
                    ),
                    format!("---@class TOOL.context_test : {base}"),
                )
            },
        );
        ws.def_file(
            "lua/weapons/gmod_tool/stools/context_test.lua",
            &format!(
                r#"
                {base_contract}
                {class_contract}
                TOOL = {{}}

                function TOOL.CustomRun(value)
                    inherited_workspace_param = value
                end
                "#
            ),
        );

        ws.expr_ty("inherited_workspace_param")
    }

    #[test]
    fn inherited_scripted_class_param_ignores_other_main_workspace_edges_with_isolation() {
        assert_eq!(
            infer_scripted_class_param_from_workspace_super_edge(Some(true), None),
            LuaType::Unknown
        );
    }

    #[test]
    fn inherited_scripted_class_param_uses_current_edge_with_isolation() {
        assert_eq!(
            infer_scripted_class_param_from_workspace_super_edge(
                Some(true),
                Some(("RightBase", "number")),
            ),
            LuaType::Number
        );
    }

    #[test]
    fn inherited_scripted_class_param_prefers_current_workspace_edge_without_isolation() {
        assert_eq!(
            infer_scripted_class_param_from_workspace_super_edge(
                Some(false),
                Some(("RightBase", "number")),
            ),
            LuaType::Number
        );
    }

    #[test]
    fn inherited_scripted_class_param_keeps_other_main_workspace_edges_by_default() {
        assert_eq!(
            infer_scripted_class_param_from_workspace_super_edge(None, None),
            LuaType::String
        );
    }

    #[test]
    fn inherited_member_contract_rejects_branch_realm_edges_at_every_hop() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let edge_source = |realm| {
            format!(
                r#"
            ---@class WrongBase
            ---@field Run fun(value: string)

            if {realm} then
                ---@class RealmMid : WrongBase
                local RealmMid = {{}}
            end
            "#
            )
        };
        let edge_uri = ws.virtual_url_generator.new_uri("sh_contract.lua");
        ws.analysis
            .update_file_by_uri(&edge_uri, Some(edge_source("CLIENT")))
            .expect("client edge file");
        ws.def_file(
            "sh_override.lua",
            r#"
            ---@class RightBase
            ---@field Run fun(value: number)

            ---@class RealmMid : RightBase
            local RealmMid = {}

            ---@class RealmChild : RealmMid
            local RealmChild = {}

            if SERVER then
                function RealmChild.Run(value)
                    realm_edge_param = value
                end
            end
            "#,
        );

        assert_eq!(ws.expr_ty("realm_edge_param"), LuaType::Number);
    }

    #[test]
    fn registered_entity_contract_uses_base_source_realm() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let registration_file_id = ws.def_file(
            "lua/sh_registration.lua",
            r#"
            ---@class WrongEntityBase
            ---@field Run fun(value: string)
            local WrongEntityBase = {}

            if CLIENT then
                DEFINE_BASECLASS("WrongEntityBase")
            end

            local ENT = {}
            scripted_ents.Register(ENT, "RealmRegisteredEntity")
            "#,
        );
        ws.def_file(
            "lua/sv_override.lua",
            r#"
            ---@class RightEntityBase
            ---@field Run fun(value: number)
            local RightEntityBase = {}

            ---@class RealmRegisteredEntity : RightEntityBase
            local RealmRegisteredEntity = {}

            function RealmRegisteredEntity.Run(value)
                registered_entity_realm_param = value
            end
            "#,
        );

        let db = ws.get_db_mut();
        let wrong_base = LuaType::Ref(LuaTypeDeclId::global("WrongEntityBase"));
        let wrong_edge = db
            .get_type_index()
            .get_super_type_entries(&LuaTypeDeclId::global("RealmRegisteredEntity"))
            .and_then(|entries| entries.iter().find(|entry| entry.value.typ == wrong_base))
            .expect("registered entity base edge");
        assert_eq!(wrong_edge.file_id, registration_file_id);
        assert_eq!(
            db.get_gmod_infer_index()
                .get_realm_at_offset(&registration_file_id, wrong_edge.value.source_range.start()),
            GmodRealm::Client
        );

        assert_eq!(ws.expr_ty("registered_entity_realm_param"), LuaType::Number);
    }

    #[test]
    fn inherited_member_contract_rejects_isolated_intermediate_edge() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.workspace.enable_isolation = true;
        ws.update_emmyrc(emmyrc);

        let other_workspace = ws
            .virtual_url_generator
            .base
            .parent()
            .expect("virtual workspace parent")
            .join("other_workspace");
        ws.analysis.add_main_workspace(other_workspace.clone());
        let other_uri =
            lsp_types::Uri::parse_from_file_path(&other_workspace.join("lua/foreign_contract.lua"))
                .expect("other workspace uri");
        ws.analysis.update_file_by_uri(
            &other_uri,
            Some(
                r#"
                ---@class WorkspaceMid : WorkspaceContract
                local WorkspaceMid = {}
                "#
                .to_string(),
            ),
        );
        ws.def_file(
            "lua/current_override.lua",
            r#"
            ---@class WorkspaceContract
            ---@field Run fun(value: string)
            local WorkspaceContract = {}

            ---@class WorkspaceMid
            local WorkspaceMid = {}

            ---@class WorkspaceChild : WorkspaceMid
            local WorkspaceChild = {}

            function WorkspaceChild.Run(value)
                isolated_intermediate_param = value
            end
            "#,
        );

        assert_eq!(ws.expr_ty("isolated_intermediate_param"), LuaType::Unknown);
    }

    fn infer_cross_workspace_inherited_callback_call_params(
        enable_isolation: bool,
    ) -> [LuaType; 3] {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.workspace.enable_isolation = enable_isolation;
        ws.update_emmyrc(emmyrc);

        let other_workspace = ws
            .virtual_url_generator
            .base
            .parent()
            .expect("virtual workspace parent")
            .join("other_workspace");
        ws.analysis.add_main_workspace(other_workspace.clone());
        let other_uri = lsp_types::Uri::parse_from_file_path(
            &other_workspace.join("lua/foreign_callback_edges.lua"),
        )
        .expect("other workspace uri");
        ws.analysis.update_file_by_uri(
            &other_uri,
            Some(
                r#"
                ---@class CallbackChild : CallbackBase
                local CallbackChild = {}

                ---@class GenericCallbackChild<T> : T
                local GenericCallbackChild = {}

                ---@class IndexCallbackChild : IndexCallbackBase
                local IndexCallbackChild = {}
                "#
                .to_string(),
            ),
        );
        ws.def_file(
            "lua/current_callback_calls.lua",
            r#"
            ---@class CallbackBase
            ---@field Run fun(callback: fun(value: string))
            local CallbackBase = {}
            ---@class CallbackChild
            local CallbackChild = {}

            ---@class GenericCallbackChild<T>
            local GenericCallbackChild = {}

            ---@class IndexCallbackBase
            ---@field [string] fun(callback: fun(value: string))
            local IndexCallbackBase = {}
            ---@class IndexCallbackChild
            local IndexCallbackChild = {}

            ---@type CallbackChild
            local direct
            direct.Run(function(value)
                direct_inherited_callback_param = value
            end)

            ---@type GenericCallbackChild<{ Run: fun(callback: fun(value: string)) }>
            local generic
            generic.Run(function(value)
                generic_inherited_callback_param = value
            end)

            ---@type IndexCallbackChild
            local indexed
            indexed.Run(function(value)
                index_inherited_callback_param = value
            end)
            "#,
        );

        [
            ws.expr_ty("direct_inherited_callback_param"),
            ws.expr_ty("generic_inherited_callback_param"),
            ws.expr_ty("index_inherited_callback_param"),
        ]
    }

    #[test]
    fn inherited_callback_calls_filter_every_isolated_super_path() {
        assert_eq!(
            infer_cross_workspace_inherited_callback_call_params(false),
            [LuaType::String, LuaType::String, LuaType::String]
        );
        assert_eq!(
            infer_cross_workspace_inherited_callback_call_params(true),
            [LuaType::Unknown, LuaType::Unknown, LuaType::Unknown]
        );
    }

    #[test]
    fn test_inherited_member_contract_respects_realm_visibility() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "sv_contract.lua",
            r#"
            ---@class RealmBase
            ---@field Run fun(value: number)
            "#,
        );
        ws.def_file(
            "cl_contract.lua",
            r#"
            ---@class RealmBase
            ---@field Run fun(value: string)
            "#,
        );
        ws.def_file(
            "cl_override.lua",
            r#"
            ---@class RealmChild : RealmBase
            local RealmChild = {}

            function RealmChild.Run(value)
                realm_member_value = value
            end
            "#,
        );

        let ty = ws.expr_ty("realm_member_value");
        let expected = ws.ty("string");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_gmod_hook_add_callback_params_infer_from_hook_name() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        let file_id = ws.def(
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

            hook.Add("AcceptInput", "Test", function(ent, input, activator, caller, value)
                gmod_hook_ent = ent
                gmod_hook_input = input
                gmod_hook_activator = activator
                gmod_hook_caller = caller
                gmod_hook_value = value
            end)
            "#,
        );

        {
            let metadata = ws
                .get_db_mut()
                .get_gmod_infer_index()
                .get_hook_file_metadata(&file_id)
                .expect("expected hook metadata for annotated hook.Add");
            let site = metadata
                .sites
                .iter()
                .find(|site| {
                    site.kind == GmodHookKind::Add
                        && site.hook_name.as_deref() == Some("AcceptInput")
                })
                .expect("expected AcceptInput hook metadata");
            assert_eq!(site.callback_arg_idx, Some(2));
        }

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

    #[test]
    fn gmod_hook_callback_uses_realm_compatible_populate_signature() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def_file(
            "annotations/populate_hooks.lua",
            r#"
            ---@class DTree_Node
            local DTree_Node = {}

            ---@class SpawnmenuContentPanel
            local SpawnmenuContentPanel = {}

            ---@class GM
            GM = {}

            ---@realm server
            ---@param unexpected string
            ---@param ignored number
            ---@param node string
            function GM:PopulateContent(unexpected, ignored, node) end

            ---@realm client
            ---@param content SpawnmenuContentPanel
            ---@param node DTree_Node
            function GM:PopulateContent(content, node) end
            "#,
        );
        ws.def_file(
            "lua/autorun/client/populate_hooks.lua",
            r#"
            hook.Add("PopulateContent", "test", function(content, node)
                inferred_populate_content = content
                inferred_populate_node = node
            end)

            hook.Add("UnregisteredPopulate", "test", function(node)
                unregistered_populate_node = node
            end)
            "#,
        );

        let content = ws.expr_ty("inferred_populate_content");
        let expected_content = ws.ty("SpawnmenuContentPanel");
        assert_eq!(
            ws.humanize_type(content),
            ws.humanize_type(expected_content)
        );
        let node = ws.expr_ty("inferred_populate_node");
        let expected_node = ws.ty("DTree_Node");
        assert_eq!(ws.humanize_type(node), ws.humanize_type(expected_node));
        assert_eq!(ws.expr_ty("unregistered_populate_node"), LuaType::Unknown);
    }

    #[test]
    fn test_gmod_hook_callback_params_infer_from_annotated_wrapper() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def(
            r#"
            ---@class Player
            local Player = {}

            ---@class GM
            GM = {}

            ---@param ply Player
            ---@return boolean
            function GM:PlayerSpawn(ply) end

            ---@[call_arg("gmod.hook", "add")]
            ---@param eventName string
            ---@[call_arg("gmod.hook", "callback")]
            ---@param callback function
            local function add_hook(eventName, callback, identifier) end

            add_hook("PlayerSpawn", function(ply)
                gmod_wrapper_hook_ply = ply
            end, "Test")
            "#,
        );

        let hook_ply = ws.expr_ty("gmod_wrapper_hook_ply");
        let player_type = ws.ty("Player");
        assert_eq!(ws.humanize_type(hook_ply), ws.humanize_type(player_type));
    }

    #[test]
    fn test_schema_hook_owner_infers_gamemode_hook_params() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def(
            r#"
            ---@class Player
            local Player = {}

            ---@class GM
            GM = {}

            ---@param client Player
            function GM:PlayerSpawn(client) end
            "#,
        );
        ws.def_file(
            "gamemodes/example-rp/schema/sh_hooks.lua",
            r#"
            function Schema:PlayerSpawn(client)
                schema_hook_client = client
            end
            "#,
        );

        let client_type = ws.expr_ty("schema_hook_client");
        let player_type = ws.ty("Player");
        assert_eq!(ws.humanize_type(client_type), ws.humanize_type(player_type));
    }
}
