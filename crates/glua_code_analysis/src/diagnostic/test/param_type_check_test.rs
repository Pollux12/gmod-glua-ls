#[cfg(test)]
mod test {
    use std::{ops::Deref, sync::Arc};

    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};

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
    fn test_mutual_alias_type_check_does_not_overflow() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@alias AliasA AliasB
            ---@alias AliasB AliasA

            ---@param value string
            local function takesString(value) end

            ---@type AliasA
            local value

            takesString(value)
        "#
        ));
    }

    #[test]
    fn test_correlated_overload_params_narrow_after_body_normalization() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param value number
            local function takesNumber(value) end

            ---@return number
            local function findIndex() end

            ---@param value any
            ---@return TypeGuard<string>
            function isstring(value) end

            ---@overload fun(slot: string)
            ---@overload fun(slot: nil, name: string)
            ---@param slot number
            ---@param name string
            local function wrapper(slot, name)
                if isstring(slot) and not name then
                    name = slot
                    slot = findIndex()
                elseif not slot and isstring(name) then
                    slot = findIndex()
                end

                takesNumber(slot)
            end

            wrapper(1, "Age")
            wrapper("Age")
            wrapper(nil, "Age")
            "#
        ));
    }

    #[test]
    fn test_overload_param_string_slot_without_normalization_still_reports() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param value number
            local function takesNumber(value) end

            ---@overload fun(slot: string)
            ---@param slot number
            local function wrapper(slot)
                takesNumber(slot)
            end

            wrapper(1)
            wrapper("Name")
            "#
        ));
    }

    #[test]
    fn test_dtvar_string_slot_with_name_still_reports() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@overload fun(type: string, name: string)
            ---@overload fun(type: string, slot: nil, name: string)
            ---@param type string
            ---@param slot number
            ---@param name string
            local function DTVar(type, slot, name) end

            DTVar("Float", "bad", "Name")
            "#
        ));
    }

    #[test]
    fn test_correlated_overload_params_forward_to_colon_method_dot_call() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Entity
            local Entity = {}

            ---@overload fun(type: string, name: string)
            ---@overload fun(type: string, slot: nil, name: string)
            ---@param type string
            ---@param slot number
            ---@param name string
            function Entity:DTVar(type, slot, name) end

            ---@return number
            local function findIndex() end

            ---@param value any
            ---@return TypeGuard<string>
            function isstring(value) end

            ---@overload fun(type: string, name: string, extended?: table)
            ---@param type string
            ---@param slot number
            ---@param name string
            ---@param extended? table
            function Entity:NetworkVar(type, slot, name, extended)
                if isstring(slot) and (istable(name) or not name) then
                    extended = name
                    name = slot
                    slot = findIndex()
                elseif not slot and isstring(name) then
                    slot = findIndex()
                end

                self.DTVar(self, type, slot, name)
            end

            Entity:NetworkVar("Float", 1, "Age")
            Entity:NetworkVar("Float", "Age")
            "#
        ));
    }

    #[test]
    fn test_correlated_overload_params_forward_from_member_assigned_closure() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Entity
            local Entity = {}

            ---@overload fun(type: string, name: string)
            ---@overload fun(type: string, slot: nil, name: string)
            ---@param type string
            ---@param slot number
            ---@param name string
            function Entity:DTVar(type, slot, name) end

            ---@return number
            local function FindUnusedIndex() end

            ---@param value any
            ---@return TypeGuard<string>
            function isstring(value) end

            ---@param value any
            ---@return TypeGuard<table>
            function istable(value) end

            ---@overload fun(type: string, name: string, extended?: table)
            ---@param ent Entity
            ---@param typename string
            ---@param index number
            ---@param name string
            ---@param other_data? table
            Entity.NetworkVar = function(ent, typename, index, name, other_data)
                if isstring(index) and (istable(name) or not name) then
                    other_data = name
                    name = index
                    index = FindUnusedIndex()
                elseif not index and isstring(name) then
                    index = FindUnusedIndex()
                end

                ent.DTVar(ent, typename, index, name)
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
    fn test_numeric_alias_union_is_compatible_with_number_param() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@alias BUTTON_CODE number

            ---@param code number
            local function isDown(code) end

            ---@type BUTTON_CODE|number
            local key

            isDown(key)
            "#
        ));
    }

    #[test]
    fn test_pcall_variadic_generic_accepts_class_arg_from_unresolved_callable() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Entity

            ---@type table[]
            local indicators = {}

            ---@param vehicle Entity
            local function ResetHUDForVehicle(vehicle)
                for _, indicator in ipairs(indicators) do
                    if indicator.getValue then
                        pcall(indicator.getValue, vehicle)
                    end
                end
            end
        "#
        ));
    }

    #[test]
    fn test_required_table_field_assigned_from_reused_unresolved_local_is_not_nil() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Vector

            ---@class HullTrace
            ---@field start Vector
            ---@field endpos Vector

            ---@param trace HullTrace
            function TraceHull(trace) end

            local pos
            local traceData = {}

            local function Fire(params)
                pos = params.pos
                traceData.start = pos
                traceData.endpos = pos
                TraceHull(traceData)
            end
        "#
        ));
    }

    #[test]
    fn test_required_table_field_assigned_explicit_nil_still_mismatches() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Vector

            ---@class HullTrace
            ---@field start Vector
            ---@field endpos Vector

            ---@param trace HullTrace
            function TraceHull(trace) end

            local traceData = {}
            traceData.start = nil
            traceData.endpos = nil
            TraceHull(traceData)
        "#
        ));
    }

    #[test]
    fn test_required_table_field_assigned_known_wrong_type_still_mismatches() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Vector

            ---@class HullTrace
            ---@field start Vector
            ---@field endpos Vector

            ---@param trace HullTrace
            function TraceHull(trace) end

            local traceData = {}
            traceData.start = "wrong"
            traceData.endpos = "wrong"
            TraceHull(traceData)
        "#
        ));
    }

    #[test]
    fn test_inferred_dynamic_table_field_before_assignment_is_lenient_by_default() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r##"
            util = {}

            ---@return table?
            function util.JSONToTable(s) end

            ---@return table
            local function read_table()
                return util.JSONToTable("") or {}
            end

            ---@param value string
            ---@return string
            local function first_char(value)
                return value
            end

            ---@param value string
            ---@return string
            local function translate(value)
                return value
            end

            local data = read_table()

            if first_char(data.text) == "#" then
                data.text = translate(data.text)
            end
        "##,
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
        assert!(ws.check_code_for(
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
        assert!(ws.check_code_for(
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
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@type {[string]: number}
            local matchers = {}
            for _, matcher in pairs(matchers) do
                aaaa(matcher)
            end
        "#
        ));
        assert!(ws.check_code_for(
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

    #[test]
    fn test_vgui_instance_close_override_does_not_poison_sibling_dframe_close() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        emmyrc.gmod.dynamic_fields_global = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::ParamTypeMismatch);

        ws.def(
            r#"
            ---@class DFrame
            local DFrame = {}

            function DFrame:Close()
            end

            vgui = {}

            ---@generic T: DFrame
            ---@param className `T`
            ---@return T
            function vgui.Create(className)
            end

            function vgui.Register(name, tbl, base)
            end
            "#,
        );

        ws.def_file(
            "lua/vgui/editor.lua",
            r#"
            local PANEL = {}

            function PANEL:Init()
                self.Close = function(s)
                end
            end

            vgui.Register("EditorFrame", PANEL, "DFrame")
            "#,
        );

        let file_id = ws.def_file(
            "lua/vgui/browser.lua",
            r#"
            local PANEL = {}

            vgui.Register("BrowserFrame", PANEL, "DFrame")

            local frame = vgui.Create("BrowserFrame")
            frame:Close()
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let param_type_code = Some(NumberOrString::String(
            DiagnosticCode::ParamTypeMismatch.get_name().to_string(),
        ));
        assert!(
            diagnostics.iter().all(|diag| diag.code != param_type_code),
            "unexpected param-type-mismatch diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn test_array_slots_reassigned_from_struct_literals_use_reassigned_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Panel
            ---@field Remove fun(self: Panel)

            ---@class DPanel: Panel

            ---@class DLabel: Panel

            vgui = {}

            ---@generic T: Panel
            ---@param classname `T`
            ---@param parent Panel?
            ---@return T
            function vgui.Create(classname, parent)
            end
            "#,
        );

        ws.enable_check(DiagnosticCode::ParamTypeMismatch);
        let file_id = ws.def(
            r#"
                ---@type Panel
                local parent = {}
                parent.Tabs = {
                    { Title = "Tab 1", Tip = "Tip 1" },
                    { Title = "Tab 2", Tip = "Tip 2" },
                }

                for i = 1, #parent.Tabs do
                    parent.Tabs[i] = vgui.Create("DPanel", parent)
                end

                parent.Tabs[1].Label = vgui.Create("DLabel", parent.Tabs[1])
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let param_type_code = Some(NumberOrString::String(
            DiagnosticCode::ParamTypeMismatch.get_name().to_string(),
        ));
        assert!(
            diagnostics.iter().all(|diag| diag.code != param_type_code),
            "unexpected param-type-mismatch diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn test_array_slot_overwrite_after_reassignment_still_errors() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Panel

            ---@class DPanel: Panel

            ---@class DLabel: Panel

            vgui = {}

            ---@generic T: Panel
            ---@param classname `T`
            ---@param parent Panel?
            ---@return T
            function vgui.Create(classname, parent)
            end
            "#,
        );

        ws.enable_check(DiagnosticCode::ParamTypeMismatch);
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@type Panel
                local parent = {}
                parent.Tabs = {
                    { Title = "Tab 1", Tip = "Tip 1" },
                    { Title = "Tab 2", Tip = "Tip 2" },
                }

                for i = 1, #parent.Tabs do
                    parent.Tabs[i] = vgui.Create("DPanel", parent)
                end

                parent.Tabs[1] = "not a panel"
                parent.Tabs[1].Label = vgui.Create("DLabel", parent.Tabs[1])
            "#,
        ));
    }

    #[test]
    fn test_other_array_slot_assignment_does_not_hide_reassigned_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Panel

            ---@class DPanel: Panel

            ---@class DLabel: Panel

            vgui = {}

            ---@generic T: Panel
            ---@param classname `T`
            ---@param parent Panel?
            ---@return T
            function vgui.Create(classname, parent)
            end
            "#,
        );

        ws.enable_check(DiagnosticCode::ParamTypeMismatch);
        let file_id = ws.def(
            r#"
                ---@type Panel
                local parent = {}
                parent.Tabs = {
                    { Title = "Tab 1", Tip = "Tip 1" },
                    { Title = "Tab 2", Tip = "Tip 2" },
                }

                for i = 1, #parent.Tabs do
                    parent.Tabs[i] = vgui.Create("DPanel", parent)
                end

                parent.Tabs[2] = "not this slot"
                parent.Tabs[1].Label = vgui.Create("DLabel", parent.Tabs[1])
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let param_type_code = Some(NumberOrString::String(
            DiagnosticCode::ParamTypeMismatch.get_name().to_string(),
        ));
        assert!(
            diagnostics.iter().all(|diag| diag.code != param_type_code),
            "unexpected param-type-mismatch diagnostics: {diagnostics:?}"
        );
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
        // `cond and "valid"` has type `boolean | "valid"`. Both are tostring-coercible primitives,
        // so the param-type-mismatch diagnostic is suppressed for the `string` param.
        assert!(ws.check_code_for(
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
    fn test_vector_arithmetic_from_normalizing_helper_preserves_vector_result() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Vector
            ---@operator add(Vector): Vector
            ---@operator sub(Vector): Vector
            ---@operator mul(number|Vector): Vector
            ---@operator unm: Vector
            local Vector = {}

            ---@return number
            function Vector:Length() end

            ---@return Vector
            function Vector:Cross(other) end

            function Vector:Normalize() end

            ---@return Vector
            function Vector(x, y, z) end

            render = {}

            ---@param a Vector
            ---@param b Vector
            ---@param c Vector
            ---@param d Vector
            function render.DrawQuad(a, b, c, d) end

            ---@return string
            function type(value) end
            "#,
        );

        ws.enable_check(DiagnosticCode::ParamTypeMismatch);
        let file_id = ws.def(
            r#"
            local function toVector(v)
                if not v then return Vector(0, 0, 0) end
                if type(v) == "Vector" then return v end
                return Vector(0, 0, 0)
            end

            local function drawTraceBox(contactPos, contactNormal, fw, rt, radius)
                radius = radius or 8

                local halfWidth = radius * 1.1
                local halfLen = radius * 1.75

                local c1 = contactPos + -fw * (-halfLen) + rt * (-halfWidth)
                local c2 = contactPos + -fw * (-halfLen) + rt * (halfWidth)
                local c3 = contactPos + -fw * (halfLen) + rt * (halfWidth)
                local c4 = contactPos + -fw * (halfLen) + rt * (-halfWidth)

                local cn = toVector(contactNormal)
                if cn:Length() < 1e-4 then cn = Vector(0, 0, 1) end
                if cn.z < 0 then cn = -cn end
                cn:Normalize()

                local offset = cn * (radius * 0.2)

                local o1 = c1 + offset
                local o2 = c2 + offset
                local o3 = c3 + offset
                local o4 = c4 + offset
                render.DrawQuad(o1, o2, o3, o4)
            end
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
    fn test_unary_minus_definite_nil_still_mismatches_string_param() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param value table
            local function takes_table(value) end

            local value = nil
            takes_table(-value)
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

    // --- GLua primitive coercion suppression tests ---

    /// `string` params should accept integer/float literals (tostring() is implicit in GLua).
    #[test]
    fn test_string_param_accepts_integer_literal() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param label string
            local function show(label) end
            show(42)
        "#
        ));
    }

    #[test]
    fn test_string_param_accepts_float_literal() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param label string
            local function show(label) end
            show(3.14)
        "#
        ));
    }

    /// `string` params should accept a bare `number` typed variable.
    #[test]
    fn test_string_param_accepts_number_type() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param label string
            local function show(label) end
            ---@type number
            local n
            show(n)
        "#
        ));
    }

    /// `string` params should accept a `boolean`.
    #[test]
    fn test_string_param_accepts_boolean() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param label string
            local function show(label) end
            show(true)
            show(false)
        "#
        ));
    }

    /// `string` params should accept a union of all-primitive types.
    #[test]
    fn test_string_param_accepts_primitive_union() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param label string
            local function show(label) end
            ---@type string|number|boolean
            local v
            show(v)
        "#
        ));
    }

    /// Mirrors the real-world false-positive from cl_test10.lua: a union of string
    /// literals, integer literals, and a boolean literal passed to a `string` param.
    #[test]
    fn test_string_param_accepts_mixed_literal_union() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param label string
            local function show(label) end
            ---@type ""| "some_string"| 1| 45| 750| false
            local entry
            show(entry)
        "#
        ));
    }

    /// `string` params must still reject tables.
    #[test]
    fn test_string_param_rejects_table() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param label string
            local function show(label) end
            show({})
        "#
        ));
    }

    /// `string` params must still reject named class instances.
    #[test]
    fn test_string_param_rejects_class_instance() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class MyObj
            ---@field value number

            ---@param label string
            local function show(label) end

            ---@type MyObj
            local obj
            show(obj)
        "#
        ));
    }

    /// A union that contains a non-primitive (class) must still produce a diagnostic.
    #[test]
    fn test_string_param_rejects_union_with_class() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class DateData
            ---@param label string
            local function show(label) end
            ---@type DateData|string
            local v
            show(v)
        "#
        ));
    }

    /// `number` params must reject string literals (even numeric-looking ones) in strict mode.
    /// In lenient mode (default), numeric string literals are accepted.
    #[test]
    fn test_number_param_rejects_string_literal() {
        let mut ws = VirtualWorkspace::new();
        // Non-numeric strings are always rejected.
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param n number
            local function calc(n) end
            calc("abc")
        "#
        ));
        // Numeric string literals are rejected only in strict mode.
        let mut strict_emmyrc = Emmyrc::default();
        strict_emmyrc.strict.strict_type_coercion = true;
        ws.update_emmyrc(strict_emmyrc);
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param n number
            local function calc(n) end
            calc("2")
        "#
        ));
    }

    /// A bare `string` type must be rejected for `number` params.
    #[test]
    fn test_number_param_rejects_bare_string_type() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param n number
            local function calc(n) end
            ---@type string
            local s
            calc(s)
        "#
        ));
    }

    /// `integer` params must reject string literals in strict mode;
    /// in lenient mode (default), numeric string literals are accepted.
    #[test]
    fn test_integer_param_rejects_string_literal() {
        let mut ws = VirtualWorkspace::new();
        // Non-numeric strings are always rejected.
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param n integer
            local function calc(n) end
            calc("hello")
        "#
        ));
        // Numeric string literals are rejected only in strict mode.
        let mut strict_emmyrc = Emmyrc::default();
        strict_emmyrc.strict.strict_type_coercion = true;
        ws.update_emmyrc(strict_emmyrc);
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param n integer
            local function calc(n) end
            calc("2")
        "#
        ));
    }

    /// `string` params with optional (`?`) annotation should still accept primitives.
    #[test]
    fn test_optional_string_param_accepts_number() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param label string?
            local function show(label) end
            show(10)
        "#
        ));
    }

    // --- GMod NULL compatibility tests ---

    #[test]
    fn test_core_null_type_is_assignable_to_entity() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def(
            r#"
            ---@class Entity
            ---@class NULL : Entity
            "#,
        );

        let null_ty = ws.ty("NULL");
        let entity_ty = ws.ty("Entity");
        // In GMod, NULL is the "zero value" of Entity — an invalid/empty Entity.
        // NULL is assignable to Entity (and vice versa) because they are the same type family.
        assert!(ws.check_type(&null_ty, &entity_ty));
    }

    #[test]
    fn test_null_expr_type_is_assignable_to_entity() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def(
            r#"
            ---@class Entity
            ---@class NULL : Entity
            ---@type NULL
            NULL = nil
            "#,
        );

        let null_expr_ty = ws.expr_ty("NULL");
        let entity_ty = ws.ty("Entity");
        // In GMod, NULL is the "zero value" of Entity — assignable to Entity.
        assert!(ws.check_type(&null_expr_ty, &entity_ty));
    }

    #[test]
    fn test_null_param_accepts_entity_gmod() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def(
            r#"
            ---@class Entity
            ---@class NULL : Entity
            ---@type NULL
            NULL = nil
            "#,
        );

        let null_ty = ws.ty("NULL");
        let entity_ty = ws.ty("Entity");
        // In GMod, Entity is assignable to NULL (replacing a NULL placeholder with a real Entity).
        assert!(ws.check_type(&entity_ty, &null_ty));
    }

    /// NULL should be compatible with Entity subclasses like Player in GMod,
    /// since NULL is the "zero value" of the Entity type hierarchy.
    #[test]
    fn test_null_compatible_with_player() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def(
            r#"
            ---@class Entity
            ---@class Player : Entity
            ---@class NULL : Entity
            "#,
        );

        let null_ty = ws.ty("NULL");
        let player_ty = ws.ty("Player");
        // In GMod, NULL and Player are both in the Entity family, so they are compatible.
        assert!(ws.check_type(&null_ty, &player_ty));
    }

    // --- Union coercion suppression tests ---

    /// `(any|string)` passed to `string` param should not produce a mismatch.
    /// Both union arms are accepted by GLua string coercion checking.
    #[test]
    fn test_any_string_union_to_string_param() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param s string
            local function f(s) end

            ---@type any|string
            local val
            f(val)
        "#
        ));
    }

    /// `(0|any|number)` passed to `number` param should not produce a mismatch.
    /// Each union arm is accepted by number checking.
    #[test]
    fn test_any_number_union_to_number_param() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param n number
            local function f(n) end

            ---@type 0|any|number
            local val
            f(val)
        "#
        ));
    }

    /// `(any|string)` passed to `number` param SHOULD still mismatch.
    /// The `string` arm remains incompatible with `number`.
    #[test]
    fn test_any_string_union_to_number_param_still_mismatches() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param n number
            local function f(n) end

            ---@type any|string
            local val
            f(val)
        "#
        ));
    }

    /// `any|string|nil` should satisfy `string?` through nullable string coercion.
    #[test]
    fn test_any_string_nil_to_string_nullable_passes() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param s string?
            local function f(s) end

            ---@type any|string|nil
            local val
            f(val)
        "#
        ));
    }

    /// `any|string|nil` should still mismatch `number?` because `string` is incompatible.
    #[test]
    fn test_any_string_nil_to_number_nullable_mismatches() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param n number?
            local function f(n) end

            ---@type any|string|nil
            local val
            f(val)
        "#
        ));
    }

    #[test]
    fn test_any_unknown_union_to_table_param_does_not_mismatch() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param t table
            local function takes_table(t) end

            ---@type any|unknown
            local value
            takes_table(value)
        "#,
        ));
    }

    #[test]
    fn test_nullable_any_unknown_union_to_table_param_does_not_mismatch() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param t table
            local function takes_table(t) end

            ---@type any|unknown|nil
            local value
            takes_table(value)
        "#,
        ));
    }

    #[test]
    fn test_inferred_any_unknown_union_to_table_param_does_not_mismatch() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param t table
            local function takes_table(t) end

            local stored = {}

            local function get_value(key, default)
                local config = stored[key]

                if config then
                    if config.value ~= nil then
                        return config.value
                    elseif config.default ~= nil then
                        return config.default
                    end
                end

                return default
            end

            takes_table(get_value("color"))
        "#,
        ));
    }

    #[test]
    fn test_method_table_param_accepts_inferred_any_unknown_union() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Panel
            ---@field DrawTextEntryText fun(self: Panel, text_color: table, highlight_color: table, cursor_color: table)

            ---@type Panel
            local panel

            local color_white = {}
            local stored = {}

            local ix = { config = {} }
            function ix.config.Get(key, default)
                local config = stored[key]

                if config and config.type then
                    if config.value ~= nil then
                        return config.value
                    elseif config.default ~= nil then
                        return config.default
                    end
                end

                return default
            end

            panel:DrawTextEntryText(color_white, ix.config.Get("color"), color_white)
        "#,
        ));
    }

    #[test]
    fn test_nullable_any_to_table_param_does_not_report_type_mismatch_in_strict_nil_mode() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.strict.allow_nullable_as_non_nullable = false;
        ws.update_emmyrc(emmyrc);

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param t table
            local function takes_table(t) end

            ---@type any|nil
            local value
            takes_table(value)
        "#,
        ));
    }

    /// In GMod, Entity is used as a generic stand-in for many things (network vars,
    /// etc.), so passing Entity where a more specific subtype is expected should not
    /// produce false-positive param-type-mismatch diagnostics.
    #[test]
    fn test_entity_accepted_for_child_type_param() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Entity
            ---@class base_glide : Entity

            ---@param e base_glide?
            local function takes_base_glide(e)
            end

            ---@type Entity
            local ent = nil

            takes_base_glide(ent)
        "#
        ));
    }

    /// Entity should also be accepted for non-nullable child type params in GMod.
    #[test]
    fn test_entity_accepted_for_nonnullable_child_type_param() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Entity
            ---@class base_glide : Entity

            ---@param e base_glide
            local function takes_base_glide(e)
            end

            ---@type Entity
            local ent = nil

            takes_base_glide(ent)
        "#
        ));
    }

    #[test]
    fn test_inferred_dynamic_string_key_field_does_not_report_param_mismatch() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::ParamTypeMismatch);

        let file_id = ws.def(
            r#"
                local function readValue()
                    if flag then
                        return Vector(1, 2, 3)
                    end

                    return "not a number"
                end

                local rec = { data = {} }
                local data = rec.data
                local key = net.ReadString()
                data[key] = readValue()

                local d = rec.data
                math.abs(d.forwardSlip or 0)
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let param_type_code = Some(NumberOrString::String(
            DiagnosticCode::ParamTypeMismatch.get_name().to_string(),
        ));
        assert!(
            diagnostics.iter().all(|diag| diag.code != param_type_code),
            "inferred dynamic key field values should respect inferred mismatch diagnostics policy: {diagnostics:?}"
        );
    }
}
