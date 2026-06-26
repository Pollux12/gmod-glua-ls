#[cfg(test)]
mod test {
    use glua_parser::{
        LuaAstNode, LuaExpr, LuaFuncStat, LuaIndexKey, LuaLocalFuncStat, LuaVarExpr,
    };
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{
        DiagnosticCode, InFiled, LuaMemberKey, LuaMemberOwner, LuaSignatureId, LuaType,
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
}
