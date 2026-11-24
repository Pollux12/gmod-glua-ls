#[cfg(test)]
mod test {
    use glua_parser::{
        LuaAstNode, LuaAstToken, LuaCallExpr, LuaExpr, LuaFuncStat, LuaIndexKey, LuaLocalFuncStat,
        LuaLocalName, LuaTableField, LuaVarExpr,
    };
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{
        DiagnosticCode, Emmyrc, InFiled, LuaMemberKey, LuaMemberOwner, LuaSignatureId, LuaType,
        VirtualWorkspace,
    };

    fn signature_return_type(ws: &VirtualWorkspace, file_id: crate::FileId, name: &str) -> LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let root = semantic_model.get_root();
        let closure = root
            .descendants::<LuaFuncStat>()
            .find(|stat| function_stat_name_is(stat, name))
            .and_then(|func_stat| func_stat.get_closure())
            .or_else(|| {
                root.descendants::<LuaLocalFuncStat>()
                    .find(|stat| local_function_stat_name_is(stat, name))
                    .and_then(|func_stat| func_stat.get_closure())
            })
            .expect("expected function declaration");
        let signature_id = LuaSignatureId::from_closure(file_id, &closure);
        semantic_model
            .get_db()
            .get_signature_index()
            .get(&signature_id)
            .expect("expected function signature")
            .get_return_type()
    }

    fn table_field_closure_signatures(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
        field_name: &str,
    ) -> Vec<LuaSignatureId> {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        semantic_model
            .get_root()
            .descendants::<LuaTableField>()
            .filter_map(|field| {
                let Some(LuaIndexKey::Name(key)) = field.get_field_key() else {
                    return None;
                };
                if key.get_name_text() != field_name {
                    return None;
                }
                let Some(LuaExpr::ClosureExpr(closure)) = field.get_value_expr() else {
                    return None;
                };
                Some(LuaSignatureId::from_closure(file_id, &closure))
            })
            .collect()
    }

    fn function_stat_name_is(stat: &LuaFuncStat, name: &str) -> bool {
        match stat.get_func_name() {
            Some(LuaVarExpr::IndexExpr(index_expr)) => {
                matches!(index_expr.get_index_key(), Some(LuaIndexKey::Name(name_token)) if name_token.get_name_text() == name)
            }
            Some(LuaVarExpr::NameExpr(name_expr)) => {
                name_expr.get_name_text().as_deref() == Some(name)
            }
            _ => false,
        }
    }

    fn local_function_stat_name_is(stat: &LuaLocalFuncStat, name: &str) -> bool {
        matches!(
            stat.get_local_name().and_then(|local_name| local_name.get_name_token()),
            Some(name_token) if name_token.get_name_text() == name
        )
    }
    #[test]
    fn test_metatable() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            cmd = setmetatable({}, {
                --- @param command string|string[]
                __call = function (_, command)
                end,

                --- @param command string
                --- @return fun(...:string)
                __index = function(_, command)
                end,
            })
            "#,
        );

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            cmd(1)
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            cmd("hello)
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            cmd({ "hello", "world" })
        "#
        ));

        let ty = ws.expr_ty("cmd.hihihi");
        let ty_desc = ws.humanize_type(ty);
        assert_eq!(ty_desc, "fun(...: string)");
    }

    #[test]
    fn test_metatable_2() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class switch
            ---@field map table
            ---@field cachedCases table
            local switchMT = {}
            switchMT.__index = switchMT

            ---@return switch
            local function switch()
                local obj = setmetatable({
                    map = {},
                    cachedCases = {},
                }, switchMT)
                a =  obj
            end
            "#,
        );

        let ty = ws.expr_ty("a");
        assert_eq!(ws.humanize_type(ty), "switch");
    }

    #[test]
    fn test_issue_599() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Class.Config
            ---@field abc string
            local ClassConfigMeta = {}

            ---@type table<string, Class.Config>
            local _classConfigMap = {}


            ---@param name string
            ---@return Class.Config
            local function getConfig(name)
                local config = _classConfigMap[name]
                if not config then
                    A = setmetatable({ name = name }, { __index = ClassConfigMeta })
                end
            end
            "#,
        );

        let ty = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(ty), "Class.Config");
    }

    #[test]
    fn test_return_setmetatable_data_or_table_keeps_metatable_methods() {
        let mut ws = VirtualWorkspace::new();
        ws.enable_check(DiagnosticCode::UndefinedField);

        let file_id = ws.def_file(
            "test.lua",
            r#"
            Glide = Glide or {}
            Glide.WeaponRegistry = Glide.WeaponRegistry or {}

            local BaseWeapon = {}
            function BaseWeapon:Initialize() end
            function BaseWeapon:Fire() end

            Glide.WeaponRegistry["base"] = BaseWeapon

            function Glide.CreateVehicleWeapon(className, data)
                local class = Glide.WeaponRegistry[className]
                assert(class)
                return setmetatable(data or {}, { __index = class })
            end

            local weapon = Glide.CreateVehicleWeapon("base")
            weapon:Initialize()
            weapon:Fire()
            A = weapon
            "#,
        );

        let return_ty = signature_return_type(&ws, file_id, "CreateVehicleWeapon");
        assert!(
            matches!(&return_ty, LuaType::Instance(_)),
            "expected CreateVehicleWeapon to keep a metatable-backed instance return type, got {return_ty:?}"
        );
        let return_ty_desc = ws.humanize_type(return_ty);
        let initialize_member_ty = ws.expr_ty("A.Initialize");
        let fire_member_ty = ws.expr_ty("A.Fire");
        assert!(
            matches!(
                initialize_member_ty,
                LuaType::Signature(_) | LuaType::DocFunction(_)
            ),
            "expected CreateVehicleWeapon return type ({return_ty_desc}) to expose Initialize via metatable, got {initialize_member_ty:?}"
        );
        assert!(
            matches!(
                fire_member_ty,
                LuaType::Signature(_) | LuaType::DocFunction(_)
            ),
            "expected CreateVehicleWeapon return type ({return_ty_desc}) to expose Fire via metatable, got {fire_member_ty:?}"
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        let undefined_field_diags: Vec<_> = diagnostics
            .iter()
            .filter(|diag| diag.code == undefined_field_code)
            .collect();

        assert!(
            undefined_field_diags.is_empty(),
            "unexpected UndefinedField diagnostics for metatable-backed weapon methods: {undefined_field_diags:?}"
        );
    }

    #[test]
    fn test_setmetatable_return_signature_uses_index_type_not_table() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "test.lua",
            r#"
            Glide = Glide or {}
            Glide.WeaponRegistry = Glide.WeaponRegistry or {}

            VSWEP = {}
            function VSWEP:Initialize() end
            function VSWEP:Fire() end

            local function Register(className)
                Glide.WeaponRegistry[className] = VSWEP
            end

            Register("base")

            function Glide.CreateVehicleWeapon(className, data)
                local class = Glide.WeaponRegistry[className]
                assert(class, "Tried to create invalid weapon class: " .. className)

                return setmetatable(data or {}, { __index = class })
            end

            local weapon = Glide.CreateVehicleWeapon("base")
            A = weapon.Initialize
            "#,
        );

        let return_ty = signature_return_type(&ws, file_id, "CreateVehicleWeapon");
        assert!(
            matches!(&return_ty, LuaType::Instance(_)),
            "setmetatable return should be a metatable-backed instance, got {return_ty:?}"
        );

        let initialize_member_ty = ws.expr_ty("A");
        assert!(
            matches!(
                initialize_member_ty,
                LuaType::Signature(_) | LuaType::DocFunction(_)
            ),
            "expected returned weapon to expose Initialize from __index, got {initialize_member_ty:?}"
        );
    }

    #[test]
    fn test_in_place_setmetatable_name_argument_uses_table_backing_range() {
        let mut ws = VirtualWorkspace::new();
        ws.enable_check(DiagnosticCode::UndefinedField);

        let file_id = ws.def_file(
            "test.lua",
            r#"
            local base = {}
            function base:Init() end

            local obj = {}
            setmetatable(obj, { __index = base })

            obj:Init()
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        let undefined_field_diags: Vec<_> = diagnostics
            .iter()
            .filter(|diag| diag.code == undefined_field_code)
            .collect();

        assert!(
            undefined_field_diags.is_empty(),
            "unexpected UndefinedField diagnostics for in-place setmetatable: {undefined_field_diags:?}"
        );
    }

    #[test]
    fn test_helix_character_keeps_meta_methods_through_return_and_map() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_file(
            "annotations/core.lua",
            r#"
            ---@meta
            ---@class Entity
            ---@class Player : Entity

            ---@generic T : table
            ---@param metaName `T`
            ---@return (definition) T
            function FindMetaTable(metaName) end

            ---@return any
            function Entity:GetNetVar(key) end
            "#,
        );
        ws.def_file(
            "gamemodes/helix/gamemode/core/meta/sh_character.lua",
            r#"
            ix = ix or {}
            ix.meta = ix.meta or {}
            ix.char = ix.char or {}
            ix.char.loaded = ix.char.loaded or {}

            local CHAR = ix.meta.character or {}
            CHAR.__index = CHAR
            ---@return integer
            function CHAR:GetID() return self.id end
            ---@return string
            function CHAR:GetInventory() return self.inv end
            ix.meta.character = CHAR
            "#,
        );
        ws.def_file(
            "gamemodes/helix/gamemode/core/libs/sh_character.lua",
            r#"
            function ix.char.New(data, id)
                local character = setmetatable({vars = {}}, ix.meta.character)
                character.id = id
                return character
            end

            function ix.char.Load(data, id)
                ix.char.loaded[id] = ix.char.New(data, id)
            end

            do
                local playerMeta = FindMetaTable("Player")
                function playerMeta:GetCharacter()
                    return ix.char.loaded[self:GetNetVar("char")]
                end
            end
            "#,
        );
        let probe = ws.def_file(
            "gamemodes/ixhl2rp/schema/sh_probe.lua",
            r#"
            ---@type Player
            local p
            local returned_id = ix.char.New({}, 1):GetID()
            local mapped_inventory = p:GetCharacter():GetInventory()
            "#,
        );

        assert_eq!(ws.local_ty(probe, "returned_id"), LuaType::Integer);
        assert_eq!(ws.local_ty(probe, "mapped_inventory"), LuaType::String);
    }

    #[test]
    fn test_mutual_anonymous_metatable_index_cycle_does_not_overflow() {
        let mut ws = VirtualWorkspace::new();
        ws.enable_check(DiagnosticCode::UndefinedField);

        let file_id = ws.def_file(
            "test.lua",
            r#"
            local mtA = {}
            local mtB = {}
            local a = setmetatable({}, mtA)
            local b = setmetatable({}, mtB)

            mtA.__index = b
            mtB.__index = a

            local value = a.missingField
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));

        assert!(
            diagnostics
                .iter()
                .any(|diag| diag.code == undefined_field_code),
            "expected UndefinedField diagnostic for missing member through mutual __index cycle, got {diagnostics:?}"
        );
    }

    #[test]
    fn test_composite_anonymous_metatable_index_cycle_does_not_overflow() {
        let mut ws = VirtualWorkspace::new();
        ws.enable_check(DiagnosticCode::UndefinedField);

        let file_id = ws.def_file(
            "test.lua",
            r#"
            local mtA = {}
            local mtB = {}
            local a = setmetatable({}, mtA)
            local b = setmetatable({}, mtB)

            local function pick(flag)
                if flag then
                    return a
                end

                return b
            end

            local cycle = pick(MAYBE_FLAG)
            mtA.__index = cycle
            mtB.__index = cycle

            local value = a.missingField
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));

        assert!(
            diagnostics
                .iter()
                .any(|diag| diag.code == undefined_field_code),
            "expected UndefinedField diagnostic for missing member through composite __index cycle, got {diagnostics:?}"
        );
    }

    #[test]
    fn test_setmetatable_factory_fields_transfer_to_class_owner() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "test.lua",
            r#"
            local Animation = {}
            Animation.__index = Animation

            function MakeAnimation()
                local anim = {}
                anim.Func = function() end
                anim.Panel = "panel"
                return setmetatable(anim, Animation)
            end
            "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let root = semantic_model.get_root();
        let animation_range = root
            .descendants::<glua_parser::LuaLocalStat>()
            .find_map(|stat| {
                let names = stat.get_local_name_list().collect::<Vec<_>>();
                let values = stat.get_value_exprs().collect::<Vec<_>>();
                let idx = names
                    .iter()
                    .position(|name| name.get_text() == "Animation")?;
                match values.get(idx)? {
                    LuaExpr::TableExpr(table) => Some(table.get_range()),
                    _ => None,
                }
            })
            .expect("expected Animation table literal");
        let owner = LuaMemberOwner::Element(InFiled::new(file_id, animation_range));
        let members = semantic_model
            .get_db()
            .get_member_index()
            .get_members(&owner)
            .expect("expected class owner members");

        assert!(
            members
                .iter()
                .any(|member| member.get_key() == &LuaMemberKey::Name("Func".into())),
            "expected Func to be transferred to class owner, got {members:#?}"
        );
        assert!(
            members
                .iter()
                .any(|member| member.get_key() == &LuaMemberKey::Name("Panel".into())),
            "expected Panel to be transferred to class owner, got {members:#?}"
        );
    }

    /// A prototype method reads the fields every instance its constructor
    /// literal makes starts with.
    fn prototype_reads_constructor_fields(metatable: &str, constructor_metatable: &str) -> bool {
        let mut ws = VirtualWorkspace::new();
        ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            &format!(
                r#"
                local Tween = {{}}
                {metatable}

                function Tween:set(clock)
                    if clock >= self.duration then
                        self.clock = self.duration
                    end
                end

                function Tween:update(dt)
                    return self:set(self.clock + dt)
                end

                ---@param duration number
                function NewTween(duration)
                    return setmetatable({{ duration = duration, clock = 0 }}, {constructor_metatable})
                end
                "#
            ),
        )
    }

    #[test]
    fn test_self_indexed_prototype_reads_inline_constructor_fields() {
        assert!(prototype_reads_constructor_fields(
            "Tween.__index = Tween",
            "Tween"
        ));
    }

    #[test]
    fn test_prototype_behind_index_metatable_reads_inline_constructor_fields() {
        assert!(prototype_reads_constructor_fields(
            "local Tween_mt = { __index = Tween }",
            "Tween_mt"
        ));
    }

    /// Editing the constructor literal changes what the prototype's methods
    /// read, and editing it back restores it.
    #[test]
    fn test_prototype_follows_edits_to_its_constructor_literal() {
        let source = |fields: &str| {
            format!(
                r#"
                local Tween = {{}}
                Tween.__index = Tween

                function Tween:set(clock)
                    if clock >= self.duration then
                        self.clock = self.duration
                    end
                end

                function Tween:update(dt)
                    return self:set(self.clock + dt)
                end

                ---@param duration number
                function NewTween(duration)
                    return setmetatable({{ {fields} }}, Tween)
                end
                "#
            )
        };
        let mut ws = VirtualWorkspace::new();
        let mut reads_set_fields = |fields: &str| {
            ws.check_file_for(
                DiagnosticCode::NeedCheckNil,
                "lua/autorun/sh_tween.lua",
                &source(fields),
            )
        };
        let full = reads_set_fields("duration = duration, clock = 0");
        let without_duration = reads_set_fields("clock = 0");
        let restored = reads_set_fields("duration = duration, clock = 0");

        assert_eq!([full, without_duration, restored], [true, false, true]);
    }

    /// A table of defaults behind `__index` has no methods reading an
    /// instance as `self`, so what instances start with is not its own.
    #[test]
    fn test_defaults_table_behind_index_keeps_its_own_fields() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "test.lua",
            r#"
            local defaults = { color = "red" }
            local mt = { __index = defaults }

            function NewThing()
                return setmetatable({ size = 1 }, mt)
            end
            "#,
        );
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let defaults_range = semantic_model
            .get_root()
            .descendants::<glua_parser::LuaTableExpr>()
            .next()
            .expect("expected the defaults table")
            .get_range();
        let owner = LuaMemberOwner::Element(InFiled::new(file_id, defaults_range));
        let keys = semantic_model
            .get_db()
            .get_member_index()
            .get_members(&owner)
            .unwrap_or_default()
            .iter()
            .map(|member| member.get_key().clone())
            .collect::<Vec<_>>();

        assert_eq!(keys, vec![LuaMemberKey::Name("color".into())]);
    }

    #[test]
    fn test_environment_behind_index_keeps_globals_unchanged() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedGlobal,
            r#"
            local env = setmetatable({ sandboxed = true }, { __index = _G })
            print(sandboxed)
            "#,
        ));
    }

    #[test]
    fn test_callable_table_parameter_resolves_as_instance_metatable() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local Setting = {
                __index = {
                    apply = function(self)
                        return self.value
                    end,
                    update = function(self, value)
                        self.value = value
                        return self:apply()
                    end,
                    convert = function(self, value)
                        return value
                    end,
                },
                __call = function(class)
                    local self = setmetatable({}, class)
                    self:convert("initial")
                    local callback = function(value)
                        self:update(value)
                    end
                    callback("changed")
                    return self
                end,
            }

            setmetatable(Setting, Setting)
            "#,
        ));
    }

    #[test]
    fn test_callable_self_metatable_constructor_preserves_raw_and_index_members() {
        const SOURCE: &str = r#"
            local pending
            local function runLater(callback)
                pending = callback
            end

            local Parent = {}
            function Parent:Inherited() end
            function Parent:unregister() end

            local Methods = {}
            function Methods:Own() end
            setmetatable(Methods, { __index = Parent })

            local Constructor = {
                __index = Methods,
                __call = function(p)
                    local t = {
                        bass = true,
                        fadeStart = 0,
                        fadeEnd = 1,
                    }
                    runLater(function()
                        t.rate = 1
                        t.max = 10
                        t:Own()
                        t:Inherited()
                        t:unregister()
                    end)
                    return setmetatable(t, p)
                end,
            }
            setmetatable(Constructor, Constructor)

            local value = Constructor()
            pending()
            print(value.bass, value.fadeStart, value.fadeEnd, value.rate, value.max)
            value:Own()
            value:Inherited()
            value:unregister()
            Result = value
        "#;

        let mut inspection = VirtualWorkspace::new();
        inspection.def(SOURCE);
        let result_type = inspection.expr_ty("Result");
        assert!(
            matches!(result_type, LuaType::Instance(_)),
            "callable constructor must return an instance retaining raw backing members, got {result_type:?}"
        );

        let mut fields = VirtualWorkspace::new();
        assert!(
            fields.check_code_for(DiagnosticCode::UndefinedField, SOURCE),
            "callable self-metatable constructor must retain raw and captured dynamic fields"
        );

        let mut methods = VirtualWorkspace::new();
        assert!(
            methods.check_code_for(DiagnosticCode::UndefinedMethod, SOURCE),
            "callable self-metatable constructor must retain direct and inherited __index methods"
        );
    }

    #[test]
    fn test_callable_render_stack_product_keeps_materialized_index_methods() {
        const SOURCE: &str = r#"
            local RenderStack = {
                __index = {
                    create = function(self, data)
                        return setmetatable({
                            run = self.runDirty,
                            data = data,
                        }, self.objindex)
                    end,
                    runDirty = function(self, flags) end,
                    makeDirty = function(self) end,
                },
                __call = function(p, maincode, properties)
                    local ret = setmetatable({
                        maincode = maincode,
                        properties = properties,
                    }, p)
                    ret.objindex = { __index = ret }
                    return ret
                end,
            }
            setmetatable(RenderStack, RenderStack)

            local HoloRenderStack = RenderStack({}, {})
            local entity = {}
            entity.renderstack = HoloRenderStack:create(entity)
            entity.renderstack:makeDirty()
        "#;

        let mut inspection = VirtualWorkspace::new();
        let file_id = inspection.def(SOURCE);
        let create_id = table_field_closure_signatures(&inspection, file_id, "create")[0];
        let db = inspection.analysis.compilation.get_db();
        let create_self = db
            .get_call_site_param_index()
            .get_inferred_param(&create_id, 0);
        assert!(
            matches!(create_self, Some(LuaType::Instance(_))),
            "create self must retain the callable product instance, got {create_self:?}"
        );
        let semantic_model = inspection
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let setmetatable_return = semantic_model
            .get_root()
            .descendants::<LuaCallExpr>()
            .find(|call| {
                matches!(
                    call.get_prefix_expr(),
                    Some(LuaExpr::NameExpr(name))
                        if name.get_name_text().as_deref() == Some("setmetatable")
                ) && call
                    .get_args_list()
                    .is_some_and(|args| args.get_args().count() == 2)
            })
            .and_then(|call| semantic_model.infer_expr(LuaExpr::CallExpr(call)).ok())
            .expect("expected inferred create setmetatable return");
        assert!(
            matches!(setmetatable_return, LuaType::Instance(_)),
            "fresh inference must retain the self.objindex metatable, got {setmetatable_return:?}"
        );
        let create_return = db
            .get_signature_index()
            .get(&create_id)
            .expect("expected create signature")
            .get_return_type();
        assert!(
            matches!(create_return, LuaType::Instance(_)),
            "create must return the self.objindex-backed instance, got {create_return:?}"
        );

        let mut methods = VirtualWorkspace::new();
        assert!(methods.check_code_for(DiagnosticCode::UndefinedMethod, SOURCE));
    }

    #[test]
    fn test_cross_file_global_render_stack_product_keeps_materialized_index_methods() {
        let mut inspection = VirtualWorkspace::new();
        inspection.def_file("lua/autorun/client/init.lua", "SF = {}");
        inspection.def_file("lua/autorun/server/init.lua", "SF = {}");
        let source_id = inspection.def_file(
            "lua/starfall/sflib.lua",
            r#"
            SF.RenderStack = {
                __index = {
                    create = function(self, data)
                        return setmetatable({
                            run = self.runDirty,
                            data = data,
                        }, self.objindex)
                    end,
                    runDirty = function(self, flags) end,
                    makeDirty = function(self) end,
                },
                __call = function(p, maincode, properties)
                    local ret = setmetatable({
                        maincode = maincode,
                        properties = properties,
                    }, p)
                    ret.objindex = { __index = ret }
                    return ret
                end,
            }
            setmetatable(SF.RenderStack, SF.RenderStack)
            "#,
        );
        let consumer_id = inspection.def_file(
            "lua/entities/starfall_hologram/cl_init.lua",
            r#"
            local HoloRenderStack = SF.RenderStack({}, {})
            local entity = {}
            entity.renderstack = HoloRenderStack:create(entity)
            entity.renderstack:makeDirty()
            "#,
        );

        let create_id = table_field_closure_signatures(&inspection, source_id, "create")[0];
        let db = inspection.analysis.compilation.get_db();
        let create_self = db
            .get_call_site_param_index()
            .get_inferred_param(&create_id, 0);
        assert!(
            matches!(create_self, Some(LuaType::Instance(_))),
            "cross-file create self must retain the callable product instance, got {create_self:?}"
        );
        let create_return = db
            .get_signature_index()
            .get(&create_id)
            .expect("expected create signature")
            .get_return_type();
        assert!(
            matches!(create_return, LuaType::Instance(_)),
            "cross-file create must return the self.objindex-backed instance, got {create_return:?}"
        );
        let semantic_model = inspection
            .analysis
            .compilation
            .get_semantic_model(consumer_id)
            .expect("expected consumer semantic model");
        let holo_local = semantic_model
            .get_root()
            .descendants::<LuaLocalName>()
            .find(|name| name.get_text() == "HoloRenderStack")
            .expect("expected HoloRenderStack local");
        let holo_type = semantic_model
            .get_semantic_info(
                holo_local
                    .get_name_token()
                    .expect("expected HoloRenderStack name token")
                    .syntax()
                    .clone()
                    .into(),
            )
            .expect("expected HoloRenderStack semantic info")
            .display_typ()
            .clone();
        assert!(
            matches!(holo_type, LuaType::Instance(_)),
            "cross-file callable product local must retain its instance, got {holo_type:?}"
        );
        inspection
            .analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedMethod);
        let diagnostics = inspection
            .analysis
            .diagnose_file(consumer_id, CancellationToken::new())
            .unwrap_or_default();
        assert!(
            diagnostics.iter().all(|diagnostic| {
                diagnostic.code
                    != Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            }),
            "cross-file materialized product must expose makeDirty, got {diagnostics:?}"
        );
    }

    #[test]
    fn test_callable_ent_manager_return_keeps_derived_and_inherited_methods() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local LimitObject = {
                __index = {
                    inherited = function(self) end,
                },
                __call = function(p)
                    return setmetatable({ count = 0 }, p)
                end,
            }
            setmetatable(LimitObject, LimitObject)

            local EntManager = {
                __index = {
                    unregister = function(self) end,
                },
                __call = function(p)
                    local t = LimitObject()
                    t.removeCb = function() end
                    return setmetatable(t, p)
                end,
            }
            setmetatable(EntManager, EntManager)
            setmetatable(EntManager.__index, LimitObject)

            local manager = EntManager()
            manager.removeCb()
            manager:unregister()
            manager:inherited()
            "#,
        ));
    }

    #[test]
    fn test_setmetatable_does_not_expose_derived_method_before_call() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local Base = {
                __index = {},
                __call = function(p)
                    return setmetatable({ value = true }, p)
                end,
            }
            setmetatable(Base, Base)

            local Derived = {
                __index = {
                    DerivedOnly = function(self) end,
                },
            }
            setmetatable(Derived, Derived)
            setmetatable(Derived.__index, Base)

            local function build()
                local t = Base()
                t:DerivedOnly()
                return setmetatable(t, Derived)
            end

            build()
            "#,
        ));
    }

    #[test]
    fn test_colon_receiver_contribution_requires_leading_self_name_leaves_methods_open() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            local Class = {
                __index = {
                    create = function(receiver)
                        return setmetatable({ value = true }, receiver.objindex)
                    end,
                    makeDirty = function(self) end,
                },
                __call = function(p)
                    local ret = setmetatable({}, p)
                    ret.objindex = { __index = ret }
                    return ret
                end,
            }
            setmetatable(Class, Class)

            Class():create():makeDirty()
            local probe = Class():create().makeDirty
            "#,
        );
        assert!(!is_function_type(&ws.local_ty(file_id, "probe")));
    }

    #[test]
    fn test_colon_receiver_contribution_rejects_nonleading_self_leaves_methods_open() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            local Class = {
                __index = {
                    create = function(prefix, self)
                        return setmetatable({ value = true }, self.objindex)
                    end,
                    makeDirty = function(self) end,
                },
                __call = function(p)
                    local ret = setmetatable({}, p)
                    ret.objindex = { __index = ret }
                    return ret
                end,
            }
            setmetatable(Class, Class)

            Class():create(nil):makeDirty()
            local probe = Class():create(nil).makeDirty
            "#,
        );
        assert!(!is_function_type(&ws.local_ty(file_id, "probe")));
    }

    #[test]
    fn test_colon_receiver_contribution_rejects_mutated_self_leaves_methods_open() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            local Class = {
                __index = {
                    create = function(self)
                        self = {}
                        return setmetatable({ value = true }, self.objindex)
                    end,
                    makeDirty = function(self) end,
                },
                __call = function(p)
                    local ret = setmetatable({}, p)
                    ret.objindex = { __index = ret }
                    return ret
                end,
            }
            setmetatable(Class, Class)

            Class():create():makeDirty()
            local probe = Class():create().makeDirty
            "#,
        );
        assert!(!is_function_type(&ws.local_ty(file_id, "probe")));
    }

    #[test]
    fn test_colon_receiver_contribution_rejects_ambiguous_method_signature() {
        const SOURCE: &str = r#"
            local MethodsA = {
                create = function(self)
                    return setmetatable({ value = true }, self.objindex)
                end,
            }
            local MethodsB = {
                create = function(self)
                    return setmetatable({ value = true }, self.objindex)
                end,
            }
            local function selectMethods()
                if unknownCondition then
                    return MethodsA
                end
                return MethodsB
            end
            local receiver = selectMethods()

            receiver:create():makeDirty()
        "#;

        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(SOURCE);
        let signature_ids = table_field_closure_signatures(&ws, file_id, "create");
        let call_site_params = ws.analysis.compilation.get_db().get_call_site_param_index();
        assert!(
            signature_ids.len() == 2
                && signature_ids.iter().all(|signature_id| call_site_params
                    .get_inferred_param(signature_id, 0)
                    .is_none()),
            "ambiguous receiver member signatures must not receive an exact self contribution"
        );
    }

    #[test]
    fn test_materialized_metatable_instance_missing_members_remain_visible() {
        const SOURCE: &str = r#"
            local Methods = {}
            local Constructor = {
                __index = Methods,
                __call = function(p)
                    return setmetatable({ shared = true }, p)
                end,
            }
            setmetatable(Constructor, Constructor)

            local first = Constructor()
            local second = Constructor()
            print(first.shared, second.shared, second.missingField)
            second:Missing()
        "#;

        let mut fields = VirtualWorkspace::new();
        assert!(
            !fields.check_code_for(DiagnosticCode::UndefinedField, SOURCE),
            "genuinely missing fields must remain diagnosable"
        );

        let mut methods = VirtualWorkspace::new();
        assert!(
            !methods.check_code_for(DiagnosticCode::UndefinedMethod, SOURCE),
            "genuinely missing methods must remain diagnosable"
        );
    }

    #[test]
    fn test_mutated_callable_table_parameter_is_not_used_as_instance_metatable() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local Setting = {
                __index = {
                    known = function(self) end,
                },
                __call = function(class)
                    class = {}
                    local self = setmetatable({}, class)
                    self:known()
                end,
            }

            setmetatable(Setting, Setting)
            "#,
        ));
    }

    #[test]
    fn test_callable_table_nonreceiver_parameter_leaves_instance_methods_open_but_unattributed() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            local Setting = {
                __index = {
                    known = function(self) end,
                },
                __call = function(class, metatable)
                    local self = setmetatable({}, metatable)
                    self:known()
                    local probe = self.known
                end,
            }

            setmetatable(Setting, Setting)
            "#,
        );
        assert!(!is_function_type(&ws.local_ty(file_id, "probe")));
    }

    #[test]
    fn test_named_metatable_factory_exposes_later_method() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.enable_check(DiagnosticCode::UndefinedMethod);
        ws.def_file(
            "lua/autorun/server/init.lua",
            "SF = {}\ninclude(\"starfall/sflib.lua\")\n",
        );
        ws.def_file(
            "lua/autorun/client/init.lua",
            "SF = {}\ninclude(\"starfall/sflib.lua\")\n",
        );
        ws.def_file("lua/starfall/sflib.lua", "include(\"instance.lua\")\n");
        let file_id = ws.def_file(
            "lua/starfall/instance.lua",
            r#"
            SF.Instance = {}
            SF.Instance.__index = SF.Instance

            function SF.Instance.make()
                local instance = setmetatable({}, SF.Instance)
                instance.value = true
                instance:later()
                return instance
            end

            function SF.Instance:later() end
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let undefined_method_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedMethod.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != undefined_method_code),
            "unexpected UndefinedMethod diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn test_colon_factory_receiver_resolves_exact_metatable_owner() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local manager = {}
            local requestClass = {}
            requestClass.__index = requestClass
            manager.requestClass = requestClass

            function requestClass:new(sender, receiver, amount, message, expiry, instance, callbackSuccess, callbackFailure)
                return setmetatable({
                    sender = sender,
                    receiver = receiver,
                    amount = amount,
                    message = message,
                    expiry = expiry,
                    instance = instance,
                    callbackSuccess = callbackSuccess,
                    callbackFailure = callbackFailure
                }, self)
            end

            if SERVER then
                function requestClass:send() end
                manager.requests = {}
                function manager:add(sender, receiver, amount, message, instance, callbackSuccess, callbackFailure)
                    local requestsForSender = self.requests[sender]
                    if not requestsForSender then
                        requestsForSender = {}
                        self.requests[sender] = requestsForSender
                    end
                    local expiry = CurTime() + 10
                    local request = requestClass:new(sender, receiver, amount, message, expiry, instance, callbackSuccess, callbackFailure)
                    requestsForSender[receiver] = request
                    request:send()
                end
            end
            "#,
        ));
    }

    #[test]
    fn test_arbitrary_factory_parameter_makes_an_instance_of_each_argument() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            local Class = {}
            Class.__index = Class
            function Class:known() end
            local Other = {}
            Other.__index = Other

            local function make(metatable)
                return setmetatable({}, metatable)
            end

            make(Class):known()
            local probe = make(Class).known
            local other = make(Other).known
            "#,
        );
        assert!(is_function_type(&ws.local_ty(file_id, "probe")));
        assert!(!is_function_type(&ws.local_ty(file_id, "other")));
    }

    #[test]
    fn test_mutated_colon_self_is_not_used_as_metatable_owner() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local Class = {}
            Class.__index = Class
            function Class:known() end

            local Other = {}
            function Class:new()
                self = Other
                return setmetatable({}, self)
            end

            Class:new():known()
            "#,
        ));
    }

    #[test]
    fn test_cross_file_callable_class_preserves_nested_index_methods() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.enable_check(DiagnosticCode::UndefinedMethod);
        ws.def_file(
            "lua/autorun/server/init.lua",
            "SF = {}\ninclude(\"starfall/sflib.lua\")\n",
        );
        ws.def_file(
            "lua/autorun/client/init.lua",
            "SF = {}\ninclude(\"starfall/sflib.lua\")\n",
        );
        ws.def_file(
            "lua/starfall/sflib.lua",
            r#"
            SF.BurstObject = {
                __index = {
                    use = function(self) end,
                    check = function(self) end,
                },
                __call = function(meta, name, limit, rate, max, rate_help, max_help, scale)
                    local instance = {
                        name = "burst",
                        objects = {},
                    }
                    register(function(value)
                        instance.rate = value
                    end)
                    register(function(value)
                        instance.max = value
                    end)
                    return setmetatable(instance, meta)
                end,
            }
            setmetatable(SF.BurstObject, SF.BurstObject)
            include("libs_sh/game.lua")
            "#,
        );
        let consumer = ws.def_file(
            "lua/starfall/libs_sh/game.lua",
            r#"
            local instance = SERVER and SF.BurstObject("name", "limit", 1, 2, "rate", "max", 1)
            if SERVER then
                instance:use()
                instance:check()
            end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(consumer, CancellationToken::new())
            .unwrap_or_default();
        let undefined_method_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedMethod.get_name().to_string(),
        ));

        assert!(
            diagnostics
                .iter()
                .all(|diag| diag.code != undefined_method_code),
            "unexpected UndefinedMethod diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn test_literal_index_later_function_overwrite_leaves_methods_open() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local Class = {}
            function Class:known() end

            local Meta = { __index = Class }
            Meta.__index = function(_, key)
                return rawget(Class, key)
            end

            local value = setmetatable({}, Meta)
            value:known()
            "#,
        ));
    }

    #[test]
    fn test_literal_index_later_dynamic_overwrite_rejects_factory_recovery() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            local Class = {}
            function Class:known() end

            local dynamic_index = unknown_provider()
            local Meta = { __index = Class }
            Meta.__index = dynamic_index

            local value = setmetatable({}, Meta)
            A = value.known
            "#,
        );
        let known_type = ws.expr_ty("A");
        assert!(
            !matches!(known_type, LuaType::Signature(_) | LuaType::DocFunction(_)),
            "dynamic overwrite must not recover Class.known, got {known_type:?}"
        );
    }

    #[test]
    fn test_literal_index_supported_later_overwrite_wins() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local Other = {}
            local Class = {}
            function Class:known() end

            local Meta = { __index = Other }
            Meta.__index = Class

            local value = setmetatable({}, Meta)
            value:known()
            "#,
        ));
    }

    #[test]
    fn test_duplicate_inline_index_uses_last_field() {
        let mut supported_last = VirtualWorkspace::new();
        supported_last.def(
            r#"
            local Class = {}
            function Class:known() end

            local value = setmetatable({}, {
                __index = function() end,
                __index = Class,
            })
            A = value.known
            "#,
        );
        let supported_type = supported_last.expr_ty("A");
        assert!(
            matches!(
                supported_type,
                LuaType::Signature(_) | LuaType::DocFunction(_)
            ),
            "last supported __index field must expose Class.known, got {supported_type:?}"
        );

        let mut unsupported_last = VirtualWorkspace::new();
        unsupported_last.def(
            r#"
            local Class = {}
            function Class:known() end

            local value = setmetatable({}, {
                __index = Class,
                __index = function() end,
            })
            A = value.known
            "#,
        );
        let unsupported_type = unsupported_last.expr_ty("A");
        assert!(
            !matches!(
                unsupported_type,
                LuaType::Signature(_) | LuaType::DocFunction(_)
            ),
            "last unsupported __index field must not expose Class.known, got {unsupported_type:?}"
        );
    }

    #[test]
    fn test_realm_conflicting_literal_index_owners_remain_conservative() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.enable_check(DiagnosticCode::UndefinedMethod);
        ws.def_file(
            "lua/autorun/server/init.lua",
            r#"
            RealmIndex = {}
            function RealmIndex:known() end
            RealmMeta = { __index = RealmIndex }
            include("literal_realm_factory.lua")
            "#,
        );
        ws.def_file(
            "lua/autorun/client/init.lua",
            r#"
            RealmMeta = {
                __index = function(_, key)
                    return rawget(RealmMeta, key)
                end,
            }
            include("literal_realm_factory.lua")
            "#,
        );
        let shared_file_id = ws.def_file(
            "lua/literal_realm_factory.lua",
            r#"
            local value = setmetatable({}, RealmMeta)
            value:known()
            "#,
        );
        let undefined_method_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedMethod.get_name().to_string(),
        ));
        let diagnostics = ws
            .analysis
            .diagnose_file(shared_file_id, CancellationToken::new())
            .unwrap_or_default();

        assert!(
            diagnostics
                .iter()
                .any(|diag| diag.code == undefined_method_code),
            "expected conflicting literal realm __index owners to preserve UndefinedMethod, got {diagnostics:?}"
        );
    }

    #[test]
    fn test_literal_index_overwrite_refreshes_across_edit_delete_and_reopen() {
        let mut ws = VirtualWorkspace::new();
        ws.enable_check(DiagnosticCode::UndefinedMethod);
        let uri = ws
            .virtual_url_generator
            .new_uri("lua/literal_metatable_lifecycle.lua");
        let supported_content = r#"
            local Other = {}
            local Class = {}
            function Class:known() end
            local Meta = { __index = Other }
            Meta.__index = Class
            local value = setmetatable({}, Meta)
            value:known()
        "#;
        let unsupported_content = r#"
            local Other = {}
            local Class = {}
            function Class:known() end
            local Meta = { __index = Other }
            local value = setmetatable({}, Meta)
            value:known()
        "#;
        let undefined_method_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedMethod.get_name().to_string(),
        ));
        let has_undefined_method = |ws: &VirtualWorkspace, file_id| {
            ws.analysis
                .diagnose_file(file_id, CancellationToken::new())
                .unwrap_or_default()
                .iter()
                .any(|diag| diag.code == undefined_method_code)
        };

        let initial_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(unsupported_content.to_string()))
            .map(|(id, _)| id)
            .expect("literal metatable lifecycle file must be created");
        assert!(has_undefined_method(&ws, initial_file_id));

        let edited_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(supported_content.to_string()))
            .map(|(id, _)| id)
            .expect("literal metatable lifecycle file must be updated");
        assert!(!has_undefined_method(&ws, edited_file_id));

        ws.analysis
            .remove_file_by_uri(&uri)
            .0
            .expect("literal metatable lifecycle file must be removed");
        let reopened_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(unsupported_content.to_string()))
            .map(|(id, _)| id)
            .expect("literal metatable lifecycle file must reopen");
        assert!(has_undefined_method(&ws, reopened_file_id));

        let restored_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(supported_content.to_string()))
            .map(|(id, _)| id)
            .expect("literal metatable lifecycle file must be restored");
        assert!(!has_undefined_method(&ws, restored_file_id));
    }

    #[test]
    fn test_metatable_receiver_ranges_refresh_across_edit_delete_and_reopen() {
        let mut ws = VirtualWorkspace::new();
        ws.enable_check(DiagnosticCode::UndefinedMethod);
        let uri = ws
            .virtual_url_generator
            .new_uri("lua/metatable_lifecycle.lua");
        let self_index_content = r#"
            local Class = {}
            Class.__index = Class
            function Class:known() end
            function Class:new()
                return setmetatable({}, self)
            end

            Class:new():known()
        "#;
        let other_index_content = r#"
            local Other = {}
            local Class = {}
            Class.__index = Other
            function Class:known() end
            function Class:new()
                return setmetatable({}, self)
            end

            Class:new():known()
        "#;
        let undefined_method_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedMethod.get_name().to_string(),
        ));
        let has_undefined_method = |ws: &VirtualWorkspace, file_id| {
            ws.analysis
                .diagnose_file(file_id, CancellationToken::new())
                .unwrap_or_default()
                .iter()
                .any(|diag| diag.code == undefined_method_code)
        };

        let initial_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(self_index_content.to_string()))
            .map(|(id, _)| id)
            .expect("metatable lifecycle file must be created");
        assert!(!has_undefined_method(&ws, initial_file_id));

        let edited_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(other_index_content.to_string()))
            .map(|(id, _)| id)
            .expect("metatable lifecycle file must be updated");
        assert!(has_undefined_method(&ws, edited_file_id));

        ws.analysis
            .remove_file_by_uri(&uri)
            .0
            .expect("metatable lifecycle file must be removed");
        let reopened_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(other_index_content.to_string()))
            .map(|(id, _)| id)
            .expect("metatable lifecycle file must reopen");
        assert!(has_undefined_method(&ws, reopened_file_id));

        let restored_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(self_index_content.to_string()))
            .map(|(id, _)| id)
            .expect("metatable lifecycle file must be restored");
        assert!(!has_undefined_method(&ws, restored_file_id));
    }

    #[test]
    fn test_realm_conflicting_index_owners_remain_conservative() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.enable_check(DiagnosticCode::UndefinedMethod);
        ws.def_file(
            "lua/autorun/server/init.lua",
            r#"
            RealmClass = {}
            RealmClass.__index = RealmClass
            function RealmClass:known() end
            include("realm_factory.lua")
            "#,
        );
        ws.def_file(
            "lua/autorun/client/init.lua",
            r#"
            local Other = {}
            RealmClass = {}
            RealmClass.__index = Other
            include("realm_factory.lua")
            "#,
        );
        let shared_file_id = ws.def_file(
            "lua/realm_factory.lua",
            r#"
            function RealmClass:new()
                return setmetatable({}, self)
            end

            RealmClass:new():known()
            "#,
        );
        let undefined_method_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedMethod.get_name().to_string(),
        ));
        let diagnostics = ws
            .analysis
            .diagnose_file(shared_file_id, CancellationToken::new())
            .unwrap_or_default();

        assert!(
            diagnostics
                .iter()
                .any(|diag| diag.code == undefined_method_code),
            "expected conflicting realm __index owners to preserve UndefinedMethod, got {diagnostics:?}"
        );
    }

    #[test]
    fn test_realm_mixed_table_and_function_index_remains_conservative() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.enable_check(DiagnosticCode::UndefinedMethod);
        ws.def_file(
            "lua/autorun/server/init.lua",
            r#"
            RealmClass = {}
            RealmClass.__index = RealmClass
            function RealmClass:known() end
            include("realm_function_index.lua")
            "#,
        );
        ws.def_file(
            "lua/autorun/client/init.lua",
            r#"
            RealmClass = {}
            RealmClass.__index = function(_, key)
                return rawget(RealmClass, key)
            end
            include("realm_function_index.lua")
            "#,
        );
        let shared_file_id = ws.def_file(
            "lua/realm_function_index.lua",
            r#"
            function RealmClass:new()
                return setmetatable({}, self)
            end

            RealmClass:new():known()
            "#,
        );
        let undefined_method_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedMethod.get_name().to_string(),
        ));
        let diagnostics = ws
            .analysis
            .diagnose_file(shared_file_id, CancellationToken::new())
            .unwrap_or_default();

        assert!(
            diagnostics
                .iter()
                .any(|diag| diag.code == undefined_method_code),
            "expected mixed table/function __index owners to preserve UndefinedMethod, got {diagnostics:?}"
        );
    }

    #[test]
    fn test_union_table_and_function_index_remains_conservative() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            ---@class UnionIndexClass
            local Class = {}
            ---@type UnionIndexClass|fun(self: table, key: string): unknown
            local index = Class
            Class.__index = index
            function Class:known() end

            function Class:new()
                return setmetatable({}, self)
            end

            Class:new():missing()
            "#,
        ));
    }

    #[test]
    fn test_metatable_factory_missing_method_still_diagnoses() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local Class = {}
            Class.__index = Class
            function Class:new()
                return setmetatable({}, self)
            end

            Class:new():missing()
            "#,
        ));
    }

    #[test]
    fn test_non_self_index_does_not_expose_class_method() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local Class = {}
            local Other = {}
            Class.__index = Other
            function Class:known() end
            function Class:new()
                return setmetatable({}, self)
            end

            Class:new():known()
            "#,
        ));
    }

    #[test]
    fn concrete_inferred_returns_follow_late_member_types_to_a_fixpoint() {
        let mut ws = VirtualWorkspace::new();
        let player_path = "lua/a_player.lua";
        let player_source = r#"
            Player = Player or {}

            function Player:GetGender()
                return self._Gender or GENDER_NEUTRAL
            end

            local genders = Config.gender
            function Player:GetTitle()
                return genders[self:GetGender()].title
            end

            function Player:GetLabel()
                return self:GetTitle()
            end
        "#;
        ws.def_files(vec![
            (player_path, player_source),
            ("lua/b_male_write.lua", "Player._Gender = GENDER_MALE"),
            ("lua/c_female_write.lua", "Player._Gender = GENDER_FEMALE"),
            (
                "lua/d_config.lua",
                r#"
                Config = { gender = {
                    [GENDER_NEUTRAL] = { title = "neutral" },
                    [GENDER_MALE] = { title = "male" },
                    [GENDER_FEMALE] = { title = "female" },
                } }
                "#,
            ),
            (
                "lua/e_enums.lua",
                "GENDER_NEUTRAL = \"Neutral\"\nGENDER_MALE = \"Male\"\nGENDER_FEMALE = \"Female\"",
            ),
        ]);

        let player_uri = ws.virtual_url_generator.new_uri(player_path);
        let player_file = ws
            .analysis
            .get_file_id(&player_uri)
            .expect("expected player file");
        let title_return = signature_return_type(&ws, player_file, "GetTitle");
        let label_return = signature_return_type(&ws, player_file, "GetLabel");
        let cold = (
            ws.humanize_type_detailed(title_return),
            ws.humanize_type_detailed(label_return),
        );
        let expected = (
            "(\"female\"|\"male\"|\"neutral\")".to_string(),
            "(\"female\"|\"male\"|\"neutral\")".to_string(),
        );
        assert_eq!(cold, expected);

        ws.analysis
            .update_file_text_only(&player_uri, format!("{player_source}\n"));
        ws.analysis.reindex_files(vec![player_file]);

        let title_return = signature_return_type(&ws, player_file, "GetTitle");
        let label_return = signature_return_type(&ws, player_file, "GetLabel");
        let warm = (
            ws.humanize_type_detailed(title_return),
            ws.humanize_type_detailed(label_return),
        );
        assert_eq!(warm, expected);
    }

    #[test]
    fn inferred_returns_follow_late_settled_dynamic_fields_to_a_fixpoint() {
        // The writer's `Ranks[rank]` prefix only resolves once the
        // `pairs(Ranks)` loop variables settle, so `iconMat` lands after the
        // returner's `.iconMat` read was taken. Sorting the returner first
        // forces that late publication on a cold build.
        let mut ws = VirtualWorkspace::new();
        let returner_path = "lua/a_returner.lua";
        let returner_source = r#"
            ---@class IMaterial
            ---@return IMaterial
            function Material(path) end

            function GetRankData(rank)
                return Ranks[rank]
            end

            function GetRankIconMaterial(rank)
                return GetRankData(rank).iconMat
            end

            function GetRankMissing(rank)
                return GetRankData(rank).nope
            end
        "#;
        ws.def_files(vec![
            (returner_path, returner_source),
            (
                "lua/b_ranks.lua",
                r#"
                Ranks = {
                    admin = { title = "Admin" },
                    user = { title = "User" },
                }

                for rank, data in pairs(Ranks) do
                    Ranks[rank].iconMat = Material("icon.vmt")
                end
                "#,
            ),
        ]);

        let returner_uri = ws.virtual_url_generator.new_uri(returner_path);
        let returner_file = ws
            .analysis
            .get_file_id(&returner_uri)
            .expect("expected returner file");
        let cold_icon = ws.humanize_type_detailed(signature_return_type(
            &ws,
            returner_file,
            "GetRankIconMaterial",
        ));
        let cold_missing =
            ws.humanize_type_detailed(signature_return_type(&ws, returner_file, "GetRankMissing"));
        assert_eq!(cold_icon, "IMaterial");
        assert_eq!(cold_missing, "nil");

        ws.analysis
            .update_file_text_only(&returner_uri, format!("{returner_source}\n"));
        ws.analysis.reindex_files(vec![returner_file]);

        let warm_icon = ws.humanize_type_detailed(signature_return_type(
            &ws,
            returner_file,
            "GetRankIconMaterial",
        ));
        let warm_missing =
            ws.humanize_type_detailed(signature_return_type(&ws, returner_file, "GetRankMissing"));
        assert_eq!(warm_icon, "IMaterial");
        assert_eq!(warm_missing, "nil");
    }

    #[test]
    fn test_class_annotated_metatable_keeps_methods_after_in_place_setmetatable() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local settings = {}

            ---@class Convar
            local meta = {}
            meta.__index = meta

            function meta:GetValue()
                return self.value
            end

            local function Add(name)
                local t = {}
                setmetatable(t, meta)
                t:GetValue()
                settings[name] = t
                return t
            end

            local obj = settings["a"]
            obj:GetValue()
            "#,
        ));
    }

    #[test]
    fn test_accessor_func_on_unannotated_metatable_table_keeps_accessors() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local meta = {}
            meta.__index = meta
            AccessorFunc(meta, "iMat", "Material")

            local function Create()
                local t = {}
                setmetatable(t, meta)
                t:SetMaterial("a")
                return t:GetMaterial()
            end
            "#,
        ));
    }

    #[test]
    fn test_class_annotated_metatable_factory_fields_are_class_fields() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@class Weather
            local meta = {}
            meta.__index = meta

            ---@return Weather
            function Create(name)
                local t = {}
                t.Name = name
                setmetatable(t, meta)
                return t
            end

            local w = Create("rain")
            print(w.Name)
            "#,
        ));
    }

    #[test]
    fn test_metatable_factory_constructor_fields_are_instance_fields() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            local meta = {}
            meta.__index = meta

            function meta:Describe()
                return self.name, self[1]
            end

            local function create(name, first)
                local t = { first, name = name }
                setmetatable(t, meta)
                return t
            end
            "#,
        ));
    }

    #[test]
    fn test_metatable_factory_positional_constructor_fields_are_instance_fields() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            local meta = {}
            meta.__index = meta

            function meta:First()
                return self[1], self[2]
            end

            local function create(ent, center)
                local t = { ent, center }
                setmetatable(t, meta)
                return t
            end
            "#,
        ));
    }

    #[test]
    fn test_metatable_factory_positional_constructor_does_not_open_other_indices() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            local meta = {}
            meta.__index = meta

            function meta:Third()
                return self[3]
            end

            local function create(ent, center)
                local t = { ent, center }
                setmetatable(t, meta)
                return t
            end
            "#,
        ));
    }

    #[test]
    fn test_metatable_index_taken_from_a_function_defined_elsewhere_leaves_methods_open() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local methods = {}
            function methods:Known() end

            local shared = {
                __index = function(_, key)
                    return methods[key]
                end,
            }

            local object = setmetatable({}, { __index = shared.__index })
            object:Known()
            "#,
        ));
    }

    #[test]
    fn test_named_metatable_index_taken_from_a_function_defined_elsewhere_leaves_methods_open() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local methods = {}
            function methods:Known() end

            local shared = {
                __index = function(_, key)
                    return methods[key]
                end,
            }

            local metatable = { __index = shared.__index }
            local object = setmetatable({}, metatable)
            object:Known()
            "#,
        ));
    }

    #[test]
    fn test_metatable_without_index_still_reports_undefined_methods() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local shared = { __call = function() end }
            local object = setmetatable({}, { __call = shared.__call })
            object:Known()
            "#,
        ));
    }

    /// `getmetatable` answers with the metatable `setmetatable` gave the
    /// table, whether the table is the call's result or was passed to it.
    #[test]
    fn getmetatable_returns_the_metatable_the_table_was_given() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/cat.lua",
            r#"
            local CAT = {}
            CAT.__index = CAT
            function CAT:Meow() return "meow" end

            local cat = setmetatable({}, CAT)
            local from_result = getmetatable(cat).Meow

            local dog = {}
            setmetatable(dog, CAT)
            local from_statement = getmetatable(dog).Meow
            "#,
        );

        for name in ["from_result", "from_statement"] {
            let typ = ws.local_ty(file_id, name);
            assert!(is_function_type(&typ), "{name}: {}", ws.humanize_type(typ));
        }
    }

    const REGISTERED_COLOR: (&str, &str) = (
        "lua/autorun/sh_color.lua",
        r#"
        local COLOR = {}
        COLOR.__index = COLOR
        function COLOR:Brighten() return self end
        RegisterMetaTable("MyColor", COLOR)
        local made = setmetatable({}, COLOR)
        "#,
    );

    fn registered_color_workspace(reader: &str) -> (VirtualWorkspace, crate::FileId) {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![REGISTERED_COLOR]);
        let reader = ws.def_file("lua/autorun/sh_reader.lua", reader);
        (ws, reader)
    }

    /// `RegisterMetaTable(name, table)` is a class registration call.
    #[test]
    fn register_metatable_is_filed_as_a_class_registration() {
        let (ws, _) = registered_color_workspace("");
        let db = ws.analysis.compilation.get_db();
        let uri = ws.virtual_url_generator.new_uri(REGISTERED_COLOR.0);
        let file_id = ws.analysis.get_file_id(&uri).expect("file id");
        let calls = db
            .get_gmod_class_metadata_index()
            .get_file_metadata(&file_id)
            .map(|metadata| metadata.metatable_register_calls.len());
        assert_eq!(calls, Some(1));
    }

    /// A registered metatable is the class of that name: `FindMetaTable`
    /// returns it, with its methods and the fields registration adds, and the
    /// tables given it by `setmetatable` are its instances.
    #[test]
    fn a_registered_metatable_is_the_class_find_metatable_returns() {
        let (ws, reader) = registered_color_workspace(
            r#"
            local meta = FindMetaTable("MyColor")
            local brighten = meta.Brighten
            local name = meta.MetaName
            "#,
        );

        let meta = ws.local_ty(reader, "meta");
        assert_eq!(ws.humanize_type(meta), "MyColor");
        assert!(is_function_type(&ws.local_ty(reader, "brighten")));
        let name = ws.local_ty(reader, "name");
        assert_eq!(ws.humanize_type(name), "string");
        let uri = ws.virtual_url_generator.new_uri(REGISTERED_COLOR.0);
        let color_file = ws.analysis.get_file_id(&uri).expect("file id");
        let made = ws.local_ty(color_file, "made");
        assert_eq!(ws.humanize_type(made), "MyColor");
    }

    /// Renaming a registration, removing it and putting it back leaves what a
    /// fresh analysis of the final text has.
    #[test]
    fn editing_a_metatable_registration_matches_a_fresh_analysis() {
        let reader = (
            "lua/autorun/sh_reader.lua",
            "local meta = FindMetaTable(\"MyColor\")",
        );
        let renamed = REGISTERED_COLOR.1.replace("\"MyColor\"", "\"OtherColor\"");
        let unregistered = REGISTERED_COLOR
            .1
            .replace("RegisterMetaTable(\"MyColor\", COLOR)", "");
        let setup = |ws: &mut VirtualWorkspace| {
            ws.def_gmod_call_arg_builtins();
        };
        VirtualWorkspace::assert_warm_matches_cold(
            setup,
            &[REGISTERED_COLOR, reader],
            &[
                (REGISTERED_COLOR.0, &renamed),
                (REGISTERED_COLOR.0, &unregistered),
            ],
        );
        VirtualWorkspace::assert_warm_matches_cold(
            setup,
            &[(REGISTERED_COLOR.0, &unregistered), reader],
            &[REGISTERED_COLOR],
        );
    }

    /// `newproxy(true)` gives the proxy a metatable of its own, which
    /// `getmetatable` returns; `newproxy(p)` shares p's metatable, and reads
    /// on a proxy go through that metatable's `__index`.
    #[test]
    fn a_proxy_has_the_metatable_newproxy_gave_it() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/proxy.lua",
            r#"
            local proxy = newproxy(true)
            local meta = getmetatable(proxy)
            meta.__index = meta
            function meta:Name() return "proxy" end

            local other = newproxy(proxy)
            local from_meta = getmetatable(proxy).Name
            local from_shared = getmetatable(other).Name
            local called = other:Name()
            "#,
        );

        for name in ["from_meta", "from_shared"] {
            let typ = ws.local_ty(file_id, name);
            assert!(is_function_type(&typ), "{name}: {}", ws.humanize_type(typ));
        }
        let called = ws.local_ty(file_id, "called");
        assert_eq!(ws.humanize_type(called), "\"proxy\"");
    }

    /// A proxy's metatable registered under a name is that class, and the
    /// proxies sharing it are its instances.
    #[test]
    fn a_registered_proxy_metatable_is_a_class() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        let file_id = ws.def_file(
            "lua/autorun/sh_handle.lua",
            r#"
            local proxy = newproxy(true)
            local meta = getmetatable(proxy)
            RegisterMetaTable("Handle", meta)
            meta.__index = meta
            function meta:Close() return true end

            local handle = newproxy(proxy)
            local found = FindMetaTable("Handle")
            local close = found.Close
            "#,
        );

        let handle = ws.local_ty(file_id, "handle");
        assert_eq!(ws.humanize_type(handle), "Handle");
        let found = ws.local_ty(file_id, "found");
        assert_eq!(ws.humanize_type(found), "Handle");
        assert!(is_function_type(&ws.local_ty(file_id, "close")));
    }

    const HANDLE_PROXY: (&str, &str) = (
        "lua/autorun/sh_handle.lua",
        r#"
        local meta = FindMetaTable("Handle")
        local proxy = newproxy(true)
        meta = getmetatable(proxy)
        RegisterMetaTable("Handle", meta)
        meta.__index = meta
        function meta:Close() return true end
        HandleProxy = proxy
        "#,
    );

    /// The reload-safe form, which reuses the local `FindMetaTable` filled,
    /// registers the proxy's metatable too, and another file's proxies made
    /// from the shared one are its instances.
    #[test]
    fn a_proxy_metatable_registered_through_a_reused_local_is_a_class() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![HANDLE_PROXY]);
        let reader = ws.def_file(
            "lua/autorun/sh_reader.lua",
            r#"
            local handle = newproxy(HandleProxy)
            local closed = handle:Close()
            local close = FindMetaTable("Handle").Close
            "#,
        );

        let handle = ws.local_ty(reader, "handle");
        assert_eq!(ws.humanize_type(handle), "Handle");
        let closed = ws.local_ty(reader, "closed");
        assert_eq!(ws.humanize_type(closed), "true");
        assert!(is_function_type(&ws.local_ty(reader, "close")));
    }

    /// Dropping the registration or the proxy's own metatable, and putting
    /// them back, leaves what a fresh analysis of the final text has.
    #[test]
    fn editing_a_proxy_metatable_matches_a_fresh_analysis() {
        let reader = (
            "lua/autorun/sh_reader.lua",
            "local handle = newproxy(HandleProxy)\nlocal closed = handle:Close()",
        );
        let unregistered = HANDLE_PROXY
            .1
            .replace("RegisterMetaTable(\"Handle\", meta)", "");
        let no_metatable = HANDLE_PROXY.1.replace("newproxy(true)", "newproxy(false)");
        let setup = |ws: &mut VirtualWorkspace| {
            ws.def_gmod_call_arg_builtins();
        };
        VirtualWorkspace::assert_warm_matches_cold(
            setup,
            &[HANDLE_PROXY, reader],
            &[
                (HANDLE_PROXY.0, &unregistered),
                (HANDLE_PROXY.0, &no_metatable),
                HANDLE_PROXY,
            ],
        );
    }

    const METATABLE_FACTORY: (&str, &str) = (
        "lua/autorun/sh_meta.lua",
        r#"
        function CreateMetaTable(name)
            local metaName = "Example." .. name
            local meta = FindMetaTable(metaName)
            if meta then
                return meta
            end

            local proxy = newproxy(true)
            meta = getmetatable(proxy)
            RegisterMetaTable(metaName, meta)
            meta.__index = meta
            function meta:GetTable() return {} end
            return meta
        end
        "#,
    );

    /// A function that registers the metatable it returns makes each call a
    /// class registration: the local the call fills is the named class.
    #[test]
    fn a_metatable_factory_call_registers_the_class_it_returns() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![METATABLE_FACTORY]);
        let bone = ws.def_file(
            "lua/autorun/sh_bone.lua",
            r#"
            local BONE = CreateMetaTable("Bone")
            function BONE:Name() return "bone" end
            "#,
        );
        let reader = ws.def_file(
            "lua/autorun/sh_reader.lua",
            r#"
            local found = FindMetaTable("Example.Bone")
            local name = found.Name
            "#,
        );

        let class = ws.local_ty(bone, "BONE");
        assert_eq!(ws.humanize_type(class), "Example.Bone");
        let found = ws.local_ty(reader, "found");
        assert_eq!(ws.humanize_type(found), "Example.Bone");
        assert!(is_function_type(&ws.local_ty(reader, "name")));
    }

    /// What the factory puts on the table it returns, including the fields
    /// the registration adds, belongs to every class a call makes.
    #[test]
    fn a_factory_made_class_has_what_the_factory_put_on_its_table() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![METATABLE_FACTORY]);
        let bone = ws.def_file(
            "lua/autorun/sh_bone.lua",
            r#"
            local BONE = CreateMetaTable("Bone")
            function BONE:Name() return self:GetTable() end
            local name = BONE.MetaName
            local tab = BONE:Name()
            "#,
        );

        let name = ws.local_ty(bone, "name");
        assert_eq!(ws.humanize_type(name), "string");
        let tab = ws.local_ty(bone, "tab");
        assert_eq!(ws.humanize_type(tab), "table");
    }

    /// A function that returns what a factory call returns is a factory
    /// too, whether it returns the call or a local holding it.
    #[test]
    fn a_function_returning_a_factory_call_is_a_factory() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![(
            "lua/autorun/sh_meta.lua",
            r#"
            function NewClass(name)
                local meta = {}
                meta.__index = meta
                RegisterMetaTable(name, meta)
                return meta
            end

            function NewWidget(name)
                return NewClass("Widget." .. name)
            end

            function NewGadget(name)
                local gadget = NewClass("Gadget." .. name)
                gadget.Kind = name
                return gadget
            end
            "#,
        )]);
        let file_id = ws.def_file(
            "lua/autorun/sh_use.lua",
            r#"
            local WIDGET = NewWidget("Button")
            local GADGET = NewGadget("Lamp")
            "#,
        );

        let widget = ws.local_ty(file_id, "WIDGET");
        assert_eq!(ws.humanize_type(widget), "Widget.Button");
        let gadget = ws.local_ty(file_id, "GADGET");
        assert_eq!(ws.humanize_type(gadget), "Gadget.Lamp");
    }

    /// Factories are not particular to metatables: a function returning the
    /// panel table it registers makes each call a panel class.
    #[test]
    fn a_panel_factory_call_registers_the_panel_it_returns() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![(
            "lua/autorun/client/cl_panels.lua",
            r#"
            function MakePanel(name)
                local PANEL = {}
                vgui.Register(name, PANEL, "DPanel")
                return PANEL
            end
            "#,
        )]);
        let file_id = ws.def_file(
            "lua/autorun/client/cl_use.lua",
            r#"
            local PNL = MakePanel("MyPanel")
            "#,
        );

        let panel = ws.local_ty(file_id, "PNL");
        assert_eq!(ws.humanize_type(panel), "MyPanel");
    }

    /// Making the factory return something else or drop a method, and back, leaves what a
    /// fresh analysis of the final text has.
    #[test]
    fn editing_a_metatable_factory_matches_a_fresh_analysis() {
        let user = (
            "lua/autorun/sh_bone.lua",
            "local BONE = CreateMetaTable(\"Bone\")\nfunction BONE:Name() return self:GetTable() end",
        );
        let (head, tail) = METATABLE_FACTORY
            .1
            .rsplit_once("return meta")
            .expect("return");
        let not_returned = format!("{head}return proxy{tail}");
        let no_method = METATABLE_FACTORY
            .1
            .replace("function meta:GetTable() return {} end", "");
        let setup = |ws: &mut VirtualWorkspace| {
            ws.def_gmod_call_arg_builtins();
        };
        VirtualWorkspace::assert_warm_matches_cold(
            setup,
            &[METATABLE_FACTORY, user],
            &[
                (METATABLE_FACTORY.0, &not_returned),
                METATABLE_FACTORY,
                (METATABLE_FACTORY.0, &no_method),
                METATABLE_FACTORY,
            ],
        );
    }

    const STORED_OBJECT: (&str, &str) = (
        "lua/autorun/sh_stored.lua",
        r#"
        local Storage = setmetatable({}, { __mode = "k" })
        local META = {}
        RegisterMetaTable("Stored", META)

        function META:__index(key)
            local value = Storage[self][key]
            if value ~= nil then
                return value
            end
            return rawget(META, key)
        end

        function META:GetCount()
            local count = self.Count
            local label = self.Label
            local other = self.Unlisted
            return count
        end

        function NewStored(label)
            local object = setmetatable({}, META)
            Storage[object] = { Label = label, Count = 0 }
            return object
        end
        "#,
    );

    /// A key a class does not define reads what its `__index` function
    /// returns: with `__index` reading `Storage[self][key]`, the fields of
    /// the stored table are the object's fields.
    #[test]
    fn a_function_index_reads_fields_from_the_table_it_looks_in() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        let file_id = ws.def_file(STORED_OBJECT.0, STORED_OBJECT.1);

        let count = ws.local_ty(file_id, "count");
        assert_eq!(ws.humanize_type(count), "integer");
        let label = ws.local_ty(file_id, "label");
        assert!(!label.is_nil(), "{}", ws.humanize_type(label));
        let other = ws.local_ty(file_id, "other");
        assert!(!other.is_nil(), "{}", ws.humanize_type(other));
    }

    /// An `__index` function written on a class a doc declares, as code
    /// that speeds up `Entity` lookups does, leaves the class its declared
    /// shape: a key it does not declare is still undefined.
    #[test]
    fn an_index_function_on_a_declared_class_keeps_its_fields_checked() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!ws.check_file_for(
            DiagnosticCode::UndefinedField,
            "lua/autorun/sh_fast_index.lua",
            r#"
            ---@class Thing
            ---@field Name string
            local THING = {}

            function THING:__index(key)
                return rawget(THING, key)
            end

            ---@param thing Thing
            local function read(thing)
                return thing.Nmae
            end
            "#,
        ));
    }

    /// A class whose `__index` is a function can answer any key, so no
    /// key it does not list is undefined.
    #[test]
    fn a_function_index_leaves_no_field_undefined() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        assert!(ws.check_file_for(
            DiagnosticCode::UndefinedField,
            STORED_OBJECT.0,
            STORED_OBJECT.1,
        ));
    }

    const KIND_FACTORY: (&str, &str) = (
        "lua/autorun/sh_kinds.lua",
        r#"
        Kinds = Kinds or { Root = {} }
        Make = Make or {}

        function Kinds.Root:Describe() return "kind" end

        local function lookup(meta, key)
            while meta do
                local value = rawget(meta, key)
                if value ~= nil then
                    return value
                end
                meta = rawget(meta, "Parent")
            end
        end

        function DefineKind(name, parent)
            local meta = {}
            RegisterMetaTable("Kind." .. name, meta)
            Kinds[name] = meta
            meta.Parent = parent and Kinds[parent] or Kinds.Root

            function meta:__index(key)
                return lookup(meta, key)
            end

            Make[name] = function(...)
                local object = setmetatable({}, meta)
                return object
            end
            return meta
        end
        "#,
    );

    const KIND_USES: (&str, &str) = (
        "lua/autorun/sh_animals.lua",
        r#"
        local ANIMAL = DefineKind("Animal")
        function ANIMAL:Speak() return "..." end
        local DOG = DefineKind("Dog", "Animal")
        "#,
    );

    /// Writes a factory makes under a key built from its parameters are
    /// named per call: the registry entry is the call's class, and a
    /// constructor returning the factory's instances returns that class.
    #[test]
    fn a_factory_call_names_what_the_factory_writes_under_its_arguments() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![KIND_FACTORY, KIND_USES]);
        let reader = ws.def_file(
            "lua/autorun/sh_reader.lua",
            r#"
            local registered = Kinds.Dog
            local dog = Make.Dog()
            "#,
        );

        let registered = ws.local_ty(reader, "registered");
        assert_eq!(ws.humanize_type(registered), "Kind.Dog");
        let dog = ws.local_ty(reader, "dog");
        assert_eq!(ws.humanize_type(dog), "Kind.Dog");
    }

    /// An `__index` walking `rawget(meta, "Parent")` inherits through
    /// `Parent`: what the factory stores there for a call is the class's
    /// parent.
    #[test]
    fn an_index_walk_through_a_parent_field_is_inheritance() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![KIND_FACTORY, KIND_USES]);
        let reader = ws.def_file(
            "lua/autorun/sh_reader.lua",
            r#"
            local speak = Kinds.Dog.Speak
            local describe = Kinds.Animal.Describe
            "#,
        );

        assert!(is_function_type(&ws.local_ty(reader, "speak")));
        assert!(is_function_type(&ws.local_ty(reader, "describe")));
    }

    /// Renaming a factory call's arguments, and back, leaves what a fresh
    /// analysis of the final text has.
    #[test]
    fn editing_a_factory_call_argument_matches_a_fresh_analysis() {
        let reader = (
            "lua/autorun/sh_reader.lua",
            "local dog = Make.Dog()\nlocal speak = Kinds.Dog.Speak",
        );
        let renamed = KIND_USES
            .1
            .replace("\"Dog\", \"Animal\"", "\"Dog\", \"Plant\"");
        assert_ne!(renamed, KIND_USES.1);
        let setup = |ws: &mut VirtualWorkspace| {
            ws.def_gmod_call_arg_builtins();
        };
        VirtualWorkspace::assert_warm_matches_cold(
            setup,
            &[KIND_FACTORY, KIND_USES, reader],
            &[(KIND_USES.0, &renamed), KIND_USES],
        );
    }

    /// A factory in a `module` file writing a constructor into the module
    /// table under the call's name gives each call a constructor of its class.
    #[test]
    fn a_module_factory_names_a_constructor_per_call() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![
            (
                "lua/kinds/sh_meta.lua",
                r#"
            module("kinds", package.seeall)
            Registry = Registry or {}
            function Define(name)
                local metaName = "K." .. name
                local proxy = newproxy(true)
                local meta = getmetatable(proxy)
                RegisterMetaTable(metaName, meta)
                Registry[name] = meta
                kinds[name] = function(owner, ...)
                    local instance = newproxy(proxy)
                    return instance
                end
                return meta
            end
            "#,
            ),
            (
                "lua/kinds/sh_bone.lua",
                r#"
            module("kinds", package.seeall)
            local BONE = Define("Bone")
            function BONE:Grow() end
            "#,
            ),
        ]);
        let reader = ws.def_file(
            "lua/autorun/sh_reader.lua",
            r#"
            local made = kinds.Bone(1)
            local registered = kinds.Registry.Bone
            "#,
        );
        let made = ws.local_ty(reader, "made");
        assert_eq!(ws.humanize_type(made), "K.Bone");
        let registered = ws.local_ty(reader, "registered");
        assert_eq!(ws.humanize_type(registered), "K.Bone");
    }

    /// A method that makes an instance of `self` and turns it into a class
    /// of its own (`sub.__index = sub`) settles: the instance is `self`'s
    /// class, and the write to its `__index` does not feed back into it.
    #[test]
    fn a_subclass_made_from_self_settles() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_extend.lua",
            r#"
            local Base = {}
            Base.__index = Base
            function Base:extend()
                local sub = setmetatable({}, self)
                sub.__index = sub
                return sub
            end
            function Base:Hello() return 1 end
            local Sub = Base:extend()
            local hello = Sub.Hello
            "#,
        );

        assert!(is_function_type(&ws.local_ty(file_id, "hello")));
    }

    /// A method defined on a subclass belongs to the subclass: an instance of
    /// the parent does not have it, while an instance of the subclass still
    /// inherits the parent's methods.
    #[test]
    fn a_subclass_method_stays_off_the_parent() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_animals.lua",
            r#"
            local Animal = {}
            Animal.__index = Animal
            function Animal:Speak() return "..." end
            local Dog = setmetatable({}, Animal)
            Dog.__index = Dog
            function Dog:Fetch() return true end
            local cat = setmetatable({}, Animal)
            local catFetch = cat.Fetch
            local dog = setmetatable({}, Dog)
            local dogSpeak = dog.Speak
            local dogFetch = dog.Fetch
            "#,
        );

        assert!(!is_function_type(&ws.local_ty(file_id, "catFetch")));
        assert!(is_function_type(&ws.local_ty(file_id, "dogSpeak")));
        assert!(is_function_type(&ws.local_ty(file_id, "dogFetch")));
    }

    /// The same holds for a subclass held in a global.
    #[test]
    fn a_global_subclass_method_stays_off_the_parent() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_animals.lua",
            r#"
            Animal = {}
            Animal.__index = Animal
            function Animal:Speak() return "..." end
            Dog = setmetatable({}, Animal)
            Dog.__index = Dog
            function Dog:Fetch() return true end
            local cat = setmetatable({}, Animal)
            local catFetch = cat.Fetch
            local dog = setmetatable({}, Dog)
            local dogSpeak = dog.Speak
            local dogFetch = dog.Fetch
            "#,
        );

        assert!(!is_function_type(&ws.local_ty(file_id, "catFetch")));
        assert!(is_function_type(&ws.local_ty(file_id, "dogSpeak")));
        assert!(is_function_type(&ws.local_ty(file_id, "dogFetch")));
    }

    const VECTOR_CLASS: &str = r#"
            local V = {}
            V.__index = V
            local function new(x) return setmetatable({ x = x }, V) end
            V.__add = function(a, b) return new(a.x + b.x) end
            function V.__unm(a) return new(-a.x) end
            V.__concat = function(a, b) return new(0) end
            function V:Length() return 1 end
            local v = new(1)
    "#;

    /// An operator on values whose metatable defines its metamethod returns
    /// what the metamethod returns, however the metamethod was written.
    #[test]
    fn an_operator_returns_what_its_metamethod_returns() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_vector.lua",
            &format!(
                "{VECTOR_CLASS}
            local sum = v + v
            local neg = -v
            local joined = v .. v
            local sumLength = sum.Length
            local negLength = neg.Length
            local joinedLength = joined.Length
            "
            ),
        );

        assert!(is_function_type(&ws.local_ty(file_id, "sumLength")));
        assert!(is_function_type(&ws.local_ty(file_id, "negLength")));
        assert!(is_function_type(&ws.local_ty(file_id, "joinedLength")));
    }

    /// A metamethod on a registered class types the operator on its
    /// instances.
    #[test]
    fn an_operator_on_a_registered_class_returns_what_its_metamethod_returns() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        let file_id = ws.def_file(
            "lua/autorun/sh_money.lua",
            r#"
            local META = {}
            META.__index = META
            function META.__add(a, b) return a end
            RegisterMetaTable("Money", META)
            local money = setmetatable({}, FindMetaTable("Money"))
            local total = money + money
            "#,
        );

        assert_eq!(ws.humanize_type(ws.local_ty(file_id, "total")), "Money");
    }

    /// Without a metamethod the operator keeps its plain Lua result.
    #[test]
    fn an_operator_without_a_metamethod_keeps_its_plain_result() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_plain.lua",
            r#"
            local P = {}
            P.__index = P
            local p = setmetatable({}, P)
            local joined = p .. "x"
            "#,
        );

        assert_eq!(ws.humanize_type(ws.local_ty(file_id, "joined")), "string");
    }

    /// A table made callable by `__call` returns what `__call` returns, with
    /// the table itself passed first.
    #[test]
    fn calling_a_table_runs_its_call_metamethod_on_it() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_point.lua",
            r#"
            local Point = {}
            Point.__index = Point
            function Point:Length() return 1 end
            function Point.new(x)
                local o = setmetatable({}, Point)
                o.x = x
                return o
            end
            setmetatable(Point, { __call = function(cls, x) return cls.new(x) end })
            local Shape = setmetatable({}, { __call = function(cls)
                return setmetatable({}, cls)
            end })
            Shape.__index = Shape
            function Shape:Area() return 2 end
            local point = Point(1)
            local length = point.Length
            local shape = Shape()
            local area = shape.Area
            "#,
        );

        assert!(is_function_type(&ws.local_ty(file_id, "length")));
        assert!(is_function_type(&ws.local_ty(file_id, "area")));
    }

    const ACCOUNT_CLASS: &str = r#"
            Account = { balance = 0 }
            function Account:new(o)
                o = o or {}
                setmetatable(o, self)
                self.__index = self
                return o
            end
            function Account:deposit(v) self.balance = self.balance + v end
            SpecialAccount = Account:new()
            function SpecialAccount:getLimit() return self.limit or 0 end
    "#;

    /// `Class:new(o)` makes an instance of whatever it is called on, so a
    /// class made by calling it on another is a subclass with its own methods.
    #[test]
    fn a_new_method_makes_an_instance_of_its_receiver() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_account.lua",
            &format!(
                "{ACCOUNT_CLASS}
            local account = Account:new({{ balance = 10 }})
            local special = SpecialAccount:new({{ limit = 100 }})
            local accountDeposit = account.deposit
            local accountLimit = account.getLimit
            local specialDeposit = special.deposit
            local specialLimit = special.getLimit
            "
            ),
        );

        assert!(is_function_type(&ws.local_ty(file_id, "accountDeposit")));
        assert!(!is_function_type(&ws.local_ty(file_id, "accountLimit")));
        assert!(is_function_type(&ws.local_ty(file_id, "specialDeposit")));
        assert!(is_function_type(&ws.local_ty(file_id, "specialLimit")));
    }

    /// A method returning `setmetatable({}, self)` extends whatever it is
    /// called on: a class extended from a subclass inherits both.
    #[test]
    fn an_extend_method_extends_its_receiver() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_extend.lua",
            r#"
            local Base = {}
            Base.__index = Base
            function Base:extend()
                local sub = setmetatable({}, self)
                sub.__index = sub
                return sub
            end
            function Base:hello() return 1 end
            local A = Base:extend()
            function A:onlyA() return 2 end
            local B = A:extend()
            local b = setmetatable({}, B)
            local bHello = b.hello
            local bOnlyA = b.onlyA
            local base = setmetatable({}, Base)
            local baseOnlyA = base.onlyA
            "#,
        );

        assert!(is_function_type(&ws.local_ty(file_id, "bHello")));
        assert!(is_function_type(&ws.local_ty(file_id, "bOnlyA")));
        assert!(!is_function_type(&ws.local_ty(file_id, "baseOnlyA")));
    }

    /// `table.Inherit` gives its target the base's members and `BaseClass`.
    #[test]
    fn table_inherit_gives_the_target_the_base_members() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        let file_id = ws.def_file(
            "lua/autorun/sh_inherit.lua",
            r#"
            local Base = { hp = 100 }
            function Base:Heal() return self.hp end
            local Child = table.Inherit({ armor = 5 }, Base)
            local heal = Child.Heal
            local armor = Child.armor
            local base = Child.BaseClass
            local baseHeal = base.Heal
            "#,
        );

        assert!(is_function_type(&ws.local_ty(file_id, "heal")));
        assert_eq!(ws.humanize_type(ws.local_ty(file_id, "armor")), "5");
        assert!(is_function_type(&ws.local_ty(file_id, "baseHeal")));
    }

    /// An `__index` function reading the key with `rawget(P, key)`, also as
    /// one side of an `or`, gives `P`'s field.
    #[test]
    fn an_index_function_reading_with_rawget_gives_the_field() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_rawget.lua",
            r#"
            local MT = {}
            MT.__index = function(self, key)
                return rawget(MT, key) or rawget(self, "_" .. key)
            end
            function MT:Ping() return "pong" end
            local object = setmetatable({}, MT)
            local ping = object.Ping
            "#,
        );

        assert!(is_function_type(&ws.local_ty(file_id, "ping")));
    }

    /// An `__index` function returning a lookup `or` a default gives the
    /// field when it is there and the default when it is not.
    #[test]
    fn an_index_function_with_a_default_gives_the_field_or_the_default() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_defaults.lua",
            r#"
            ---@type { label: string? }
            local Stored = {}
            local MT = {}
            MT.__index = function(self, key)
                return Stored[key] or 5
            end
            local object = setmetatable({}, MT)
            local label = object.label
            "#,
        );

        assert_eq!(
            ws.humanize_type(ws.local_ty(file_id, "label")),
            "(integer|string)"
        );
    }

    /// An `__index` function that returns a value of its own for some keys
    /// gives that value too, not only the field it looks up.
    #[test]
    fn an_index_function_returning_its_own_value_for_some_keys_gives_it_too() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_special_key.lua",
            r#"
            ---@type { label: string }
            local Stored = {}
            local MT = {}
            MT.__index = function(self, key)
                if key == "id" then
                    return false
                end
                return Stored[key]
            end
            local object = setmetatable({}, MT)
            local label = object.label
            "#,
        );

        assert_eq!(
            ws.humanize_type(ws.local_ty(file_id, "label")),
            "(boolean|string)"
        );
    }

    /// A field a table may not hold reads as its own value when it is there
    /// and the `__index` table's when it is not: either one, not only what
    /// both could be.
    #[test]
    fn a_field_the_table_may_lack_reads_its_value_or_the_index_value() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_read_through.lua",
            r#"
            ---@return number?
            local function maybeSize() end
            local sized = { size = maybeSize() }
            setmetatable(sized, { __index = { size = 1 } })
            local size = sized.size
            "#,
        );

        assert_eq!(ws.humanize_type(ws.local_ty(file_id, "size")), "number");
    }

    /// A key computed at runtime does not name a field the table lacks: a
    /// table holding values of its own reads as them, not as every member its
    /// class holds too, while one holding nothing reads its class's.
    #[test]
    fn a_runtime_key_reads_the_class_only_when_the_table_holds_nothing() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_runtime_key.lua",
            r#"
            local Stream = {}
            Stream.__index = Stream
            function Stream:Destroy() end
            ---@return string
            local function anyKey() end
            local filled = setmetatable({ id = 1, label = "x" }, Stream)
            local fromFilled = filled[anyKey()]
            local empty = setmetatable({}, Stream)
            local fromEmpty = empty[anyKey()]
            "#,
        );

        let holds_method = |name: &str| match ws.local_ty(file_id, name) {
            LuaType::Union(union) => union.types().any(is_function_type),
            typ => is_function_type(&typ),
        };
        assert_eq!(
            (holds_method("fromFilled"), holds_method("fromEmpty")),
            (false, true)
        );
    }

    /// A field a table may not hold, read through an `__index` table whose
    /// field may be missing too, keeps what both may hold.
    #[test]
    fn a_field_both_tables_may_lack_reads_either_value_or_nil() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_read_through_nil.lua",
            r#"
            ---@return string?
            local function maybeLabel() end
            ---@return integer?
            local function maybeId() end
            local labelled = setmetatable({ label = maybeLabel() }, { __index = { label = maybeId() } })
            local label = labelled.label
            "#,
        );

        assert_eq!(
            ws.humanize_type(ws.local_ty(file_id, "label")),
            "(integer|string)?"
        );
    }

    /// `getmetatable` gives a metatable's `__metatable` field when it has one.
    #[test]
    fn getmetatable_gives_the_metatable_field_that_hides_it() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_locked.lua",
            r#"
            local Locked = setmetatable({}, { __metatable = "locked", __index = { a = 1 } })
            local Open = setmetatable({}, { __index = { a = 1 } })
            local locked = getmetatable(Locked)
            local open = getmetatable(Open).__index
            "#,
        );

        assert_eq!(ws.humanize_type(ws.local_ty(file_id, "locked")), "string");
        assert!(ws.humanize_type(ws.local_ty(file_id, "open")).contains("a"));
    }

    /// A function stored on a local table takes its parameter types from the
    /// calls through that table, as a local function does, so the fields a
    /// constructor copies from them have those types.
    #[test]
    fn a_constructor_field_takes_the_type_its_callers_pass() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_point.lua",
            r#"
            local Point = {}
            Point.__index = Point
            function Point.new(x, label)
                local o = setmetatable({}, Point)
                o.x = x
                o.label = label
                return o
            end
            function Point:Scale(by) return by end
            local p = Point.new(3, "a")
            local q = Point.new(4.5, "b")
            local px = p.x
            local plabel = p.label
            local scaled = Point:Scale(2)
            "#,
        );

        assert_eq!(ws.humanize_type(ws.local_ty(file_id, "px")), "(3|4.5)");
        assert_eq!(
            ws.humanize_type(ws.local_ty(file_id, "plabel")),
            r#"("a"|"b")"#
        );
        assert_eq!(ws.humanize_type(ws.local_ty(file_id, "scaled")), "2");
    }

    /// A `class(base)` helper makes a new table on each call, with `base` as
    /// its parent: each class has its own methods and inherits its parent's,
    /// also for a class made by a method calling the helper on `self`.
    #[test]
    fn a_class_helper_makes_each_class_inherit_from_its_base() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_class.lua",
            r#"
            local function class(base)
                local c = {}
                c.__index = c
                setmetatable(c, { __index = base, __call = function(cls, ...) return cls.new(...) end })
                function c.new(...)
                    local o = setmetatable({}, c)
                    if o.init then o:init(...) end
                    return o
                end
                function c:extend() return class(self) end
                return c
            end
            local Animal = class()
            function Animal:Speak() return "..." end
            local Dog = class(Animal)
            function Dog:Fetch() return 1 end
            local Puppy = Dog:extend()
            function Puppy:Nap() return true end
            local dog = Dog.new()
            local animal = Animal.new()
            local puppy = Puppy()
            local dogSpeak = dog.Speak
            local dogFetch = dog.Fetch
            local dogNap = dog.Nap
            local animalFetch = animal.Fetch
            local puppySpeak = puppy.Speak
            local puppyFetch = puppy.Fetch
            local puppyNap = puppy.Nap
            "#,
        );

        for name in [
            "dogSpeak",
            "dogFetch",
            "puppySpeak",
            "puppyFetch",
            "puppyNap",
        ] {
            assert!(is_function_type(&ws.local_ty(file_id, name)), "{name}");
        }
        for name in ["dogNap", "animalFetch"] {
            assert!(!is_function_type(&ws.local_ty(file_id, name)), "{name}");
        }
    }

    /// A class library whose `extend` copies a class into a new table with
    /// the class as its metatable, and whose `__call` method makes instances:
    /// each class has its own methods, and `super` names its own parent.
    #[test]
    fn a_class_library_extending_through_a_metatable_keeps_each_class() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_classic.lua",
            r#"
            local Object = {}
            Object.__index = Object
            function Object:new() end
            function Object:extend()
                local cls = {}
                for k, v in pairs(self) do
                    if k:find("__") == 1 then
                        cls[k] = v
                    end
                end
                cls.__index = cls
                cls.super = self
                setmetatable(cls, self)
                return cls
            end
            function Object:__call(...)
                local obj = setmetatable({}, self)
                obj:new(...)
                return obj
            end
            local Point = Object:extend()
            function Point:Len() return 1 end
            local Rect = Point:extend()
            function Rect:Area() return 2 end
            local p = Point()
            local r = Rect()
            local pointLen = p.Len
            local pointArea = p.Area
            local rectLen = r.Len
            local rectArea = r.Area
            local rectSuperLen = Rect.super.Len
            local pointSuperLen = Point.super.Len
            "#,
        );

        for name in ["pointLen", "rectLen", "rectArea", "rectSuperLen"] {
            assert!(is_function_type(&ws.local_ty(file_id, name)), "{name}");
        }
        for name in ["pointArea", "pointSuperLen"] {
            assert!(!is_function_type(&ws.local_ty(file_id, name)), "{name}");
        }
    }

    /// A factory whose metatable's `__call` makes instances reading through
    /// the class it made, with a parent per call: an instance reads its
    /// class's members and its parent's, including accessors a helper
    /// installs on the parent, past the ones it may install on the class.
    #[test]
    fn a_call_metamethod_closing_over_its_class_makes_instances_of_it() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        let file_id = ws.def_file(
            "lua/autorun/sh_oop.lua",
            r#"
            local function Class(name, parent)
                local t = {}
                local meta = {}
                meta.__call = function(_, ...)
                    local inst = setmetatable({}, {__index = t})
                    if inst.initialize then inst:initialize(...) end
                    return inst
                end
                if parent then meta.__index = parent end
                setmetatable(t, meta)
                return t
            end
            local function AccessorFunc(panel, fieldname, name)
                panel["Get" .. name] = function(self) return self[fieldname] end
                panel["Set" .. name] = function(self, value) self[fieldname] = value end
            end
            local GridDim = Class("GridDim")
            AccessorFunc(GridDim, "_size", "Size")
            function GridDim:Reset() end
            local Row = Class("Row", GridDim)
            AccessorFunc(Row, "_marginTop", "MarginTop")
            function Row:Width() return 1 end
            local Cell = Class("Cell")
            local PANEL = {}
            function PANEL:Init() self._rows = {} end
            function PANEL:Row()
                local row = Row()
                self._rows[#self._rows + 1] = row
                return row
            end
            function PANEL:RowCount() return #self._rows end
            function PANEL:Layout()
                for r = 1, self:RowCount() do
                    local row = self._rows[r]
                    local rowSetSize = row.SetSize
                end
            end
            local row = Row()
            local cell = Cell()
            local rowReset = row.Reset
            local rowWidth = row.Width
            local cellReset = cell.Reset
            "#,
        );

        for name in ["rowSetSize", "rowReset", "rowWidth"] {
            assert!(is_function_type(&ws.local_ty(file_id, name)), "{name}");
        }
        assert!(!is_function_type(&ws.local_ty(file_id, "cellReset")));
    }

    /// A field a factory sets from its parameter holds what the call making
    /// the table passed.
    #[test]
    fn a_factory_field_holds_what_its_call_passed() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_named.lua",
            r#"
            local function class(name)
                local cls = { name = name }
                cls.__index = cls
                return cls
            end
            local Shape = class("Shape")
            local Square = class("Square")
            local shapeName = Shape.name
            local squareName = Square.name
            "#,
        );

        assert_eq!(
            ws.humanize_type(ws.local_ty(file_id, "shapeName")),
            r#""Shape""#
        );
        assert_eq!(
            ws.humanize_type(ws.local_ty(file_id, "squareName")),
            r#""Square""#
        );
    }

    /// A factory that gives its parameter a default before storing it stores
    /// the default when its call passes nothing, not the `nil` it was passed.
    #[test]
    fn a_factory_field_from_a_reassigned_parameter_holds_the_new_value() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "lua/autorun/sh_named.lua",
            r#"
            local function class(name)
                name = name or "Object"
                local cls = { name = name }
                cls.__index = cls
                return cls
            end
            local Base = class()
            local baseName = Base.name
            "#,
        );

        assert_eq!(ws.humanize_type(ws.local_ty(file_id, "baseName")), "string");
    }

    fn is_function_type(typ: &LuaType) -> bool {
        matches!(typ, LuaType::Signature(_) | LuaType::DocFunction(_))
    }
}
