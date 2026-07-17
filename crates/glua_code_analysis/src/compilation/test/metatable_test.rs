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
    fn test_colon_receiver_contribution_requires_leading_self_name() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
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
            "#,
        ));
    }

    #[test]
    fn test_colon_receiver_contribution_rejects_nonleading_self() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
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
            "#,
        ));
    }

    #[test]
    fn test_colon_receiver_contribution_rejects_mutated_self() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
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
            "#,
        ));
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
    fn test_callable_table_nonreceiver_parameter_is_not_used_as_instance_metatable() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local Setting = {
                __index = {
                    known = function(self) end,
                },
                __call = function(class, metatable)
                    local self = setmetatable({}, metatable)
                    self:known()
                end,
            }

            setmetatable(Setting, Setting)
            "#,
        ));
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
    fn test_arbitrary_factory_parameter_is_not_treated_as_metatable_owner() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            local Class = {}
            Class.__index = Class
            function Class:known() end

            local function make(metatable)
                return setmetatable({}, metatable)
            end

            make(Class):known()
            "#,
        ));
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
    fn test_literal_index_later_function_overwrite_rejects_factory_recovery() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
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
            local Class = {}
            function Class:known() end
            local Meta = { __index = Class }
            Meta.__index = function(_, key)
                return rawget(Class, key)
            end
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
            .expect("literal metatable lifecycle file must be created");
        assert!(has_undefined_method(&ws, initial_file_id));

        let edited_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(supported_content.to_string()))
            .expect("literal metatable lifecycle file must be updated");
        assert!(!has_undefined_method(&ws, edited_file_id));

        ws.analysis
            .remove_file_by_uri(&uri)
            .expect("literal metatable lifecycle file must be removed");
        let reopened_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(unsupported_content.to_string()))
            .expect("literal metatable lifecycle file must reopen");
        assert!(has_undefined_method(&ws, reopened_file_id));

        let restored_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(supported_content.to_string()))
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
            .expect("metatable lifecycle file must be created");
        assert!(!has_undefined_method(&ws, initial_file_id));

        let edited_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(other_index_content.to_string()))
            .expect("metatable lifecycle file must be updated");
        assert!(has_undefined_method(&ws, edited_file_id));

        ws.analysis
            .remove_file_by_uri(&uri)
            .expect("metatable lifecycle file must be removed");
        let reopened_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(other_index_content.to_string()))
            .expect("metatable lifecycle file must reopen");
        assert!(has_undefined_method(&ws, reopened_file_id));

        let restored_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(self_index_content.to_string()))
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

            Class:new():known()
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
}
