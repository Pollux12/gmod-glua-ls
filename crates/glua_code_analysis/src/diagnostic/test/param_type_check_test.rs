#[cfg(test)]
mod test {
    use std::{ops::Deref, sync::Arc};

    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn test_issue_216() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@alias F1 fun(x: integer):integer
            do
                ---@type F1
                local test = function(x) return x + 1 end

                test("wrong type")
            end
        "#
        ));
    }

    #[test]
    fn test_issue_82() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@generic F: function
            ---@param _a F|integer
            ---@param _b? F
            ---@return F
            function foo(_a, _b)
                return _a
            end
            foo(function() end)
        "#
        ));
    }

    #[test]
    fn test_issue_75() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local a, b = pcall(string.rep, "a", "w")
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local a, b = pcall(string.rep, "a", 10000)
        "#
        ));
    }

    #[test]
    fn test_issue_85() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        ---@param a table | nil
        local function foo(a)
            a = a or {}
            _ = a.b
        end

        ---@param a table?
        local function _bar(a)
            a = a or {}
            _ = a.b
        end
        "#
        ));
    }

    #[test]
    fn test_issue_84() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        ---@param _a string[]
        local function bar(_a) end

        ---@param a? string[]?
        local function _foo(a)
            if not a then
                a = {}
            end

            bar(a)

            if not a then
                a = {}
            end

            bar(a)
        end
        "#
        ));
    }

    #[test]
    fn test_issue_83() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        ---@param _t table<any, string>
        local function foo(_t) end

        foo({})
        foo({'a'})
        foo({'a', 'b'})

        local a ---@type string[]
        foo(a)
        "#
        ));
    }

    #[test]
    fn test_issue_113() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@enum Baz
            local asd = {
                Foo = 0,
                Bar = 1,
                Baz = 2,
            }

            ---@param bob {a: Baz}
            function Foo(bob)
                return Bar(bob)
            end

            ---@param bob {a: Baz}
            function Bar(bob)
            end
        "#
        ));
    }

    #[test]
    fn test_issue_111() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        local Table = {}

        ---@param target table
        ---@param ... table
        ---@return table
        function Table.mergeInto(target, ...)
            -- Stuff
        end

        ---@param ... table
        ---@return table
        function Table.merge(...)
            return Table.mergeInto({}, ...)
        end
        "#
        ));
    }

    #[test]
    fn test_var_param_check() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
        ---@param target table
        ---@param ... table
        ---@return table
        function mergeInto(target, ...)
            -- Stuff
        end
        "#,
        );

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        mergeInto({}, 1)
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        mergeInto({}, {}, {})
        "#
        ));
    }

    #[test]
    fn test_issue_102() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        ---@param _kind '' | 'Nr' | 'Ln' | 'Cul'
        function foo(_kind) end

        for _, kind in ipairs({ '', 'Nr', 'Ln', 'Cul' }) do
            foo(kind)
        end
        "#
        ));
    }

    #[test]
    fn test_issue_95() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        local range ---@type { [1]: integer, [2]: integer }

        table.sort(range)
        "#
        ));
    }

    #[test]
    fn test_method_members_do_not_cause_param_type_mismatch_for_table_literals() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "libraries/sh_cami.lua",
            r#"
            CAMI = {}

            ---@class CAMI_PRIVILEGE
            ---@field Name string
            ---@field MinAccess "'user'" | "'admin'" | "'superadmin'"
            ---@field Description string?
            local CAMI_PRIVILEGE = {}

            function CAMI_PRIVILEGE:HasAccess(actor, target)
                return true
            end

            ---@param privilege CAMI_PRIVILEGE
            function CAMI.RegisterPrivilege(privilege)
            end
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            CAMI.RegisterPrivilege{
                Name = "DarkRP_SetLicense",
                MinAccess = "superadmin",
            }
            "#
        ));
    }

    #[test]
    fn test_function_fields_still_cause_param_type_mismatch_when_missing() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class CAMI_PRIVILEGE
            ---@field Name string
            ---@field HasAccess fun(): boolean

            ---@param privilege CAMI_PRIVILEGE
            local function register_privilege(privilege)
            end

            register_privilege({
                Name = "DarkRP_SetLicense",
            })
            "#
        ));
    }

    #[test]
    fn test_issue_135() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        ---@alias A
        ---| "number" # A number

        ---@param a A
        local function f(a)
        end

        f("number")
        "#
        ));
    }

    #[test]
    fn test_colon_call_and_not_colon_define() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Test
            local Test = {}

            ---@param a string
            function Test.name(a)
            end

            Test:name()
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Test
            local Test = {}

            ---@param ... any
            function Test.dots(...)
            end

            Test:dots("a", "b", "c")
        "#
        ));
    }

    #[test]
    fn test_issue_148() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local a = (''):format()
        "#
        ));
    }

    #[test]
    fn test_generic_dots_param() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local d = select(1, 1, 2, 3)
        "#
        ));
    }

    #[test]
    fn test_bool_as_type() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        --- @param _x string|true
        function foo(_x) end

        foo(true)
        "#
        ));
    }

    #[test]
    fn test_function() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param sorter function
            ---@return string[]
            local function getTableKeys(sorter)
                local keys = {}
                table.sort(keys, sorter)
                return keys
            end
        "#
        ));
    }

    #[test]
    fn test_table_array() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@generic K, V
                ---@param t table<K, V>
                ---@return table<V, K>
                local function revertMap(t)
                end

                ---@param arr any[]
                local function sortCallbackOfIndex(arr)
                    ---@type table<any, integer>
                    local indexMap = revertMap(arr)
                end
        "#
        ));
    }

    #[test]
    fn test_table_class() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@param t table
                local function bar(t)
                end

                ---@class D11.A

                ---@type D11.A|any
                local a

                bar(a)
        "#
        ));
    }

    #[test]
    fn test_table_1() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@param t table[]
                local function bar(t)
                end

                ---@type table|any
                local a

                bar(a)
        "#
        ));
    }

    #[test]
    fn test_pairs() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@diagnostic disable: missing-return
                ---@generic K, V
                ---@param t table<K, V> | V[] | {[K]: V}
                ---@return fun(tbl: any):K, std.NotNull<V>
                local function _pairs(t) end

                ---@class D10.A

                ---@type {[string]: D10.A, _id: D10.A}
                local a

                for k, v in _pairs(a) do
                end
        "#
        ));
    }

    #[test]
    fn test_issue_278() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        local a --- @type type|'callable'
        error(a)  -- expected `string` but found `(type|"callable")`
        "#
        ))
    }

    #[test]
    fn test_issue_696() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        local ty --- @type type|fun(x:any): boolean

        --- @param _ty fun(v:any):boolean
        local function validate(_ty) end

        if type(ty) == 'function' then
          validate(ty)
        end
        "#
        ));
    }

    #[test]
    fn test_4() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class D13.Meta
            ---@field __defineGet fun(self: self, key: string, f: fun(self: self): any)

            ---@class D13.Impl: D13.Meta
            local impl = {}

            impl:__defineGet("value", function(self)
                return 1
            end)

            "#
        ));
    }

    #[test]
    fn test_issue_286() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                local a --- @type boolean
                local b --- @type integer?
                local c = a and b or nil
                -- type of c is (nil|true), should be (integer|nil)
                local d = a and b
                -- type of d is (boolean|nil), should be (false|integer|nil)

                ---@param p integer?
                local function f1(p)
                end
                f1(c)

                ---@param p false|integer|nil
                local function f2(p)
                end
                f2(d)
        "#
        ));
    }

    #[test]
    fn test_issue_287() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@param a table
                local function f(a)
                end

                ---@type table?
                local a
                a = a or {}

                f(a)
        "#
        ));
    }

    #[test]
    fn test_issue_336() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            --- @param b string
            --- @param c? boolean
            --- @param d? string
            --- @overload fun(b: string, d: string)
            function foo(b, c, d) end

            foo('number', true)
        "#
        ));
    }

    #[test]
    fn test_issue_348() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                local a --- @type type|'a'
                string.len(a)
        "#
        ));
    }

    #[test]
    fn test_super() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@class py.ETypeMeta

                ---@class py.Vector3: py.ETypeMeta
                ---@class py.FVector3: py.ETypeMeta

                ---@alias Point.HandleType py.FVector3

                ---@class py.Point: py.Vector3

                ---@param point py.Point
                local function test(point)
                end

                ---@type Point.HandleType
                local handle

                test(handle)
        "#
        ));
    }

    #[test]
    fn test_union_type() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class py.Area
            ---@class py.RecArea: py.Area
            ---@class py.CirArea: py.Area

            ---@param a py.Area
            local function test(a)
            end

            ---@type py.RecArea | py.CirArea
            local a

            test(a)
        "#
        ));
    }

    #[test]
    fn test_super_1() {
        let mut ws = VirtualWorkspace::new();
        // Integer literals are not implicitly branded as py.SlotType.
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@class py.SlotType: integer

                ---@param a py.SlotType
                local function test(a)
                end

                ---@type 0|1
                local a

                test(a)
        "#
        ));
    }

    #[test]
    fn test_alias_union_enum() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@alias EventType
                ---| GlobalEventType
                ---| UIEventType

                ---@enum UIEventType
                local UIEventType = {
                    ['UI_CREATE'] = "ET_UI_PREFAB_CREATE_EVENT",
                    ['UI_DELETE'] = "ET_UI_PREFAB_DEL_EVENT",
                }

                ---@enum GlobalEventType
                local GlobalEventType = {
                    ['GAME_INIT'] = "ET_GAME_INIT",
                    ['GAME_PAUSE'] = "ET_GAME_PAUSE",
                }

                ---@param event_name string
                local function get_py_event_name(event_name)
                end

                ---@param a EventType
                local function test(a)
                    get_py_event_name(a)
                end

        "#
        ));
    }

    #[test]
    fn test_alias_union_enum_2() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@alias EventType
                ---| GlobalEventType
                ---| UIEventType

                ---@enum UIEventType
                local UIEventType = {
                    ['UI_CREATE'] = "ET_UI_PREFAB_CREATE_EVENT",
                    ['UI_DELETE'] = "ET_UI_PREFAB_DEL_EVENT",
                }

                ---@enum GlobalEventType
                local GlobalEventType = {
                    ['GAME_INIT'] = 1,
                    ['GAME_PAUSE'] = "ET_GAME_PAUSE",
                }

                ---@param event_name string
                local function get_py_event_name(event_name)
                end

                ---@param a EventType
                local function test(a)
                    get_py_event_name(a)
                end

        "#
        ));
    }

    #[test]
    fn test_empty_class() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@class D4.A: table<integer, string>

                ---@param lua_conf D4.A
                local function enable_global_lua_trigger(lua_conf) end

                ---@return { on_event: fun(trigger: table,  actor, data), [integer]: string }
                function new_global_trigger() end

                local a = new_global_trigger()

                enable_global_lua_trigger(a)
        "#
        ));
    }

    #[test]
    fn test_super_and_enum_1() {
        let mut ws = VirtualWorkspace::new();
        // Integer enums are not implicitly branded as py.AbilityType.
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@enum AbilityType
                local AbilityType = {
                    HIDE   = 0,
                    NORMAL = 1,
                    COMMON = 2,
                    HERO   = 3,
                }
                ---@class py.AbilityType: integer

                ---@param ability_type py.AbilityType
                local function a(ability_type) end

                ---@param type AbilityType
                local function get(type)
                    local py_list = a(type)
                end
        "#
        ));
    }

    #[test]
    fn test_super_and_enum_2() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@enum AbilityType
                local AbilityType = {
                    HIDE   = "a",
                    NORMAL = 1,
                    COMMON = 2,
                    HERO   = 3,
                }
                ---@class py.AbilityType: integer

                ---@param ability_type py.AbilityType
                local function a(ability_type) end

                ---@param type AbilityType
                local function get(type)
                    local py_list = a(type)
                end
        "#
        ));
    }

    #[test]
    fn test_generic_array() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class LocalTimer

            ---@generic V
            ---@param list V[]
            ---@return integer
            local function sort(list) end

            ---@type { need_sort: true?, [integer]: LocalTimer }
            local queue = {}
            sort(queue)
        "#
        ));
    }

    #[test]
    fn test_function_union() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@class (partial) D21.A
                local M

                ---@alias EventType
                ---| GlobalEventType
                ---| UIEventType

                ---@enum UIEventType
                local UIEventType = {
                    ['UI_CREATE'] = "ET_UI_PREFAB_CREATE_EVENT",
                }
                ---@enum GlobalEventType
                local GlobalEventType = {
                    ['GAME_INIT'] = "ET_GAME_INIT",
                }

                ---@param event_type EventType
                function M:event(event_type)
                end

                ---@class (partial) D21.A
                ---@field event fun(self: self, event: "游戏-初始化")

                ---@param p string
                local function test(p)
                    M:event(p)
                end
        "#
        ));
    }

    #[test]
    fn test_function_union_2() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@class (partial) D21.A
                local M

                ---@alias EventType
                ---| GlobalEventType
                ---| UIEventType

                ---@enum UIEventType
                local UIEventType = {
                    ['UI_CREATE'] = "ET_UI_PREFAB_CREATE_EVENT",
                }
                ---@enum GlobalEventType
                local GlobalEventType = {
                    ['GAME_INIT'] = "ET_GAME_INIT",
                }

                ---@param event_type EventType
                function M:event(event_type)
                end

                ---@class (partial) D21.A
                ---@field event fun(self: self, event: "游戏-初始化")

                ---@param p EventType
                local function test(p)
                    M:event(p)
                end
        "#
        ));
    }

    #[test]
    fn test_function_union_meta() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.analysis.emmyrc.as_ref().clone();
        emmyrc.strict.meta_override_file_define = false;
        ws.analysis.update_config(Arc::new(emmyrc));

        ws.def(
            r#"
                ---@meta
                ---@class (partial) D21.A
                ---@field event fun(self: self, event: "游戏-初始化")
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@class (partial) D21.A
                local M

                ---@alias EventType
                ---| GlobalEventType
                ---| UIEventType

                ---@enum UIEventType
                local UIEventType = {
                    ['UI_CREATE'] = "ET_UI_PREFAB_CREATE_EVENT",
                }
                ---@enum GlobalEventType
                local GlobalEventType = {
                    ['GAME_INIT'] = "ET_GAME_INIT",
                }

                ---@param event_type EventType
                function M:event(event_type)
                end

                ---@param p EventType
                local function test(p)
                    M:event(p)
                end
        "#
        ));
    }

    #[test]
    fn test_function_self() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@class D23.A

                ---@generic Extends: string
                ---@param init? fun(self: self, super: Extends)
                local function extends(init)
                end

                ---@generic Super: string
                ---@param super? `Super`
                ---@param superInit? fun(self: D23.A, super: Super, ...)
                local function declare(super, superInit)
                    extends(superInit)
                end
        "#
        ));
    }

    #[test]
    fn test_self_1() {
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
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local A = require("1")

            ---@class D31.B
            local B = {}

            function B:__init()
                self.originalExecute = A.execute
                A.execute = function(trg, ...)
                    self.originalExecute(trg, ...)
                end
            end
        "#
        ));
    }

    #[test]
    fn test_flow_alias() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = ws.analysis.get_emmyrc().deref().clone();
        emmyrc.strict.array_index = false;
        ws.analysis.update_config(Arc::new(emmyrc));

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@class Trigger
                ---@alias Trigger.CallBack fun(trg: Trigger, ...): any, any, any, any

                ---@param callback Trigger.CallBack
                local function event(callback)
                end

                local function core_subscribe(...)
                    ---@type Trigger.CallBack
                    local callback
                    local nargs = select('#', ...)
                    if nargs == 1 then
                        callback = ...
                    elseif nargs > 1 then
                        extra_args = { ... }
                        callback = extra_args[nargs]
                    end

                    local b = event(callback)
                end
        "#
        ));
    }

    #[test]
    fn test_issue_487() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param start string
            ---@return boolean
            function string:startswith(start)
                return self:sub(1, #start) == start
            end
        "#
        ));
    }

    #[test]
    fn test_int() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param count integer
            local function loop_count(count)
            end

            loop_count(45 / 3)
        "#
        ));
    }

    #[test]
    fn test_int_to_alias() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.analysis.get_emmyrc().deref().clone();
        emmyrc.strict.doc_base_const_match_base_type = true;
        ws.analysis.update_config(Arc::new(emmyrc));

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@alias IdAlias
                ---| 311000001
                ---| 311000002

                ---@param id IdAlias
                local function f(id)
                end

                ---@type integer
                local a
                f(a)
        "#
        ));
    }

    #[test]
    fn test_enum_value_matching() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.analysis.get_emmyrc().deref().clone();
        emmyrc.strict.doc_base_const_match_base_type = true;
        ws.analysis.update_config(Arc::new(emmyrc));

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@enum SlotType
                local SlotType = {
                    bag = 0,
                    item = 1,
                }

                ---@enum ConstSlotType
                local ConstSlotType = {
                    ['NOT_IN_BAG'] = -1,
                    ['PKG'] = 0,
                    ['BAR'] = 1,
                }

                ---@param type ConstSlotType
                local function get_item_by_slot(type)
                end

                ---@param field SlotType
                local function bind_unit_slot(field)
                    get_item_by_slot(field)
                end"#
        ));
    }

    #[test]
    fn test_enum_value_matching_2() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@enum DamageType
                local DamageType = {
                    ['物理'] = "物理",
                }
                for _, damageType in pairs(DamageType) do end
            "#
        ));
    }

    #[test]
    fn test_super_type_match() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@class UnitKey: integer

                ---@alias IdAlias
                ---| 10101

                ---@param key IdAlias
                local function get(key)
                end

                ---@type UnitKey
                local key

                get(key)
            "#
        ));
    }

    #[test]
    fn test_self() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@class test
                local A

                function A:stop()
                end

                local stop = A.stop
                stop(A)
            "#
        ));
    }

    #[test]
    fn test_generic_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class ObserverParams<T>
                ---@field next fun(value: T)
                ---@field errorResume? fun(error: any)


                ---@class Observer<T>
                local Observer = {}

                ---@param observer ObserverParams<T>
                function Observer:subscribe(observer)
                end
        "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type Observer<number>
            local observer

            observer:subscribe({
                next = function(value)
                    print(value)
                end
            })
            "#
        ));
    }

    #[test]
    fn test_issue_573() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                --- @class A
                --- @field [integer] string
                --- @field data any

                --- @param a string[]
                function takesArray(a) end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                takesArray({} --[[@as A]])
            "#
        ));
    }

    #[test]
    fn test_issue_574() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                --- @param x { y: integer } & { z: string }
                function foo(x) end
            "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
               foo({y = "", z = ""})
            "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
               foo({y = 1, z = ""})
            "#
        ));
    }

    #[test]
    fn test_meta_pairs() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
                ---@class RingBufferSpan<T>
                local RingBufferSpan

                ---@return fun(): integer, T
                function RingBufferSpan:__pairs()
                end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local pairs = pairs

            ---@type RingBufferSpan
            local span

            for k, v in pairs(span) do
            end
            "#
        ));

        // 测试泛型
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type RingBufferSpan<number>
            local span

            for k, v in pairs(span) do
                A = v
            end
            "#
        ));
        let a_ty = ws.expr_ty("A");
        let expected = ws.ty("number");
        assert_eq!(a_ty, expected);
    }

    #[test]
    fn test_generic_union_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class Params<T>
                ---@field next fun(value: T)
                ---@field error? fun(error: any)

                ---@generic T
                ---@param params fun(value: T) | Params<T>
                function test(params)
                end

            "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                test({})
            "#
        ));
    }

    #[test]
    fn test_alias_branch_label_flow() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "test.lua",
            r#"
            ---@alias EditorAttrTypeAlias
            ---| 'ATTR_BASE'
            ---| 'ATTR_BASE_RATIO'
            ---| 'ATTR_ALL_RATIO'

            ---@param attr_element string
            function test(attr_element) end
        "#,
        );

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param attr_type EditorAttrTypeAlias
            function add_attr(attr_type)
                if attr_type ~= 'ATTR_BASE' then
                end
                test(attr_type)
            end
        "#
        ));
    }

    #[test]
    fn test_self_contain_tpl() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "test.lua",
            r#"
                ---@class Observable<T>
                Observable = {}

                ---@param ... Observable<any>
                function zip(...)
                end

        "#,
        );

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            function Observable:test()
                zip(self)
            end
        "#
        ));
    }

    #[test]
    fn test_issue_841() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        --- @class B
        --- @field cmd string

        --- @class A: B
        --- @field cmd? string

        --- @param x A
        local function foo(x)
        end

        foo({})
        "#,
        ));
    }

    #[test]
    fn test_fix_issue_844() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
        ---@alias Tester fun(customTesters: Tester[]): boolean?

        ---@generic V
        ---@param t V[]
        ---@return fun(tbl: any):int, V
        function ipairs(t) end
        "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param newTesters Tester[]
            local function addMatchers(newTesters)
                for _, tester in ipairs(newTesters) do
                end
            end
        "#
        ));
    }

    #[test]
    fn test_pairs_1() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
        ---@param value string
        function aaaa(value)
        end

        ---@generic K, V
        ---@param t {[K]: V} | V[]
        ---@return fun(tbl: any):K, V
        function pairs(t) end
        "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type {[string]: number}
            local matchers = {}
            for _, matcher in pairs(matchers) do
                aaaa(matcher)
            end
        "#
        ));
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@alias MatchersObject {[string]: number}
            ---@type MatchersObject
            local matchers = {}
            for _, matcher in pairs(matchers) do
                aaaa(matcher)
            end
        "#
        ));
    }

    #[test]
    fn test_keyof() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class SuiteHooks
            ---@field beforeAll string
            ---@field afterAll string

            ---@param name keyof SuiteHooks
            function test(name)
            end
        "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            test("a")
        "#,
        ));
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            test("beforeAll")
        "#,
        ));
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type keyof SuiteHooks
            local name
            test(name)
        "#,
        ));
    }

    #[test]
    fn test_origin_self() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Runner
            ---@field onCollectStart? fun(self:self)

            ---@type Runner
            runner = {}

        "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local originOnCollectStart = runner.onCollectStart
            runner.onCollectStart = function(self)
                originOnCollectStart(self)
            end
        "#,
        ));
    }

    #[test]
    fn test_issue_896() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.strict.array_index = false;
        ws.update_emmyrc(emmyrc);

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@alias MyFnA fun(): number
            ---@alias MyFnB<T> fun(): T
            ---@alias MyFnC<T> fun(): number

            local aaa ---@type MyFnA[]
            ---@param aaa MyFnA[]
            local function AAA(aaa) return aaa end
            local _ = AAA(aaa)

            local bbb1 ---@type (MyFnB<number>)[]
            local bbb2 = { function() return 1 end } ---@type (MyFnB<number>)[]
            ---@param bbb (MyFnB<number>)[]
            local function BBB(bbb) return bbb end
            local _ = BBB(bbb1)
            local _ = BBB(bbb2)

            local ccc1 ---@type (MyFnC<number>)[]
            local ccc2 = { function() return 1 end } ---@type (MyFnC<number>)[]
            ---@param ccc (MyFnC<number>)[]
            local function CCC(ccc) return ccc end
            local _ = CCC(ccc1)
            local _ = CCC(ccc2)
        "#,
        ));
    }

    /// Regression test: calling a base-class method stored in a local variable with a
    /// subclass `self` should NOT produce a `param-type-mismatch` diagnostic.
    ///
    /// Pattern:
    ///   local GetNWEntity = EntityMeta.GetNWEntity
    ///   function PlayerMeta:Test()
    ///     return GetNWEntity(self, "key", nil)  -- self is Player, method expects Entity
    ///   end
    ///
    /// Player extends Entity, so this is valid. When the method is defined in a separate
    /// file (e.g. API annotations), `get_call_source_type` used to infer the prefix
    /// expression cross-file, which failed and fell back to `SelfInfer`. The resulting
    /// `type_check(SelfInfer, Player)` always failed, producing a spurious diagnostic.
    #[test]
    fn test_no_false_positive_subclass_self_via_local_var() {
        let mut ws = VirtualWorkspace::new();

        // Set up Entity and Player in a separate file to simulate cross-file member lookup.
        ws.def(
            r#"
            ---@class Entity
            local EntityMeta = {}

            ---@param key string
            ---@param fallback any
            ---@return any
            function EntityMeta:GetNWEntity(key, fallback) end

            ---@param key string
            ---@param fallback integer
            ---@return integer
            function EntityMeta:GetNWInt(key, fallback) end

            ---@class Player: Entity
            "#,
        );

        // Player (subclass of Entity) self via local method variable — no diagnostic expected.
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local EntityMeta = {} ---@type Entity
            local PlayerMeta = {} ---@type Player

            local GetNWEntity = EntityMeta.GetNWEntity

            do
                local GetNWInt = EntityMeta.GetNWInt

                function PlayerMeta:GlideGetVehicle()
                    return GetNWEntity(self, "GlideVehicle", nil)
                end

                function PlayerMeta:GlideGetSeatIndex()
                    return GetNWInt(self, "GlideSeatIndex", 0)
                end
            end
            "#,
        ));
    }

    /// Complementary test: passing a completely unrelated type should still raise the
    /// diagnostic.
    #[test]
    fn test_wrong_type_for_self_still_errors() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Entity
            local EntityMeta = {}

            ---@param key string
            ---@param fallback any
            ---@return any
            function EntityMeta:GetNWEntity(key, fallback) end
            "#,
        );

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local EntityMeta = {} ---@type Entity
            local GetNWEntity = EntityMeta.GetNWEntity

            GetNWEntity("not_an_entity", "GlideVehicle", nil)
            "#,
        ));
    }

    #[test]
    fn test_no_false_positive_realm_specific_member_overload() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "lua/glide/client/network.lua",
            r#"
            Glide = Glide or {}

            ---@param commandId number
            ---@param handler fun(len: number)
            function Glide.AddCommandHandler(commandId, handler)
            end
            "#,
        );
        ws.def_file(
            "lua/glide/server/network.lua",
            r#"
            Glide = Glide or {}
            Glide.Repair = Glide.Repair or {}

            ---@class Player

            ---@param ply Player
            function Glide.Repair.StartSession(ply)
            end

            ---@param commandId number
            ---@param handler fun(ply: Player)
            function Glide.AddCommandHandler(commandId, handler)
            end
            "#,
        );

        ws.enable_check(DiagnosticCode::ParamTypeMismatch);
        let file_id = ws.def_file(
            "lua/glide/server/repair_network.lua",
            r#"
            Glide.AddCommandHandler(1, function(ply)
                Glide.Repair.StartSession(ply)
            end)
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::ParamTypeMismatch.get_name().to_string(),
        ));
        assert!(!diagnostics.iter().any(|diag| diag.code == code));
    }

    #[test]
    fn test_never_param_no_mismatch() {
        let mut ws = VirtualWorkspace::new();
        // Passing a `never` typed argument should not produce param-type-mismatch.
        // `never` arises from type inference limitations, not real code errors.
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@param x integer
                local function foo(x) end

                ---@type never
                local val
                foo(val)
            "#
        ));
    }

    #[test]
    fn test_inferred_dynamic_index_arg_is_lenient_by_default() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@class InputGroups
                ---@field keyboard table

                ---@param tbl table
                local function SortedPairs(tbl) end

                ---@type InputGroups
                local inputGroups = { keyboard = {} }

                local groupId = ...
                local actions = inputGroups[groupId]
                SortedPairs(actions)
            "#
        ));
    }

    #[test]
    fn test_inferred_dynamic_index_arg_strict_flag_restores_warning() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.strict.inferred_type_mismatch = true;
        ws.update_emmyrc(emmyrc);

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@class InputGroups
                ---@field keyboard table

                ---@param tbl table
                local function SortedPairs(tbl) end

                ---@type InputGroups
                local inputGroups = { keyboard = {} }

                local groupId = ...
                local actions = inputGroups[groupId]
                SortedPairs(actions)
            "#
        ));
    }

    #[test]
    fn test_closure_param_from_function_call() {
        let mut ws = VirtualWorkspace::new();

        // Closure param should be inferred from the function's param type
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Player
            local Player = {}
            function Player:Nick() return "" end

            ---@param handler fun(ply: Player)
            function RegisterHandler(handler) end

            RegisterHandler(function(ply)
                local name = ply:Nick()
            end)
        "#
        ));
    }

    #[test]
    fn test_closure_param_cross_file() {
        let mut ws = VirtualWorkspace::new();

        // Define the function with handler param in one file
        ws.def_file(
            "network.lua",
            r#"
            ---@class Player
            local Player = {}
            function Player:Nick() return "" end

            Glide = Glide or {}

            ---@param commandId number
            ---@param handler fun(ply: Player)
            function Glide.AddCommandHandler(commandId, handler) end
        "#,
        );

        // Use the closure in another file (mimicking addon pattern)
        // Note: Glide = Glide or {} in BOTH files, just like the addon
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            Glide = Glide or {}

            ---@param ply Player
            local function Validate(ply) end

            Glide.AddCommandHandler(1, function(ply)
                Validate(ply)
            end)
        "#
        ));
    }

    #[test]
    fn test_dot_call_self_infer_local_var() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Player
            local PlayerMeta = {}

            ---@param vehicle Entity
            function PlayerMeta:EnterVehicle(vehicle)
            end

            ---@class GlideNS
            Glide = Glide or {}

            Glide._OriginalEnterVehicle = Glide._OriginalEnterVehicle or PlayerMeta.EnterVehicle
            local EnterVehicle = Glide._OriginalEnterVehicle

            function PlayerMeta:TestMethod(vehicle)
                local seat = vehicle

                return EnterVehicle(self, seat)
            end
        "#
        ));
    }

    #[test]
    fn test_and_or_chain_false_suppression() {
        let mut ws = VirtualWorkspace::new();
        // Idiomatic Lua pattern: require(a and "x" or b and "y")
        // The `and` operator produces `false | "literal"` when left operand may be falsy.
        // The `false` is an inference artifact that should be stripped for diagnostic purposes.
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                local moo = true
                local tmsql = true
                local preferred = "mysqloo"

                ---@param name string
                local function require_module(name)
                end

                require_module(
                    moo and tmsql and preferred or
                    moo and "mysqloo" or
                    tmsql and "tmysql4"
                )
        "#
        ));
    }

    #[test]
    fn test_and_or_chain_false_suppression_real_mismatch() {
        let mut ws = VirtualWorkspace::new();
        // When remaining types after stripping false DON'T match, diagnostic should still fire.
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                local cond = true

                ---@param n integer
                local function f(n)
                end

                f(cond and "not_an_integer")
        "#
        ));
    }

    #[test]
    fn test_and_or_chain_false_suppression_annotated_not_stripped() {
        let mut ws = VirtualWorkspace::new();
        // When the expression involves annotated types, the false stripping should NOT apply
        // because expr_has_inferred_type returns false for annotated operands.
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@type boolean
                local cond = true

                ---@param s string
                local function f(s)
                end

                f(cond and "valid")
        "#
        ));
    }

    #[test]
    fn test_and_or_simple_false_suppression() {
        let mut ws = VirtualWorkspace::new();
        // Simple case: `x and "literal"` should not warn when x is unannotated.
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                local has_module = true

                ---@param name string
                local function require_module(name)
                end

                require_module(has_module and "mymodule")
        "#
        ));
    }

    #[test]
    fn test_and_or_nil_and_false_combined() {
        let mut ws = VirtualWorkspace::new();
        // Both nil and false should be stripped together.
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                local a = nil
                local b = true

                ---@param s string
                local function f(s)
                end

                f(a and "first" or b and "second")
        "#
        ));
    }

    #[test]
    fn test_nullable_number_literal_union_accepted_as_number() {
        let mut ws = VirtualWorkspace::new();
        // Test 1: direct indexing works
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param delay number
            local function takesNumber(delay) end

            local delays = { 0.5, 1, 2, 3, 5 }
            local delay = delays[1]
            takesNumber(delay)
            "#
        ));
        // Test 2: nullable number literal union via annotation
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param delay number
            local function takesNumber(delay) end

            ---@type (0.5|1|2|3|5)?
            local delay
            takesNumber(delay)
            "#
        ));
    }

    #[test]
    fn test_localized_vector_mul_method_call_from_find_meta_table() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class VMatrix

            ---@class Vector
            local Vector = {}

            ---@overload fun(multiplier: Vector)
            ---@overload fun(matrix: VMatrix)
            ---@param multiplier number
            function Vector:Mul(multiplier) end

            ---@generic T: table
            ---@param metaName `T`
            ---@return T
            function FindMetaTable(metaName) end
            "#,
        );

        ws.enable_check(DiagnosticCode::ParamTypeMismatch);
        let file_id = ws.def(
            r#"
            local VectorMul = FindMetaTable("Vector").Mul
            local velUDt = 0
            local scratchVec2 = {} ---@type Vector
            VectorMul(scratchVec2, velUDt)
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::ParamTypeMismatch.get_name().to_string(),
        ));
        let messages: Vec<_> = diagnostics
            .iter()
            .filter(|diag| diag.code == code)
            .map(|diag| diag.message.clone())
            .collect();

        assert!(messages.is_empty(), "{}", messages.join(" || "));
    }

    #[test]
    fn test_localized_vector_mul_method_call_direct_table_member() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class VMatrix

            ---@class Vector
            local Vector = {}

            ---@overload fun(multiplier: Vector)
            ---@overload fun(matrix: VMatrix)
            ---@param multiplier number
            function Vector:Mul(multiplier) end
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local VectorMul = Vector.Mul
            local velUDt = 0
            local scratchVec2 = {} ---@type Vector
            VectorMul(scratchVec2, velUDt)
            "#,
        ));
    }

    #[test]
    fn test_localized_vector_mul_method_call_from_find_meta_table_direct_return() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class VMatrix

            ---@class Vector
            local Vector = {}

            ---@overload fun(multiplier: Vector)
            ---@overload fun(matrix: VMatrix)
            ---@param multiplier number
            function Vector:Mul(multiplier) end

            ---@param metaName string
            ---@return (definition) Vector|nil
            function FindMetaTable(metaName) end
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local VectorMul = FindMetaTable("Vector").Mul
            local velUDt = 0
            local scratchVec2 = {} ---@type Vector
            VectorMul(scratchVec2, velUDt)
            "#,
        ));
    }

    #[test]
    fn test_localized_vector_mul_method_call_from_definition_receiver() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class VMatrix

            ---@class Vector
            local Vector = {}

            ---@overload fun(multiplier: Vector)
            ---@overload fun(matrix: VMatrix)
            ---@param multiplier number
            function Vector:Mul(multiplier) end
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type (definition) Vector
            local VecMeta

            local VectorMul = VecMeta.Mul
            local velUDt = 0
            local scratchVec2 = {} ---@type Vector
            VectorMul(scratchVec2, velUDt)
            "#,
        ));
    }

    #[test]
    fn test_localized_vector_mul_method_call_from_definition_receiver_no_overload() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Vector
            local Vector = {}

            ---@param multiplier number
            function Vector:Mul(multiplier) end
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type (definition) Vector
            local VecMeta

            local VectorMul = VecMeta.Mul
            local velUDt = 0
            local scratchVec2 = {} ---@type Vector
            VectorMul(scratchVec2, velUDt)
            "#,
        ));
    }

    #[test]
    fn test_type_guard_alias_assignment_or_chain_narrows_to_number() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param reason number|string|nil
            local function invalidate_session(reason)
                ---@param code number?
                local function end_session(code)
                end

                local reason_code = reason
                if type(reason) == "string" then
                    reason_code = 1
                end

                reason_code = reason_code or 1
                end_session(reason_code)
            end
            "#,
        ));
    }

    #[test]
    fn test_inherited_entity_array_in_union_param() {
        let mut ws = VirtualWorkspace::new();
        // Test that inherited Entity classes work correctly when passed in an array
        // to a function with Entity|Entity[] union parameter type
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Entity
            ---@field Activate function
            
            ---@class BaseEntity : Entity
            ---@field baseField number
            
            ---@class MyEntity : BaseEntity  
            ---@field myField number
            
            ---@param filter Entity|Entity[]|function
            local function testFunc(filter) end
            
            local myInstance = {} ---@type MyEntity
            -- This should work - MyEntity inherits from Entity through BaseEntity
            testFunc({myInstance})
        "#
        ));
    }

    #[test]
    fn test_simple_type_mismatch() {
        let mut ws = VirtualWorkspace::new();
        // Verify basic type checking works
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param x number
            local function test(x) end
            
            test("string")
        "#
        ));
    }

    #[test]
    fn test_invalid_type_in_entity_array_still_reports_error() {
        let mut ws = VirtualWorkspace::new();
        // Test that passing invalid types in an array to Entity|Entity[] param
        // reports param-type-mismatch (not MissingFields, but actual type mismatch)
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Entity
            
            ---@param filter Entity|Entity[]|function
            local function testFunc(filter) end
            
            -- This should report an error - number is not compatible with Entity
            testFunc({123})
        "#
        ));
    }

    #[test]
    fn test_std_loadfile_mode_and_env_align_with_load_types() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local chunk, err = loadfile("autorun/server/sv_test.lua", "bt", _G)
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local chunk, err = loadfile("autorun/server/sv_test.lua", "x", _G)
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local chunk, err = loadfile("autorun/server/sv_test.lua", "bt", 1)
        "#
        ));
    }

    #[test]
    fn test_std_debug_setlocal_requires_integer_index() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local name = debug.setlocal(1, 1, 1)
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local name = debug.setlocal(1, "x", 1)
        "#
        ));
    }

    #[test]
    fn test_open_class_param_allows_extra_fresh_table_fields() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class OpenOptions
            ---@field mode string

            ---@param options OpenOptions
            local function useOptions(options) end

            useOptions({
                mode = "fast",
                extra = 1,
            })
        "#
        ));
    }

    #[test]
    fn test_open_class_param_allows_extra_nonfresh_table_fields() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class OpenOptionsFromVariable
            ---@field mode string

            ---@param options OpenOptionsFromVariable
            local function useOptions(options) end

            local options = {
                mode = "fast",
                extra = 1,
            }

            useOptions(options)
        "#
        ));
    }

    #[test]
    fn test_exact_class_param_rejects_extra_fresh_table_fields() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class (exact) ExactOptions
            ---@field mode string

            ---@param options ExactOptions
            local function useOptions(options) end

            useOptions({
                mode = "fast",
                extra = 1,
            })
        "#
        ));
    }

    #[test]
    fn test_exact_class_dynamic_key_member_is_not_required_for_fresh_table_param() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@alias DisplayMode
            ---| "compact"
            ---| "expanded"

            ---@class (exact) RenderOptions
            ---@field size? integer
            ---@field weight? number
            ---@field mode? DisplayMode

            ---@param baseOptions RenderOptions?
            ---@return RenderOptions
            local function copyOptions(baseOptions)
                ---@type RenderOptions
                local options = {}

                if baseOptions ~= nil then
                    for key, value in pairs(baseOptions) do
                        options[key] = value
                    end
                end

                return options
            end

            ---@param options? RenderOptions
            local function render(options) end

            render({
                mode = "expanded",
                size = 1,
                weight = 0,
            })
        "#
        ));
    }

    #[test]
    fn test_structural_object_param_rejects_extra_fresh_table_fields() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param options { mode: string }
            local function useOptions(options) end

            useOptions({
                mode = "fast",
                extra = 1,
            })
        "#
        ));
    }

    #[test]
    fn test_structural_object_param_allows_extra_nonfresh_table_fields() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param options { mode: string }
            local function useOptions(options) end

            local options = {
                mode = "fast",
                extra = 1,
            }

            useOptions(options)
        "#
        ));
    }

    #[test]
    fn test_broad_table_union_member_accepts_returned_object() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param options { camera?: "2D"|table }
            local function render(options) end

            local function makeCamera()
                return {
                    x = 0,
                    y = 0,
                    mode = "3D",
                }
            end

            render({
                camera = makeCamera(),
            })
        "#
        ));
    }
}
