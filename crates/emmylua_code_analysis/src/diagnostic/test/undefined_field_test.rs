#[cfg(test)]
mod test {
    use std::{ops::Deref, sync::Arc};

    use crate::{DiagnosticCode, LuaType, VirtualWorkspace};

    #[test]
    fn test_1() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@alias std.NotNull<T> T - ?

                ---@generic V
                ---@param t {[any]: V}
                ---@return fun(tbl: any):int, std.NotNull<V>
                function ipairs(t) end

                ---@type {[integer]: string|table}
                local a = {}

                for i, extendsName in ipairs(a) do
                    print(extendsName.a)
                end
            "#
        ));
    }

    #[test]
    fn test() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class diagnostic.test3
                ---@field private a number

                ---@type diagnostic.test3
                local test = {}

                local b = test.b
            "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class diagnostic.test3
                ---@field private a number
                local Test3 = {}

                local b = Test3.b
            "#
        ));
    }

    #[test]
    fn test_enum() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@enum diagnostic.enum
                local Enum = {
                    A = 1,
                }

                local enum_b = Enum["B"]
            "#
        ));
    }
    #[test]
    fn test_issue_194() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            local a ---@type 'A'
            local _ = a:lower()
            "#
        ));
    }

    #[test]
    fn test_issue_917() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@alias Required917<T> { [K in keyof T]: T[K]; }

                ---@alias SomeMap917 { some_int?: integer, some_str?: string }

                ---@type Required917<SomeMap917>
                local a

                local _ = a.some_int
            "#
        ));
    }

    #[test]
    fn test_any_key() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class LogicalOperators
                local logicalOperators <const> = {}

                ---@param key any
                local function test(key)
                    print(logicalOperators[key])
                end
            "#
        ));
    }

    #[test]
    fn test_class_key_to_class_key() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                --- @type table<string, integer>
                local FUNS = {}

                ---@class D10.AAA

                ---@type D10.AAA
                local Test1

                local a = FUNS[Test1]
            "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@generic K, V
                ---@param t table<K, V> | V[] | {[K]: V}
                ---@return fun(tbl: any):K, std.NotNull<V>
                local function pairs(t) end

                ---@class D11.AAA
                ---@field name string
                ---@field key string
                local AAA = {}

                ---@type D11.AAA
                local a

                for k, v in pairs(AAA) do
                    if not a[k] then
                        -- a[k] = v
                    end
                end
            "#
        ));
    }

    #[test]
    fn test_2() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local function sortCallbackOfIndex()
                    ---@type table<string, integer>
                    local indexMap = {}
                    return function(v)
                        return -indexMap[v]
                    end
                end
            "#
        ));
    }

    #[test]
    fn test_index_key_define() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local Flags = {
                    A = {},
                }

                ---@class (constructor) RefImpl
                local a = {
                    [Flags.A] = true,
                }

                print(a[Flags.A])
            "#
        ));
    }

    #[test]
    fn test_issue_292() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            --- @type {head:string}[]?
            local b
            ---@diagnostic disable-next-line: need-check-nil
            _ = b[1].head == 'b'
            "#
        ));
    }

    #[test]
    fn test_issue_317() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                --- @class A
                --- @field [string] string
                --- @field [integer] integer
                local foo = {}

                local bar = foo[1]
            "#
        ));
    }

    #[test]
    fn test_issue_345() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                --- @class C
                --- @field a string
                --- @field b string

                local scope --- @type 'a'|'b'

                local m --- @type C

                a = m[scope]
        "#
        ));
        let ty = ws.expr_ty("a");
        let expected = ws.ty("string");
        assert_eq!(ws.humanize_type(ty), ws.humanize_type(expected));
    }

    #[test]
    fn test_index_key_by_string() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@enum (key) K1
            local apiAlias = {
                Unit         = 'unit_entity',
            }

            ---@type string?
            local cls
            local a = apiAlias[cls]
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@enum (key) K2
            local apiAlias = {
                Unit         = 'unit_entity',
            }

            ---@type string?
            local cls
            local a = apiAlias["1" .. cls]
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@enum K3
            local apiAlias = {
                Unit         = 'unit_entity',
            }

            ---@type string?
            local cls
            local a = apiAlias["Unit1"]
        "#
        ));
    }

    #[test]
    fn test_unknown_type() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local function test(...)
                    local args = { ... }
                    local a = args[1]
                end
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::InjectField,
            r#"
                local function test(...)
                    local args = { ... }
                    args[1] = 1
                end
        "#
        ));
    }

    #[test]
    fn test_g() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                print(_G['game_lua_files'])
        "#
        ));
    }

    #[test]
    fn test_def() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::InjectField,
            r#"
                ---@class ECABind
                Bind = {}

                ---@class ECAFunction
                ---@field call_name string
                local M = {}

                ---@param func function
                function M:call(func)
                    Bind[self.call_name] = function(...)
                        return
                    end
                end
        "#
        ));
    }

    #[test]
    fn test_enum_1() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@enum (key) UnitAttr
                local UnitAttr = {
                    ['hp_cur'] = 'hp_cur',
                    ['mp_cur'] = 1,
                }

                ---@param name UnitAttr
                local function get(name)
                    local a = UnitAttr[name]
                end
        "#
        ));
    }

    #[test]
    fn test_enum_2() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@enum AbilityType
            local AbilityType = {
                HIDE    = 0,
                NORMAL  = 1,
                ['隐藏'] = 0,
                ['普通'] = 1,
            }

            ---@alias AbilityTypeAlias
            ---| '隐藏'
            ---| '普通'


            ---@param name AbilityType | AbilityTypeAlias
            local function get(name)
                local a = AbilityType[name]
            end
        "#
        ));
    }

    #[test]
    fn test_enum_3() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@enum (key) PlayerAttr
            local PlayerAttr = {}

            ---@param key PlayerAttr
            local function add(key)
                local a = PlayerAttr[key]
            end
        "#
        ));
    }

    #[test]
    fn test_enum_alias() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@enum EA
                A = {
                    ['GAME_INIT'] = "ET_GAME_INIT",
                }

                ---@enum EB
                B = {
                    ['GAME_PAUSE'] = "ET_GAME_PAUSE",
                }

                ---@alias EventName EA | EB

                ---@class Event
                local event = {}
                event.ET_GAME_INIT = {}
                event.ET_GAME_PAUSE = {}


                ---@param name EventName
                local function test(name)
                    local a = event[name]
                end
        "#
        ));
    }

    #[test]
    fn test_userdata() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@type any
            local value
            local tp = type(value)

            if tp == 'userdata' then
                ---@cast value userdata
                if value['type'] then
                end
            end
        "#
        ));
    }

    #[test]
    fn test_has_nil() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"

                ---@type table<string, boolean>
                local includedNameMap = {}

                ---@param name? string
                local function a(name)
                    if not includedNameMap[name] then
                    end
                end
        "#
        ));
    }

    #[test]
    fn test_super_integer() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@type table<integer, string>
            local t = {}

            ---@class NewKey: integer

            ---@type NewKey
            local key = 1

            local a = t[key]

        "#
        ));
    }

    #[test]
    fn test_generic_super() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@generic Super: string
            ---@param super? `Super`
            local function declare(super)
                ---@type table<string, string>
                local config

                local superClass = config[super]
            end
        "#
        ));
    }

    #[test]
    fn test_ref_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@enum ReactiveFlags
                local ReactiveFlags = {
                    IS_REF = { '<IS_REF>' },
                }
                local IS_REF = ReactiveFlags.IS_REF

                ---@class ObjectRefImpl
                local ObjectRefImpl = {}

                function ObjectRefImpl.new()
                    ---@class (constructor) ObjectRefImpl
                    local self = {
                        [IS_REF] = true, -- 标记为ref
                    }
                end

                ---@param a ObjectRefImpl
                local function name(a)
                    local c = a[IS_REF]
                end
        "#
        ));
    }

    #[test]
    fn test_string_add_enum_key() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class py.GameAPI
                GameAPI = {}

                function GameAPI.get_kv_pair_value_unit_entity(handle, key) end

                function GameAPI.get_kv_pair_value_unit_name() end

                ---@enum(key) KV.SupportTypeEnum
                local apiAlias = {
                    Unit         = 'unit_entity',
                    UnitKey      = 'unit_name',
                }

                ---@param lua_type 'boolean' | 'number' | 'integer' | 'string' | 'table' | KV.SupportTypeEnum
                ---@return any
                local function kv_load_from_handle(lua_type)
                    local alias = apiAlias[lua_type]
                    local api = GameAPI['get_kv_pair_value_' .. alias]
                end
        "#
        ));
    }

    #[test]
    fn test_global_arg_override() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = ws.analysis.emmyrc.deref().clone();
        emmyrc.strict.meta_override_file_define = false;
        ws.analysis.update_config(Arc::new(emmyrc));

        ws.def(
            r#"
        ---@class py.Dict

        ---@return py.Dict
        local function lua_get_start_args() end

        ---@type table<string, string>
        arg = lua_get_start_args()
        "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            local function isDebuggerValid()
                if arg['lua_multi_mode'] == 'true' then
                end
            end
        "#
        ));
    }

    #[test]
    fn test_if_1() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@type table<int, string>
            local arg = {}
            if arg['test'] == 'true' then
            end
        "#
        ));
    }

    #[test]
    fn test_enum_field_1() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@enum Enum
                local Enum = {
                    a = 1,
                }
        "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@param p Enum
                function func(p)
                    local x1 = p.a
                end
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@param p Enum
                function func(p)
                    local x1 = p
                    local x2 = x1.a
                end
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@param p Enum
                function func(p)
                    local x1 = p
                    local x2 = x1
                    local x3 = x2.a
                end
        "#
        ));
    }

    #[test]
    fn test_if_custom_type_1() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@enum Flags
                Flags = {
                    b = 1
                }
            "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"

                if Flags.a then
                end
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"

                if Flags['a'] then
                end
        "#
        ));
    }

    #[test]
    fn test_if_custom_type_2() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class Flags
                ---@field a number
                Flags = {}
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                if Flags.b then
                end
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                if Flags["b"] then
                end
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type string
                local a
                if Flags[a] then
                end
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type string
                local c
                if Flags[c] then
                end
        "#
        ));
    }

    #[test]
    fn test_nil_safe_logical_contexts_for_custom_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class VehicleLike
                VehicleLike = {}
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type VehicleLike
                local ent
                local ok = ent.isGlideVehicle or false
            "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type VehicleLike
                local ent
                local ok = ent.isGlideVehicle and true
            "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type VehicleLike
                local ent
                local ok = not ent.isGlideVehicle
            "#
        ));
    }

    #[test]
    fn test_nil_safe_equality_contexts_for_custom_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class VehicleEqLike
                VehicleEqLike = {}
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type VehicleEqLike
                local ent
                local ok = ent.isGlideVehicle == nil
            "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type VehicleEqLike
                local ent
                local ok = ent.isGlideVehicle ~= nil
            "#
        ));
    }

    #[test]
    fn test_nil_safe_equality_does_not_suppress_member_calls() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class VehicleCallLike
                VehicleCallLike = {}
            "#,
        );

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type VehicleCallLike
                local ent
                local ok = ent.isGlideVehicle() ~= nil
            "#
        ));
    }

    #[test]
    fn test_isfunction_member_guard_suppresses_undefined_field() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
                ---@class VehicleGuardLike
                VehicleGuardLike = {}
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type VehicleGuardLike
                local vehicle
                if isfunction(vehicle.GetFreeSeat) then
                end
            "#
        ));
    }

    #[test]
    fn test_nil_safe_or_regression_return_expression() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class EntityMeta
                EntityMeta = {}
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type EntityMeta
                local self
                local function IsVehicle(v) end

                local function is_vehicle()
                    return self.IsGlideVehicle or IsVehicle(self)
                end
            "#
        ));
    }

    #[test]
    fn test_nil_safe_logical_contexts_for_nullable_custom_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class VehicleNullable
                VehicleNullable = {}
            "#,
        );

        // nullable type (Vehicle | nil) in or-context: field access should be suppressed
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type VehicleNullable?
                local ent
                local ok = ent.isGlideVehicle or false
            "#
        ));

        // nullable type in and-context (IsValid-style guard pattern)
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type VehicleNullable?
                local ent
                local ok = ent and ent.isGlideVehicle
            "#
        ));
    }

    #[test]
    fn test_nil_safe_logical_context_keeps_enum_warning() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@enum FlagsEnum
                FlagsEnum = {
                    a = 1,
                }
            "#,
        );

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local ok = FlagsEnum.b or false
            "#
        ));
    }

    #[test]
    fn test_array_computed_number_index() {
        let mut ws = VirtualWorkspace::new();
        // Array indexed with an expression whose return type is `number`
        // (e.g. a GLua-style RandomInt or math.random) must not trigger
        // undefined-field.
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local SMOKE_SPRITES = {
                    "particle/smokesprites_0001",
                    "particle/smokesprites_0002",
                    "particle/smokesprites_0003",
                }

                ---@return number
                local function RandomInt(m, n) end

                local sprite = SMOKE_SPRITES[RandomInt(1, #SMOKE_SPRITES)]
            "#
        ));

        // table[variable] where the variable is typed `number` must not trigger
        // undefined-field either.
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local SPRITES = {
                    "a",
                    "b",
                }

                ---@type number
                local idx

                local sprite = SPRITES[idx]
            "#
        ));
    }

    #[test]
    fn test_export() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "a.lua",
            r#"
            ---@export
            local export = {}

            return export
            "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            local a = require("a")
            a.func()
            "#,
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            local a = require("a").ABC
            "#,
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"

            ---@export
            local export = {}

            export.aaa()

            return export

            "#,
        ));
    }

    #[test]
    fn test_keyof_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
        ---@class SuiteHooks
        ---@field beforeAll string

        ---@type SuiteHooks
        hooks = {}

        ---@type keyof SuiteHooks
        name = "beforeAll"
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
        local a = hooks[name]
        "#
        ));
    }

    #[test]
    fn test_never_prefix_no_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        // Accessing a field on a `never` typed value should not produce undefined-field.
        // `never` arises from type inference limitations, not real code errors.
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type never
                local x
                local _ = x.someField
            "#
        ));
    }

    #[test]
    fn test_nil_guarded_field_in_if_body() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class TestConfig
                local Config = {}

                ---@type TestConfig
                local cfg = {}

                if cfg.dynamicField ~= nil then
                    local x = cfg.dynamicField
                end
            "#,
        ));
    }

    #[test]
    fn test_nil_guarded_field_truthy_check() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class TestConfig2
                local Config = {}

                ---@type TestConfig2
                local cfg = {}

                if cfg.dynamicField then
                    local x = cfg.dynamicField
                end
            "#,
        ));
    }

    #[test]
    fn test_nil_guarded_field_compound_and() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class TestConfig3
                local Config = {}

                ---@type TestConfig3
                local cfg = {}

                if cfg.dynamicField ~= nil and cfg.dynamicField > 0 then
                    local x = cfg.dynamicField
                end
            "#,
        ));
    }

    #[test]
    fn test_field_on_subclass_suppressed() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class SubclassTest.BaseEntity
                local BaseEntity = {}

                ---@class SubclassTest.Vehicle : SubclassTest.BaseEntity
                local Vehicle = {}
                function Vehicle:GetDriver() end

                ---@type SubclassTest.BaseEntity
                local ent = nil
                ent:GetDriver()
            "#,
        ));
    }

    #[test]
    fn test_field_on_deep_subclass_suppressed() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class DeepSubTest.Entity
                local Entity = {}

                ---@class DeepSubTest.Vehicle : DeepSubTest.Entity
                local Vehicle = {}

                ---@class DeepSubTest.Airboat : DeepSubTest.Vehicle
                local Airboat = {}
                function Airboat:GetSpecialField() end

                ---@type DeepSubTest.Entity
                local ent = nil
                ent:GetSpecialField()
            "#,
        ));
    }

    #[test]
    fn test_field_not_on_any_subclass_still_reported() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class NoSubField.BaseEntity
                local BaseEntity = {}

                ---@class NoSubField.Vehicle : NoSubField.BaseEntity
                local Vehicle = {}
                function Vehicle:GetDriver() end

                ---@type NoSubField.BaseEntity
                local ent = nil
                ent:CompletelyMadeUpMethod()
            "#,
        ));
    }

    #[test]
    fn test_tool_getowner_concommand() {
        // Tool:GetOwner() returns Player, Player:ConCommand should resolve
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Player
            function Player:ConCommand(cmd) end

            ---@class Tool
            ---@return Player
            function Tool:GetOwner() end

            ---@class TOOL : Tool
            TOOL = {}
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            function TOOL:RightClick(trace)
                local ply = self:GetOwner()
                ply:ConCommand("test")
            end
            "#,
        ));
    }

    #[test]
    fn test_buildcpanel_param_from_field_annotation() {
        // BuildCPanel field annotation fun(panel: ControlPanel) should propagate
        // the ControlPanel type to the panel parameter
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class DForm
            function DForm:Help(text) end
            function DForm:NumSlider(label, convar, min, max) end

            ---@class ControlPanel : DForm
            function ControlPanel:AddControl(type, controlinfo) end

            ---@class Tool
            ---@field BuildCPanel fun(panel: ControlPanel)

            ---@class TOOL : Tool
            TOOL = {}
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            function TOOL.BuildCPanel(panel)
                panel:Help("test help")
                panel:AddControl("slider", {})
            end
            "#,
        ));
    }

    #[test]
    fn test_unary_minus_preserves_type_methods() {
        let mut ws = VirtualWorkspace::new();

        // Unary minus on a type with @operator unm should preserve that type
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class TestVecUNM
                ---@operator unm: TestVecUNM
                local TestVecUNM = {}
                function TestVecUNM:Dot(v) return 0 end
                function TestVecUNM:Forward() return TestVecUNM end

                ---@type TestVecUNM
                local ang

                local dir = -ang:Forward()
                local result = dir:Dot(ang)
            "#,
        ));
    }

    #[test]
    fn test_global_table_cross_file_member_resolution() {
        let mut ws = VirtualWorkspace::new();

        ws.def_file(
            "defs.lua",
            r#"
                ---@class VecCross
                ---@operator unm: VecCross
                local VecCross = {}
                function VecCross:Dot(v) return 0 end
                function VecCross:Forward() return VecCross end

                ---@return VecCross
                function _G.MakeVecCross() end
            "#,
        );

        // File A defines a global table and functions (NO return annotations - inferred)
        ws.def_file(
            "file_a.lua",
            r#"
                MyGlobal = MyGlobal or {}

                local cachedPos = MakeVecCross()
                local cachedAng = MakeVecCross()

                function MyGlobal.GetViewPos()
                    return cachedPos, cachedAng
                end
            "#,
        );

        // File B uses a localized reference (like the real addon)
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local GetViewPos = MyGlobal.GetViewPos
                local pos, ang = GetViewPos()
                local dir = -ang:Forward()
                local result = dir:Dot(pos)
            "#,
        ));
    }

    #[test]
    fn test_tableof_field_access_works() {
        let mut ws = VirtualWorkspace::new();
        // Simpler test: use explicit type instead of self
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class MyEntity
                ---@field health number
                ---@field name string
                local MyEntity = {}

                ---@type tableof<MyEntity>
                local tbl
                local h = tbl.health
                local n = tbl.name
            "#,
        ));
    }

    #[test]
    fn test_tableof_self_field_access_works() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class MyEntity
                ---@field health number
                ---@field name string
                local MyEntity = {}

                ---@return tableof<self>
                function MyEntity:GetTable() end

                function MyEntity:Test()
                    local tbl = self:GetTable()
                    local h = tbl.health
                    local n = tbl.name
                end
            "#,
        ));
    }

    #[test]
    fn test_tableof_type_inference() {
        let mut ws = VirtualWorkspace::new();
        ws.def(r#"
            ---@class MyEntity2
            local MyEntity2 = {}
        "#);

        let tableof_ty = ws.ty("tableof<MyEntity2>");
        assert!(matches!(tableof_ty, LuaType::TableOf(_)));
    }

    #[test]
    fn test_tableof_colon_call_flags_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        // Colon calls on tableof should trigger undefined-field diagnostic
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class MyEntity
                local MyEntity = {}

                function MyEntity:DoSomething() end

                ---@return tableof<self>
                function MyEntity:GetTable() end

                function MyEntity:Test()
                    local tbl = self:GetTable()
                    tbl:DoSomething()
                end
            "#,
        ));
    }

    #[test]
    fn test_tableof_local_function_call() {
        // Test: local getTable = Entity.GetTable; getTable(self)
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class MyEntity
                ---@field health number
                local MyEntity = {}

                ---@return tableof<self>
                function MyEntity:GetTable() end

                local getTable = MyEntity.GetTable

                function MyEntity:Test()
                    local tbl = getTable(self)
                    local h = tbl.health
                end
            "#,
        ));
    }

    #[test]
    fn test_tableof_dynamic_field_assignment() {
        // Test: dynamically-assigned fields through tableof should be recognized
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        ws.def(
            r#"
                ---@class MyVehicle
                local MyVehicle = {}

                ---@return tableof<self>
                function MyVehicle:GetTable() end

                local getTable = MyVehicle.GetTable

                function MyVehicle:Initialize()
                    local selfTbl = getTable(self)
                    selfTbl.wheels = {}
                    selfTbl.wheelCount = 4
                end

                function MyVehicle:Update()
                    local selfTbl = getTable(self)
                    local w = selfTbl.wheels
                    local c = selfTbl.wheelCount
                end
            "#,
        );

        // We need to check the second method's file for diagnostics
        // Since both are in same def block, check the whole file
        let file_id = ws.def(
            r#"
                ---@class MyVehicle2
                local MyVehicle2 = {}

                ---@return tableof<self>
                function MyVehicle2:GetTable() end

                local getTable2 = MyVehicle2.GetTable

                function MyVehicle2:Initialize()
                    local selfTbl = getTable2(self)
                    selfTbl.wheels = {}
                    selfTbl.wheelCount = 4
                end

                function MyVehicle2:Update()
                    local selfTbl = getTable2(self)
                    local w = selfTbl.wheels
                    local c = selfTbl.wheelCount
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, tokio_util::sync::CancellationToken::new());
        if let Some(diagnostics) = diagnostics {
            let undef_fields: Vec<_> = diagnostics
                .iter()
                .filter(|d| {
                    d.code
                        == Some(lsp_types::NumberOrString::String(
                            "undefined-field".to_string(),
                        ))
                })
                .collect();
            assert!(
                undef_fields.is_empty(),
                "Expected no undefined-field diagnostics but got: {:?}",
                undef_fields
                    .iter()
                    .map(|d| &d.message)
                    .collect::<Vec<_>>()
            );
        }
    }
}
