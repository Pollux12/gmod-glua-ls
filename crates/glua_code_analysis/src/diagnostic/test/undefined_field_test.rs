#[cfg(test)]
mod test {
    use std::{ops::Deref, sync::Arc};

    use crate::{DiagnosticCode, Emmyrc, LuaType, VirtualWorkspace};
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

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
    fn test_numeric_for_index_expr_on_inferred_setmetatable_table() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local registry = {
                    base = {
                        Initialize = function(self) end,
                        OnRemove = function(self) end,
                    }
                }

                local function CreateVehicleWeapon(className, data)
                    local class = registry[className]
                    return setmetatable(data or {}, { __index = class })
                end

                local weapons = {}
                local weaponCount = 0

                local function CreateWeapon(className, data)
                    local weapon = CreateVehicleWeapon(className, data)
                    local index = weaponCount + 1

                    weaponCount = index
                    weapons[index] = weapon
                    weapon:Initialize()
                end

                CreateWeapon("base", {})

                local myWeapons = weapons

                for i = #myWeapons, 1, -1 do
                    myWeapons[i]:OnRemove()
                    myWeapons[i] = nil
                end
            "#
        ));
    }

    #[test]
    fn test_setmetatable_named_metatable_does_not_report_undefined_field_for_methods() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                Glide = {}

                local RangedFeature = {}
                RangedFeature.__index = RangedFeature

                function RangedFeature:Update() end
                function RangedFeature:Think() end
                function RangedFeature:Draw() end

                function Glide.CreateRangedFeature(vehicle, maxDistance)
                    return setmetatable({}, RangedFeature)
                end

                local ENT = {}

                function ENT:Initialize()
                    self.rfMisc = Glide.CreateRangedFeature(self, 1000)
                    self.rfMisc:Update()
                    self.rfMisc:Think()
                    self.rfMisc:Draw()
                end
            "#
        ));
    }

    #[test]
    fn test_included_server_scripted_class_reverse_numeric_for_does_not_report_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        ws.def_file(
            "lua/entities/base_glide/shared.lua",
            r#"
                ENT.Type = "anim"
                ENT.Base = "base_anim"
            "#,
        );
        ws.def_file(
            "lua/entities/base_glide/init.lua",
            r#"
                AddCSLuaFile("shared.lua")
                AddCSLuaFile("cl_init.lua")
                include("shared.lua")
                include("sv_weapons.lua")
            "#,
        );
        ws.def_file(
            "lua/entities/base_glide/cl_init.lua",
            r#"
                include("shared.lua")
                include("cl_hud.lua")

                function ENT:Initialize()
                    self.weapons = {}
                    self.weaponSlotIndex = 0
                end
            "#,
        );
        ws.def_file(
            "lua/entities/base_glide/cl_hud.lua",
            r#"
                function ENT:OnDriverChange(_, _, _)
                    self.weapons = {}
                    self.weaponSlotIndex = 0
                end

                function ENT:OnSyncWeaponData()
                    local slotIndex = net.ReadUInt(5)
                    local className = net.ReadString()
                    local weapon = self.weapons[slotIndex]

                    if not weapon then
                        weapon = Glide.CreateVehicleWeapon(className)
                        weapon.Vehicle = self
                        weapon:Initialize()

                        self.weapons[slotIndex] = weapon
                        self:OnActivateWeapon(weapon, slotIndex)
                    end
                end
            "#,
        );
        let file_id = ws.def_file(
            "lua/entities/base_glide/sv_weapons.lua",
            r#"
                function ENT:WeaponInit()
                    self.weapons = {}
                    self.weaponCount = 0
                end

                function ENT:ClearWeapons()
                    local myWeapons = self.weapons
                    if not myWeapons then return end

                    for i = #myWeapons, 1, -1 do
                        myWeapons[i]:OnRemove()
                        myWeapons[i] = nil
                    end

                    self.weapons = {}
                    self.weaponCount = 0
                end

                function ENT:CreateWeapon(class, data)
                    local weapon = Glide.CreateVehicleWeapon(class, data)
                    local index = self.weaponCount + 1

                    self.weaponCount = index
                    self.weapons[index] = weapon
                    weapon:Initialize()
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let undefined_field = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));

        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.code != undefined_field),
            "unexpected UndefinedField diagnostics: {diagnostics:#?}"
        );
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
    fn test_gmod_string_numeric_indexing_no_undefined_field() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        // Both literal numeric index and integer-typed variable index should be accepted
        // for string types when GMod mode is enabled.
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            local str = "hello"
            local a = str[2]
            ---@type integer
            local i
            local b = str[i]
            ---@type number
            local n
            local c = str[n]
            "#
        ));
    }

    #[test]
    fn test_lua_string_indexing_still_reports() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            local str = "hello"
            local a = str[1]
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
    fn test_plain_table_missing_field_reports_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local test = {}
                print(test.meow)
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
    fn test_boolean_equality_context_for_custom_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class Params
                Params = {}
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type Params
                local params
                local is_front = params.isFrontWheel == true
            "#
        ));
    }

    #[test]
    fn test_boolean_equality_context_for_inferred_table_const() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local params = {}
                local is_front = params.isFrontWheel == true
            "#
        ));
    }

    #[test]
    fn test_boolean_inequality_context_for_inferred_table_const() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local params = {}
                local is_not_front = params.isFrontWheel ~= true
            "#
        ));
    }

    #[test]
    fn test_boolean_and_context_for_inferred_table_const() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local params = {}
                local is_front = params.isFrontWheel and true
            "#
        ));
    }

    #[test]
    fn test_boolean_or_context_for_inferred_table_const() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local params = {}
                local is_front = params.isFrontWheel or false
            "#
        ));
    }

    #[test]
    fn test_boolean_not_context_for_inferred_table_const() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local params = {}
                local is_not_front = not params.isFrontWheel
            "#
        ));
    }

    #[test]
    fn test_boolean_equality_context_for_string_keyed_generic_table() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type table<string, boolean>
                local params = {}
                local is_front = params.isFrontWheel == true
            "#
        ));
    }

    #[test]
    fn test_direct_dot_access_for_string_keyed_generic_table() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type table<string, boolean>
                local params = {}
                local is_front = params.isFrontWheel
            "#
        ));
    }

    #[test]
    fn test_direct_dot_access_for_integer_keyed_generic_table_still_reports() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type table<integer, boolean>
                local params = {}
                local is_front = params.isFrontWheel
            "#
        ));
    }

    #[test]
    fn test_boolean_equality_context_for_integer_keyed_generic_table_still_reports() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type table<integer, boolean>
                local params = {}
                local is_front = params.isFrontWheel == true
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
    fn test_find_meta_table_definition_receiver_method_is_resolvable() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Player
            local Player = {}

            player = player or {}

            ---@return Player[]
            function player.GetAll() end

            ---@generic T : table
            ---@param metaName `T`
            ---@return (definition) T|nil
            function _G.FindMetaTable(metaName) end
            "#,
        );

        ws.enable_check(DiagnosticCode::UndefinedField);
        let file_id = ws.def(
            r#"
            local PLAYER = FindMetaTable("Player")
            if PLAYER == nil then return end

            function PLAYER:GetTime()
                return 0
            end

            local pl = player.GetAll()[1]
            print(pl:GetTime())

            A = PLAYER.GetTime
            B = pl.GetTime
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let undefined_field = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.code != undefined_field),
            "unexpected UndefinedField diagnostics: {diagnostics:#?}"
        );

        let player_meta_method = ws.expr_ty("A");
        assert!(
            !player_meta_method.is_unknown(),
            "expected PLAYER.GetTime to be resolvable"
        );

        let player_instance_method = ws.expr_ty("B");
        assert!(
            !player_instance_method.is_unknown(),
            "expected pl.GetTime to be resolvable"
        );
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
        ws.def(
            r#"
            ---@class MyEntity2
            local MyEntity2 = {}
        "#,
        );

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
                undef_fields.iter().map(|d| &d.message).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn test_nil_guard_in_condition_truthy_check() {
        let mut ws = VirtualWorkspace::new();
        // Field used as truthy check in if condition should be suppressed
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class NilGuardTest
                local obj = {}
                if obj.unknownField then
                    print("exists")
                end
            "#
        ));
    }

    #[test]
    fn test_nil_guard_or_default_pattern() {
        let mut ws = VirtualWorkspace::new();
        // Field used in `or` default pattern should be suppressed
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class OrDefaultTest
                local obj = {}
                local val = obj.unknownField or 42
            "#
        ));
    }

    #[test]
    fn test_nil_guard_not_condition() {
        let mut ws = VirtualWorkspace::new();
        // Field used in `not field` condition should be suppressed
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class NotCondTest
                local obj = {}
                if not obj.unknownField then
                    return
                end
            "#
        ));
    }

    #[test]
    fn test_nil_guard_and_pattern() {
        let mut ws = VirtualWorkspace::new();
        // Field used as left side of `and` should be suppressed
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class AndPatternTest
                local obj = {}
                local val = obj.unknownField and obj.unknownField()
            "#
        ));
    }

    #[test]
    fn test_unm_operator_preserves_type() {
        let mut ws = VirtualWorkspace::new();
        // Unary minus on a class with __unm operator should preserve the type
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class TestVec
                ---@operator unm: TestVec
                local TestVec = {}

                function TestVec:SomeMethod()
                    return 1
                end

                ---@type TestVec
                local v = TestVec

                local neg = -v
                neg:SomeMethod()
            "#
        ));
    }

    #[test]
    fn test_nil_guard_type_check_in_condition() {
        let mut ws = VirtualWorkspace::new();
        // type(obj.field) == "table" should suppress undefined-field
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class TypeCheckTest
                local obj = {}
                if type(obj.unknownField) == "table" then
                    print("yes")
                end
            "#
        ));
    }

    #[test]
    fn test_nil_guard_local_assign_then_nil_check() {
        let mut ws = VirtualWorkspace::new();
        // local x = obj.field; if x then ... should suppress undefined-field
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class LocalAssignTest
                local obj = {}
                local x = obj.unknownField
                if x then
                    print(x)
                end
            "#
        ));
    }

    #[test]
    fn test_nil_guard_early_return() {
        let mut ws = VirtualWorkspace::new();
        // if not obj.field then return end; ... obj.field should suppress
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class EarlyReturnTest
                local obj = {}
                if not obj.unknownField then return end
                local x = obj.unknownField .. "suffix"
            "#
        ));
    }

    #[test]
    fn test_nil_guard_reassignment_should_not_suppress() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class ReassignEntity
                ---@field species string
                local entity = { species = "Dog" }
                if entity.nickname ~= nil then
                    entity = { species = "Cat" }
                    print(entity.nickname)
                end
            "#
        ));
    }

    #[test]
    fn test_nil_guard_reassignment_in_for_loop_should_not_suppress() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class ReassignLoopEntity
                ---@field species string
                local entity = { species = "Dog" }
                if entity.nickname ~= nil then
                    for i = 1, 1 do
                        entity = { species = "Cat" }
                    end
                    print(entity.nickname)
                end
            "#
        ));
    }

    #[test]
    fn test_nil_guard_reassignment_in_while_loop_should_not_suppress() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class ReassignWhileEntity
                ---@field species string
                local entity = { species = "Dog" }
                if entity.nickname ~= nil then
                    while false do
                        entity = { species = "Cat" }
                    end
                    print(entity.nickname)
                end
            "#
        ));
    }

    #[test]
    fn test_func_stat_method_def_on_returned_type_not_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        // Definition-only: should NOT produce undefined-field.
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class FuncStatRetPanel
                ---@field name string

                ---@return FuncStatRetPanel
                local function CreatePanel() return {} end

                local row = CreatePanel()

                function row:RefreshFieldVisibility()
                end
            "#
        ));
    }

    #[test]
    fn test_func_stat_method_call_after_def_not_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        // Call after definition: should NOT produce undefined-field.
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class FuncStatCallPanel
                ---@field name string

                ---@return FuncStatCallPanel
                local function CreatePanel() return {} end

                local row = CreatePanel()

                function row:RefreshFieldVisibility()
                end

                row:RefreshFieldVisibility()
            "#
        ));
    }

    #[test]
    fn test_func_stat_dot_def_on_returned_type_not_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class FuncStatDotRetPanel
                ---@field name string

                ---@return FuncStatDotRetPanel
                local function CreatePanel() return {} end

                local row = CreatePanel()

                function row.MyStaticFunc()
                    return 1
                end
            "#
        ));
    }

    #[test]
    fn test_func_stat_multiple_method_defs_and_calls() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class FuncStatMultiPanel
                ---@field data table

                ---@return FuncStatMultiPanel
                local function CreatePanel() return {} end

                local row = CreatePanel()

                function row:RefreshLayout()
                end

                function row:RefreshFieldVisibility()
                end

                row:RefreshFieldVisibility()
            "#
        ));
    }

    #[test]
    fn test_func_stat_method_is_resolvable_on_ref_type() {
        let mut ws = VirtualWorkspace::new();
        // Verify the method is actually resolvable (not just diagnostic-suppressed).
        // The method should be a Signature type, not Unknown.
        ws.def(
            r#"
                ---@class FuncStatResolvePanel
                ---@field name string

                ---@return FuncStatResolvePanel
                local function CreatePanel() return {} end

                local row = CreatePanel()

                function row:RefreshFieldVisibility()
                end

                A = row.RefreshFieldVisibility
            "#,
        );
        let ty = ws.expr_ty("A");
        assert!(
            !ty.is_unknown(),
            "func-stat method on Ref type should be resolvable, got Unknown"
        );
    }

    #[test]
    fn test_func_stat_method_does_not_pollute_class() {
        let mut ws = VirtualWorkspace::new();
        // Regression test: a method defined via func-stat on a Ref-typed local
        // must NOT leak to other instances of the same class.
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class FuncStatPollutionPanel
                ---@field name string

                ---@return FuncStatPollutionPanel
                local function CreatePanel() return {} end

                local row = CreatePanel()

                function row:LocalOnlyMethod()
                end

                local other = CreatePanel()
                other:LocalOnlyMethod()
            "#
        ));
    }

    #[test]
    fn test_regression_typed_assignment_accumulate() {
        let mut ws = VirtualWorkspace::new();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        // File 1
        ws.def_file(
            "lua/glide/sh_lighting_api.lua",
            r#"
            ---@class Lighting
            Lighting = Lighting or {}
            Lighting.Standard = Lighting.Standard or {}
        "#,
        );

        // File 2
        let file2 = ws.def_file(
            "lua/glide/sh_lighting_api_2.lua",
            r#"
            ---@class Lighting
            Lighting = Lighting or {}
            Lighting.Standard = Lighting.Standard or {}

            local Z = Lighting.Standard
        "#,
        );

        let diags = ws
            .analysis
            .diagnose_file(file2, tokio_util::sync::CancellationToken::new())
            .unwrap_or_default();

        let has_undefined = diags.iter().any(|d| {
            d.code.as_ref()
                == Some(&lsp_types::NumberOrString::String(
                    "undefined-field".to_string(),
                ))
        });
        assert!(!has_undefined, "Expected no undefined-field, but got one");
    }
}
