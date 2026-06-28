#[cfg(test)]
mod test {
    use std::{ops::Deref, sync::Arc};

    use crate::{
        DiagnosticCode, Emmyrc, LuaMemberOwner, LuaType, RenderLevel, VirtualWorkspace,
        humanize_type,
    };
    use glua_parser::{LuaAstNode, LuaAstToken, LuaExpr, LuaIndexExpr, LuaLocalName};
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    fn assign_value_type(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        lhs_text: &str,
    ) -> LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");

        let assign_stat = semantic_model
            .get_root()
            .descendants::<glua_parser::LuaAssignStat>()
            .find(|assign_stat| {
                assign_stat
                    .get_var_and_expr_list()
                    .0
                    .into_iter()
                    .any(|var| var.syntax().text() == lhs_text)
            })
            .expect("expected assignment stat");
        let (vars, exprs) = assign_stat.get_var_and_expr_list();
        let index = vars
            .iter()
            .position(|var| var.syntax().text() == lhs_text)
            .expect("expected matching LHS variable");
        let expr = exprs
            .get(index)
            .cloned()
            .expect("expected corresponding assignment value");

        semantic_model
            .infer_expr(expr)
            .expect("expected inferred assignment value type")
    }

    fn local_name_type(ws: &mut VirtualWorkspace, file_id: crate::FileId, name: &str) -> LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");

        let local_name = semantic_model
            .get_root()
            .descendants::<LuaLocalName>()
            .find(|local_name| {
                local_name
                    .get_name_token()
                    .is_some_and(|token| token.get_name_text() == name)
            })
            .expect("expected local name");
        let token = local_name
            .get_name_token()
            .expect("expected local name token");

        semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .map(|info| info.typ)
            .expect("expected semantic info for local name")
    }

    fn index_expr_type(
        analysis: &crate::EmmyLuaAnalysis,
        file_id: crate::FileId,
        expr_text: &str,
    ) -> LuaType {
        let semantic_model = analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");

        let index_expr = semantic_model
            .get_root()
            .descendants::<LuaIndexExpr>()
            .find(|index_expr| index_expr.syntax().text() == expr_text)
            .expect("expected index expression");

        semantic_model
            .infer_expr(LuaExpr::IndexExpr(index_expr))
            .expect("expected inferred index expression type")
    }

    fn index_expr_type_displays(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
        expr_text: &str,
    ) -> Vec<String> {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");

        semantic_model
            .get_root()
            .descendants::<LuaIndexExpr>()
            .filter(|index_expr| index_expr.syntax().text() == expr_text)
            .map(|index_expr| {
                let typ = semantic_model
                    .infer_expr(LuaExpr::IndexExpr(index_expr))
                    .expect("expected inferred index expression type");
                ws.humanize_type(typ)
            })
            .collect()
    }

    fn diagnostics_for_code(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        diagnostic_code: DiagnosticCode,
    ) -> Vec<lsp_types::Diagnostic> {
        ws.analysis.diagnostic.enable_only(diagnostic_code);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            diagnostic_code.get_name().to_string(),
        ));
        diagnostics
            .into_iter()
            .filter(|diagnostic| diagnostic.code == code)
            .collect()
    }

    fn contains_empty_table_bootstrap(db: &crate::DbIndex, typ: &LuaType) -> bool {
        match typ {
            LuaType::Table => true,
            LuaType::TableConst(table_id) => {
                db.get_member_index()
                    .get_member_len(&LuaMemberOwner::Element(table_id.clone()))
                    == 0
            }
            LuaType::Union(union) => union
                .into_vec()
                .iter()
                .any(|sub_type| contains_empty_table_bootstrap(db, sub_type)),
            _ => false,
        }
    }

    fn empty_table_bootstrap_branch_count(db: &crate::DbIndex, typ: &LuaType) -> usize {
        match typ {
            LuaType::Table => 1,
            LuaType::TableConst(table_id) => usize::from(
                db.get_member_index()
                    .get_member_len(&LuaMemberOwner::Element(table_id.clone()))
                    == 0,
            ),
            LuaType::Union(union) => union
                .into_vec()
                .iter()
                .map(|sub_type| empty_table_bootstrap_branch_count(db, sub_type))
                .sum(),
            _ => 0,
        }
    }

    #[test]
    fn test_dynamic_table_param_return_or_empty_branch_keeps_fields_allowed() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@return table|nil
            function JSONToTable(s) end

            function CheckVersion(data)
                if type(data.version) ~= "number" then
                    return {}
                end

                return data
            end

            local data = JSONToTable("{}") or {}
            data = CheckVersion(data)
            print(data.carVolume)
        "#
        ));
    }

    #[test]
    fn metatable_overload_parameter_type_is_visible_inside_function_body_guards() {
        let mut ws = VirtualWorkspace::new();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        ws.def_file(
            "annotations/global.lua",
            r#"
            ---@meta

            ---@param value any
            ---@return TypeGuard<table>
            function _G.istable(value) end

            ---@generic T : table
            ---@param metaName `T`
            ---@return (definition) T
            function _G.FindMetaTable(metaName) end
            "#,
        );
        ws.def_file(
            "annotations/color.lua",
            r#"
            ---@meta

            ---@class Color
            ---@field r number The red component of the color.
            ---@field g number The green component of the color.
            ---@field b number The blue component of the color.
            ---@field a number The alpha component of the color.
            local Color = {}
            "#,
        );
        ws.def_file(
            "annotations/panel.lua",
            r#"
            ---@meta

            ---@class Panel
            Panel = Panel or {}

            ---@overload fun(color: Color)
            ---@param r number
            ---@param g number
            ---@param b number
            ---@param a number
            function Panel:SetFGColor(r, g, b, a) end

            ---@param r number
            ---@param g number
            ---@param b number
            ---@param a number
            function Panel:SetFGColorEx(r, g, b, a) end

            ---@overload fun(color: Color)
            ---@param r number
            ---@param g number
            ---@param b number
            ---@param a number
            function Panel:SetBGColor(r, g, b, a) end

            ---@param r number
            ---@param g number
            ---@param b number
            ---@param a number
            function Panel:SetBGColorEx(r, g, b, a) end
            "#,
        );

        let file_id = ws.def_file(
            "lua/includes/extensions/client/panel.lua",
            r#"
            local meta = FindMetaTable("Panel")

            meta.SetFGColorEx = meta.SetFGColor
            meta.SetBGColorEx = meta.SetBGColor

            function meta:SetFGColor(r, g, b, a)
                if istable(r) then
                    return self:SetFGColorEx(r.r, r.g, r.b, r.a)
                end
            end

            function meta:SetBGColor(r, g, b, a)
                if istable(r) then
                    return self:SetBGColorEx(r.r, r.g, r.b, r.a)
                end
            end
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert!(
            diagnostics.is_empty(),
            "unexpected UndefinedField diagnostics: {diagnostics:#?}"
        );
    }

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
    fn test_setmetatable_factory_fields_visible_to_class_methods() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local DermaAnimation = {}
                DermaAnimation.__index = DermaAnimation

                function DermaAnimation:Run()
                    self.Func(self.Panel, self)
                end

                function Derma_Anim(panel, func)
                    local anim = {}
                    anim.Panel = panel
                    anim.Func = func
                    return setmetatable(anim, DermaAnimation)
                end
            "#
        ));
    }

    #[test]
    fn test_setmetatable_factory_fields_do_not_hide_method_typos() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local DermaAnimation = {}
                DermaAnimation.__index = DermaAnimation

                function DermaAnimation:Run()
                    self.Fnc(self.Panel, self)
                end

                function Derma_Anim(panel, func)
                    local anim = {}
                    anim.Panel = panel
                    anim.Func = func
                    return setmetatable(anim, DermaAnimation)
                end
            "#
        ));
    }

    #[test]
    fn test_setmetatable_factory_fields_do_not_leak_to_other_classes() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local Animation = {}
                Animation.__index = Animation

                local Other = {}
                Other.__index = Other

                function Other:Run()
                    self.Func()
                end

                function MakeAnimation(func)
                    local anim = {}
                    anim.Func = func
                    return setmetatable(anim, Animation)
                end
            "#
        ));
    }

    #[test]
    fn test_setmetatable_factory_fields_ignore_alias_writes() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local Animation = {}
                Animation.__index = Animation

                function Animation:Run()
                    self.Func()
                end

                function MakeAnimation(func)
                    local anim = {}
                    local alias = anim
                    alias.Func = func
                    return setmetatable(anim, Animation)
                end
            "#
        ));
    }

    #[test]
    fn test_setmetatable_factory_fields_ignore_nested_closure_writes() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local Animation = {}
                Animation.__index = Animation

                function Animation:Run()
                    self.Func()
                end

                function MakeAnimation(func)
                    local anim = {}
                    local function assign()
                        anim.Func = func
                    end
                    assign()
                    return setmetatable(anim, Animation)
                end
            "#
        ));
    }

    #[test]
    fn test_setmetatable_factory_fields_ignore_reassigned_factory_local() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local Animation = {}
                Animation.__index = Animation

                function Animation:Run()
                    self.Func()
                end

                function MakeAnimation(func)
                    local anim = {}
                    anim.Func = func
                    anim = {}
                    return setmetatable(anim, Animation)
                end
            "#
        ));
    }

    #[test]
    fn test_setmetatable_factory_fields_require_self_referential_index() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local Animation = {}
                local Other = {}

                function Animation:Run()
                    self.Func()
                end

                function MakeAnimation(func)
                    local anim = {}
                    anim.Func = func
                    return setmetatable(anim, { __index = Other })
                end
            "#
        ));
    }

    #[test]
    fn test_setmetatable_factory_fields_require_named_self_referential_index() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local Animation = {}
                local Other = {}
                Animation.__index = Other

                function Animation:Run()
                    self.Func()
                end

                function MakeAnimation(func)
                    local anim = {}
                    anim.Func = func
                    return setmetatable(anim, Animation)
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
        ws.def_gmod_call_arg_builtins();
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
    fn test_gamemode_bootstrap_global_table_members_merge_across_files() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        let sh_util = r#"
                --- Various useful helper functions.
                -- @module ix.util

                ix.type = ix.type or {
                    [2] = "string",
                    string = 2,
                }

                ix.blurRenderQueue = {}

                function ix.util.Include(fileName, realm)
                end

                ix.util.Include("core/meta/sh_entity.lua")
            "#;
        let init = r#"
                ix = ix or { util = {}, meta = {} }
                include("core/sh_util.lua")
                include("shared.lua")
            "#;
        let cl_init = r#"
                ix = ix or { util = {}, gui = {}, meta = {} }
                include("core/sh_util.lua")
                include("shared.lua")
            "#;
        let shared_source = r#"
                ix.util.Include("core/cl_skin.lua")
            "#;

        ws.def_files(vec![
            ("gamemodes/helix/gamemode/core/sh_util.lua", sh_util),
            ("gamemodes/helix/gamemode/init.lua", init),
            ("gamemodes/helix/gamemode/cl_init.lua", cl_init),
            ("gamemodes/helix/gamemode/shared.lua", shared_source),
        ]);
        let shared = ws.def_file("gamemodes/helix/gamemode/shared.lua", shared_source);

        let diagnostics = ws
            .analysis
            .diagnose_file(shared, CancellationToken::new())
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
    fn test_child_gamemode_sees_parent_guarded_global_table_fields() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        ws.def_file(
            "gamemodes/helix/gamemode/init.lua",
            r#"
                ix = ix or { util = {}, meta = {} }
            "#,
        );
        ws.def_file(
            "gamemodes/helix/gamemode/cl_init.lua",
            r#"
                ix = ix or { util = {}, gui = {}, meta = {} }
            "#,
        );
        let child_hooks = ws.def_file(
            "gamemodes/helix-hl2rp/schema/cl_hooks.lua",
            r#"
                function Schema:CharacterLoaded(character)
                    if (IsValid(ix.gui.combine)) then
                        ix.gui.combine:Remove()
                    end
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(child_hooks, CancellationToken::new())
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
    fn test_guarded_global_table_bootstraps_merge_fields_across_files() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        ws.def_file(
            "gamemodes/helix/gamemode/init.lua",
            r#"
                ix = ix or { util = {} }
            "#,
        );
        ws.def_file(
            "gamemodes/helix/gamemode/cl_init.lua",
            r#"
                ix = ix or { gui = {} }
            "#,
        );
        let child_hooks = ws.def_file(
            "gamemodes/helix-hl2rp/schema/cl_hooks.lua",
            r#"
                ix.util.Include("core/meta/sh_entity.lua")

                if (IsValid(ix.gui.combine)) then
                    ix.gui.combine:Remove()
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(child_hooks, CancellationToken::new())
            .unwrap_or_default();
        let undefined_fields: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedField.get_name().to_string(),
                    ))
            })
            .collect();

        assert!(
            undefined_fields.is_empty(),
            "unexpected UndefinedField diagnostics: {undefined_fields:#?}"
        );
    }

    #[test]
    fn test_guarded_nested_table_redefinition_preserves_existing_fields() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        ws.def_file(
            "gamemodes/helix/gamemode/core/sh_util.lua",
            r#"
                ix = ix or {}
                ix.type = ix.type or {
                    [2] = "string",
                    [4] = "text",
                    string = 2,
                    text = 4,
                }
            "#,
        );
        ws.def_file(
            "gamemodes/helix/gamemode/shared.lua",
            r#"
                ix = ix or {}
                ix.type = ix.type or {}
            "#,
        );
        let command_file = ws.def_file(
            "gamemodes/helix-hl2rp/schema/sh_commands.lua",
            r#"
                local argumentType = ix.type.text
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(command_file, CancellationToken::new())
            .unwrap_or_default();
        let undefined_fields: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedField.get_name().to_string(),
                    ))
            })
            .collect();

        assert!(
            undefined_fields.is_empty(),
            "unexpected UndefinedField diagnostics: {undefined_fields:#?}"
        );
    }

    #[test]
    fn test_guarded_nested_table_redefinition_preserves_existing_fields_on_cold_batch_load() {
        let mut analysis = crate::EmmyLuaAnalysis::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        analysis.update_config(Arc::new(emmyrc));
        analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        let root = std::env::temp_dir().join("gmod_glua_ls_ix_type_batch");
        let helix_root = root.join("garrysmod/gamemodes/helix");
        let schema_root = root.join("garrysmod/gamemodes/helix-hl2rp");
        analysis.add_library_workspace(helix_root.clone());
        analysis.add_main_workspace(schema_root.clone());

        let sh_util = r#"
                --- Various useful helper functions.
                -- @module ix.util

                ix.type = ix.type or {
                    [4] = "text",
                    [8] = "number",
                    text = 4,
                    number = 8,
                }
            "#;
        let init = r#"
                ix = ix or { util = {}, meta = {} }
                include("core/sh_util.lua")
                include("shared.lua")
            "#;
        let cl_init = r#"
                ix = ix or { gui = {}, util = {}, meta = {} }
                include("core/sh_util.lua")
                include("shared.lua")
            "#;
        let shared = r#"
                --- Top-level library containing all Helix libraries.
                -- @module ix

                --- A table of variable types that are used throughout the framework.
                -- @table ix.type
                -- @realm shared
                -- @field text A regular string.
                ix.type = ix.type or {}
            "#;
        let command = r#"
                local argumentType = ix.type.text
                local numberType = ix.type.number
            "#;

        let command_path = schema_root.join("schema/sh_commands.lua");
        analysis.update_files_by_path(vec![
            (
                helix_root.join("gamemode/shared.lua"),
                Some(shared.to_string()),
            ),
            (
                helix_root.join("gamemode/core/sh_util.lua"),
                Some(sh_util.to_string()),
            ),
            (helix_root.join("gamemode/init.lua"), Some(init.to_string())),
            (
                helix_root.join("gamemode/cl_init.lua"),
                Some(cl_init.to_string()),
            ),
            (command_path.clone(), Some(command.to_string())),
        ]);

        let shared_path = helix_root.join("gamemode/shared.lua");
        let shared_uri = lsp_types::Uri::parse_from_file_path(&shared_path)
            .expect("expected shared path to convert to uri");
        let shared_file = analysis
            .get_file_id(&shared_uri)
            .expect("expected shared file id");
        let tree = analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&shared_file)
            .expect("expected shared syntax tree");
        let root = tree.get_red_root();
        let mut lookup_member_count = None;
        for assign_stat in root
            .descendants()
            .filter_map(glua_parser::LuaAssignStat::cast)
        {
            let (vars, _) = assign_stat.get_var_and_expr_list();
            for var in vars {
                if var.syntax().text() != "ix.type" {
                    continue;
                }
                let glua_parser::LuaVarExpr::IndexExpr(index_expr) = var else {
                    continue;
                };
                let member_id = crate::LuaMemberId::new(index_expr.get_syntax_id(), shared_file);
                let member_index = analysis.compilation.get_db().get_member_index();
                let owner = member_index
                    .get_member_owner(&member_id)
                    .expect("expected ix.type owner");
                let key = member_index
                    .get_member(&member_id)
                    .expect("expected ix.type member")
                    .get_key();
                lookup_member_count = member_index
                    .get_member_item(owner, key)
                    .map(|member_item| member_item.get_member_ids().len());
            }
        }

        assert!(
            lookup_member_count.is_some_and(|count| count >= 2),
            "guarded ix.type definitions should stay in the lookup item after cold batch load, found {lookup_member_count:?}"
        );

        let command_uri = lsp_types::Uri::parse_from_file_path(&command_path)
            .expect("expected command path to convert to uri");
        let command_file = analysis
            .get_file_id(&command_uri)
            .expect("expected command file id");
        let diagnostics = analysis
            .diagnose_file(command_file, CancellationToken::new())
            .unwrap_or_default();
        let ix_type = index_expr_type(&analysis, command_file, "ix.type");
        let ix_type_display = humanize_type(
            analysis.compilation.get_db(),
            &ix_type,
            RenderLevel::Detailed,
        );

        assert!(
            ix_type_display.contains("text") && ix_type_display.contains("number"),
            "expected ix.type to retain detailed fields, got {ix_type_display}"
        );
        assert!(
            !contains_empty_table_bootstrap(analysis.compilation.get_db(), &ix_type),
            "expected ix.type to omit redundant empty/bootstrap table branch, got {ix_type_display}"
        );

        let undefined_fields: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedField.get_name().to_string(),
                    ))
            })
            .collect();

        assert!(
            undefined_fields.is_empty(),
            "unexpected UndefinedField diagnostics on cold batch load: {undefined_fields:#?}"
        );
    }

    #[test]
    fn test_guarded_empty_table_bootstrap_remains_table_when_only_definition() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def_file(
            "gamemodes/helix/gamemode/shared.lua",
            r#"
                ix = ix or {}
                ix.type = ix.type or {}

                local typeTable = ix.type
            "#,
        );

        let ty = local_name_type(&mut ws, file_id, "typeTable");
        let display = ws.humanize_type(ty.clone());

        assert!(
            display == "table" || matches!(ty, LuaType::Table | LuaType::TableConst(_)),
            "expected lone guarded bootstrap to remain table-like, got {display}"
        );
    }

    #[test]
    fn test_repeated_guarded_empty_table_bootstraps_collapse_to_single_table() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def_file(
            "gamemodes/helix/gamemode/shared.lua",
            r#"
                ix = ix or {}
                ix.type = ix.type or {}
            "#,
        );
        let command_file = ws.def_file(
            "gamemodes/helix-hl2rp/schema/sh_commands.lua",
            r#"
                ix = ix or {}
                ix.type = ix.type or {}

                local typeTable = ix.type
            "#,
        );

        let ty = local_name_type(&mut ws, command_file, "typeTable");
        let display = ws.humanize_type(ty.clone());

        assert_eq!(display, "table");
        assert_eq!(
            empty_table_bootstrap_branch_count(ws.analysis.compilation.get_db(), &ty),
            1,
            "expected repeated empty guarded bootstraps to collapse to one table branch, got {display}"
        );
    }

    #[test]
    fn test_repeated_initialized_index_prefixes_do_not_report_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        let mut body = String::from("WepHolster = {}\nWepHolster.defData = {}\n");
        for i in 0..40 {
            body.push_str(&format!(
                r#"
                    WepHolster.defData["weapon_{i}"] = {{}}
                    WepHolster.defData["weapon_{i}"].Model = "models/weapons/w_{i}.mdl"
                    WepHolster.defData["weapon_{i}"].Bone = "ValveBiped.Bip01_Spine"
"#
            ));
        }

        let file_id = ws.def(&body);
        let diags = diagnostics_for_code(&mut ws, file_id, DiagnosticCode::UndefinedField);

        assert!(
            diags.is_empty(),
            "initialized table prefixes should not produce undefined-field diagnostics: {diags:#?}"
        );
    }

    #[test]
    fn test_repeated_initialized_index_prefixes_diagnose_quick_smoke() {
        let mut ws = VirtualWorkspace::new();
        let mut body = String::from("WepHolster = {}\nWepHolster.defData = {}\n");
        for i in 0..250 {
            body.push_str(&format!(
                r#"
                    WepHolster.defData["weapon_{i}"] = {{}}
                    WepHolster.defData["weapon_{i}"].Model = "models/weapons/w_{i}.mdl"
                    WepHolster.defData["weapon_{i}"].Bone = "ValveBiped.Bip01_Spine"
"#
            ));
        }

        let file_id = ws.def(&body);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        let start = std::time::Instant::now();
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let elapsed = start.elapsed();

        assert!(
            diagnostics.is_empty(),
            "unexpected undefined-field diagnostics: {diagnostics:#?}"
        );
        assert!(
            elapsed.as_millis() < 250,
            "repeated initialized index-prefix diagnostics took too long: {elapsed:?}"
        );
    }

    #[test]
    fn test_guarded_empty_table_bootstrap_keeps_typed_member_access_valid() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        ws.def_file(
            "annotations/workshopfilebase.FillFileInfo.lua",
            r#"
                ---@meta
                ---@class (partial) WorkshopFileInfoEntry
                ---@field file string Local addon file path when available.
                ---@field ownername string
                ---@field description string
                local WorkshopFileInfoEntry = {}

                ---@class (partial) WorkshopUserContentEntry
                ---@field file? string Local addon file path when available.
                ---@field ownername string
                ---@field description string
                local WorkshopUserContentEntry = {}

                ---@class WorkshopFileInfoResults
                ---@field extraresults table<integer, WorkshopFileInfoEntry|WorkshopUserContentEntry|nil>

                ---@param results WorkshopFileInfoResults
                ---@param isUGC? boolean
                function WorkshopFileBase:FillFileInfo(results, isUGC) end
            "#,
        );

        let file_id = ws.def_file(
            "garrysmod/lua/includes/util/workshop_files.lua",
            r#"
                function WorkshopFileBase(namespace, requiredtags)
                    local ret = {}
                    ret.HTML = nil

                    function ret:FillFileInfo(results, isUGC)
                        local k = 1
                        local extra = results.extraresults[k]
                        if not extra then
                            extra = {}
                        end
                        extra.ownername = "Local"
                        extra.description = "Non workshop .gma addon. (" .. extra.file .. ")"
                        local missing = extra.missing
                    end

                    return ret
                end
            "#,
        );

        let diags = diagnostics_for_code(&mut ws, file_id, DiagnosticCode::UndefinedField);
        let results_types = index_expr_type_displays(&ws, file_id, "results.extraresults[k]");
        assert_eq!(diags.len(), 1);
        assert!(
            diags[0].message.contains("`missing`"),
            "expected only extra.missing to remain, got {diags:?}"
        );
        assert!(
            results_types
                .iter()
                .any(|display| display.contains("WorkshopFileInfoEntry")
                    && display.contains("WorkshopUserContentEntry")),
            "expected typed union on results.extraresults[k], got {results_types:?}"
        );
    }

    #[test]
    fn test_fallback_table_union_still_reports_truly_missing_fields() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        ws.def_file(
            "annotations/workshopfilebase.FillFileInfo.lua",
            r#"
                ---@meta
                ---@class (partial) WorkshopFileInfoEntry
                ---@field file string Local addon file path when available.
                ---@field ownername string
                ---@field description string
                local WorkshopFileInfoEntry = {}

                ---@class (partial) WorkshopUserContentEntry
                ---@field file? string Local addon file path when available.
                ---@field ownername string
                ---@field description string
                local WorkshopUserContentEntry = {}

                ---@class WorkshopFileInfoResults
                ---@field extraresults table<integer, WorkshopFileInfoEntry|WorkshopUserContentEntry|nil>

                ---@param results WorkshopFileInfoResults
                ---@param isUGC? boolean
                function WorkshopFileBase:FillFileInfo(results, isUGC) end
            "#,
        );

        let file_id = ws.def_file(
            "garrysmod/lua/includes/util/workshop_files.lua",
            r#"
                function WorkshopFileBase(namespace, requiredtags)
                    local ret = {}
                    ret.HTML = nil

                    function ret:FillFileInfo(results, isUGC)
                        local k = 1
                        local extra = results.extraresults[k]
                        if not extra then
                            extra = {}
                        end
                        extra.ownername = "Local"
                        local missing = extra.missing
                    end

                    return ret
                end
            "#,
        );

        let diags = diagnostics_for_code(&mut ws, file_id, DiagnosticCode::UndefinedField);
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn test_missing_member_probe_does_not_nil_poison_later_valid_member_inference() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        ws.def_file(
            "annotations/workshopfilebase.FillFileInfo.lua",
            r#"
                ---@meta
                ---@class WorkshopFileInfoEntry
                ---@field file string
                ---@field ownername string
                local WorkshopFileInfoEntry = {}

                ---@class WorkshopFileInfoResults
                ---@field extraresults table<integer, WorkshopFileInfoEntry|nil>
            "#,
        );

        let file_id = ws.def_file(
            "garrysmod/lua/includes/util/workshop_files.lua",
            r#"
                ---@param results WorkshopFileInfoResults
                local function FillFileInfo(results)
                    local k = 1
                    local extra = results.extraresults[k]
                    local missing = extra.missing
                    local path = extra.file
                    local len = #extra.file
                    local again = extra.file
                end
            "#,
        );

        let diags = diagnostics_for_code(&mut ws, file_id, DiagnosticCode::UndefinedField);
        let file_types = index_expr_type_displays(&ws, file_id, "extra.file");

        assert_eq!(diags.len(), 1, "expected only extra.missing, got {diags:?}");
        assert!(
            diags[0].message.contains("`missing`"),
            "expected missing-field diagnostic for extra.missing, got {diags:?}"
        );
        assert!(
            file_types.iter().all(|display| display == "string"),
            "valid member inference should remain stable after missing-member probe"
        );
    }

    #[test]
    fn test_guarded_nested_table_redefinition_appends_rhs_fields_across_files() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        ws.def_file(
            "gamemodes/helix/gamemode/core/sh_util.lua",
            r#"
                ix = ix or {}
                ix.type = ix.type or {
                    text = 4,
                }
            "#,
        );
        ws.def_file(
            "gamemodes/helix/gamemode/shared.lua",
            r#"
                ix = ix or {}
                ix.type = ix.type or {
                    number = 8,
                }
            "#,
        );
        let command_file = ws.def_file(
            "gamemodes/helix-hl2rp/schema/sh_commands.lua",
            r#"
                local textType = ix.type.text
                local numberType = ix.type.number
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(command_file, CancellationToken::new())
            .unwrap_or_default();
        let undefined_fields: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedField.get_name().to_string(),
                    ))
            })
            .collect();

        assert!(
            undefined_fields.is_empty(),
            "unexpected UndefinedField diagnostics: {undefined_fields:#?}"
        );
    }

    #[test]
    fn test_guarded_table_redefinition_keeps_known_non_table_or_semantics() {
        let mut ws = VirtualWorkspace::new();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        let file_id = ws.def_file(
            "gamemodes/helix/gamemode/shared.lua",
            r#"
                ix = "already initialized"
                ix = ix or { gui = {} }
                local panel = ix.gui
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();

        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedField.get_name().to_string(),
                    ))
                    && diagnostic.message == "Undefined field `gui`. "
            }),
            "expected UndefinedField for gui on known non-table guard, got {diagnostics:#?}"
        );
    }

    #[test]
    fn test_global_path_table_member_fallback_respects_local_shadow() {
        let mut ws = VirtualWorkspace::new();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        ws.def_file(
            "global_util.lua",
            r#"
                ix = ix or { util = {} }
                function ix.util.Include(fileName)
                end
            "#,
        );
        let file_id = ws.def_file(
            "shadow.lua",
            r#"
                local ix = { util = {} }
                ix.util.Include("missing.lua")
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
                .any(|diagnostic| diagnostic.code == undefined_field
                    && diagnostic.message == "Undefined field `Include`. "),
            "expected UndefinedField for local shadow, got {diagnostics:#?}"
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
    fn test_dynamic_index_on_empty_table_no_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local tbl = {}
                local id = 1
                local _ = tbl[id]
            "#
        ));
    }

    #[test]
    fn test_gamemode_pairs_copy_fields_no_undefined_field() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class GM
                ---@type GM
                GAMEMODE = {}

                local source = {
                    soundA = "ambient/water/drip1.wav",
                    soundB = "ambient/water/drip2.wav",
                }

                for key, val in pairs(source) do
                    GAMEMODE[key] = val
                end

                local _a = GAMEMODE.soundA
                local _b = GAMEMODE.soundB
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
    fn test_dynamic_key_only_table_suppresses_exact_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local test = {}

                local function assign(key)
                    test[key] = true
                end

                print(test.meow)
            "#
        ));
    }

    #[test]
    fn test_literal_key_table_still_reports_exact_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local test = {}

                test.known = true

                print(test.meow)
            "#
        ));
    }

    #[test]
    fn test_integer_literal_key_table_still_reports_exact_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local test = {}

                test[1] = true

                print(test.meow)
            "#
        ));
    }

    #[test]
    fn test_module_function_does_not_take_call_site_params_from_shadowed_global_constructor() {
        let mut ws = VirtualWorkspace::new();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);
        ws.def(
            r#"
                ---@meta

                ---@class Color
                ---@field r number
                ---@field g number
                ---@field b number
                ---@field a number

                ---@param r number
                ---@param g number
                ---@param b number
                ---@param a? number
                ---@return Color
                function Color(r, g, b, a) end
            "#,
        );
        let file_id = ws.def(
            r#"
                local _Color = Color

                module("markup")

                function Color(col)
                    return col.r + col.g + col.b + (col.a or 0)
                end

                local Color = _Color
                local white = Color(255, 255, 255)
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        assert!(
            diagnostics.iter().all(|diagnostic| diagnostic.code != code),
            "expected no undefined-field diagnostics, got {diagnostics:#?}"
        );
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
        ws.def_gmod_type_predicates();
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
    fn test_table_insert_array_numeric_index_no_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local queuedSearch = {}

                local function Queue(tab, folder, extension, path)
                    table.insert(queuedSearch, { tab, folder, extension, path })
                end

                Queue("models", "props/", "*.mdl", "GAME")
                local call = queuedSearch[1]
            "#,
        ));
    }

    #[test]
    fn test_or_empty_table_field_numeric_index_no_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class WheelEntity
                local wheelEnt = {}

                wheelEnt.KeyBinds = wheelEnt.KeyBinds or {}
                numpad.Remove(wheelEnt.KeyBinds[1])
                wheelEnt.KeyBinds[1] = 123
            "#,
        ));
    }

    #[test]
    fn test_closed_table_named_missing_field_still_reports() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class ClosedConfig
                ---@field known string
                local cfg = {}

                local missing = cfg.unknown
            "#,
        ));
    }

    #[test]
    fn test_union_typeguard_on_any_still_reports_unknown_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class Entity
                local Entity = {}

                ---@class Panel
                local Panel = {}
                function Panel:SetVisible(visible) end

                ---@class PhysObj
                local PhysObj = {}
                function PhysObj:Wake() end

                ---@param value any
                ---@return TypeGuard<Entity|Panel|PhysObj>
                function IsValid(value) end

                ---@param ent any
                local function use(ent)
                    if IsValid(ent) then
                        ent.someTypo()
                    end
                end
            "#,
        ));
    }

    #[test]
    fn test_union_typeguard_on_object_still_reports_unknown_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class Entity
                local Entity = {}

                ---@class Panel
                local Panel = {}
                function Panel:SetVisible(visible) end

                ---@class PhysObj
                local PhysObj = {}
                function PhysObj:Wake() end

                ---@param value any
                ---@return TypeGuard<Entity|Panel|PhysObj>
                function IsValid(value) end

                ---@param ent { known: string }
                local function use(ent)
                    if IsValid(ent) then
                        ent.someTypo()
                    end
                end
            "#,
        ));
    }

    fn def_valid_guard_fixture(ws: &mut VirtualWorkspace) {
        ws.def(
            r#"
                ---@meta
                ---@attribute valid_guard()

                ---@class Entity
                function Entity:SetHealth(health) end

                ---@class PhysObj
                function PhysObj:SetMass(mass) end

                ---@class DTree_Node
                function DTree_Node:InternalDoClick() end

                ---@param value any
                ---@return TypeGuard<Entity>
                ---@[valid_guard]
                function IsValid(value) end
            "#,
        );
    }

    #[test]
    fn test_valid_guard_unknown_physobj_source_no_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        def_valid_guard_fixture(&mut ws);
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local function use(ent)
                    local phys = ent:GetPhysicsObject()
                    if not IsValid(phys) then return end

                    phys:SetMass(100)
                end
            "#,
        ));
    }

    #[test]
    fn test_valid_guard_unknown_dtree_chain_no_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        def_valid_guard_fixture(&mut ws);
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local function use(tree)
                    local node = tree:Root():GetChildNode(0)
                    if not IsValid(node) then return end

                    node:InternalDoClick()
                end
            "#,
        ));
    }

    #[test]
    fn test_valid_guard_typed_physobj_preserves_type_and_reports_bogus_field() {
        let mut ws = VirtualWorkspace::new();
        def_valid_guard_fixture(&mut ws);
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@param phys PhysObj
                local function use(phys)
                    if not IsValid(phys) then return end

                    phys:SetMass(100)
                    local typo = phys.EntityOnlyTypo
                end
            "#,
        ));
    }

    #[test]
    fn test_valid_guard_plain_entity_typo_still_reports() {
        let mut ws = VirtualWorkspace::new();
        def_valid_guard_fixture(&mut ws);
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@param ent Entity
                local function use(ent)
                    if not IsValid(ent) then return end

                    ent:SetHealth(100)
                    local typo = ent.EntityTypo
                end
            "#,
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
    fn test_nil_guarded_field_if_condition_does_not_guard_elseif_body() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class TestConfigElseIf
                local cfg = {}

                if cfg.dynamicField then
                    print(cfg.dynamicField)
                elseif true then
                    print(cfg.dynamicField)
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
    fn test_outparam_updated_fallback_table_field_no_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        ws.def_file(
            "trace.lua",
            r#"
                ---@class Vector
                local Vector = {}

                ---@class TraceResult
                ---@field HitPos Vector
                local TraceResult = {}

                util = {}

                ---@outparam traceConfig.output TraceResult
                ---@param traceConfig table
                function util.TraceHull(traceConfig) end
            "#,
        );

        let file_id = ws.def_file(
            "lua/entities/glide_wheel/init.lua",
            r#"
                ---@class Glide
                Glide = Glide or {}

                local ray = Glide.LastWheelTraceResult or {}
                local traceData = {
                    output = ray,
                }

                util.TraceHull(traceData)

                local hitPos = ray.HitPos
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, tokio_util::sync::CancellationToken::new())
            .unwrap_or_default();
        let undefined_fields: Vec<_> = diagnostics
            .iter()
            .filter(|d| {
                d.code
                    == Some(lsp_types::NumberOrString::String(
                        "undefined-field".to_string(),
                    ))
            })
            .collect();

        assert!(
            undefined_fields.is_empty(),
            "outparam-populated ray table should not produce undefined-field diagnostics: {undefined_fields:?}"
        );
    }

    #[test]
    fn test_outparam_updated_class_state_output_field_no_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        ws.def_file(
            "trace.lua",
            r#"
                ---@class Vector
                local Vector = {}

                ---@class TraceResult
                ---@field Hit boolean
                ---@field HitPos Vector
                local TraceResult = {}

                util = {}

                ---@outparam traceConfig.output TraceResult
                ---@param traceConfig table
                function util.TraceHull(traceConfig) end
            "#,
        );

        let file_id = ws.def_file(
            "lua/entities/glide_wheel/init.lua",
            r#"
                ---@class Glide
                Glide = Glide or {}

                ---@class GlideWheelState

                ---@class glide_wheel : Entity
                ---@field state GlideWheelState
                local ENT = {}

                ---@class Entity
                local Entity = {}

                ---@return tableof<self>
                function Entity:GetTable() end

                function FindMetaTable(name)
                    return Entity
                end

                local getTable = FindMetaTable("Entity").GetTable
                local ray = Glide.LastWheelTraceResult or {}

                function ENT:Initialize()
                    ---@type glide_wheel
                    local selfTbl = getTable(self)
                    selfTbl.state = {
                        traceData = {
                            output = ray,
                        },
                    }
                end

                function ENT:OnPostThink()
                    ---@type glide_wheel
                    local selfTbl = getTable(self)
                    local traceData = selfTbl.state.traceData
                    util.TraceHull(traceData)

                    local hit = ray.Hit
                    local hitPos = ray.HitPos
                end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, tokio_util::sync::CancellationToken::new())
            .unwrap_or_default();
        let undefined_fields: Vec<_> = diagnostics
            .iter()
            .filter(|d| {
                d.code
                    == Some(lsp_types::NumberOrString::String(
                        "undefined-field".to_string(),
                    ))
            })
            .collect();

        assert!(
            undefined_fields.is_empty(),
            "outparam-populated class state trace output should not produce undefined-field diagnostics: {undefined_fields:?}"
        );
    }

    #[test]
    fn test_dynamic_field_setter_helper_call_no_undefined_field() {
        // Proves a real helper discovered from another file still suppresses
        // undefined-field after the helper-call fast-path guards.
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        ws.enable_check(DiagnosticCode::UndefinedField);

        ws.def_file(
            "lua/autorun/sh_helpers.lua",
            r#"
                Glide = {}

                function Glide.SetNumber(t, k, v)
                    t[k] = v
                end
            "#,
        );

        let file_id = ws.def_file(
            "lua/weapons/gmod_tool/stools/glide_make_amphibious.lua",
            r#"
                local data = {}
                local SetNumber = Glide.SetNumber

                SetNumber(data, "buoyancyOffsetZ", 1)

                local offset = data.buoyancyOffsetZ
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, tokio_util::sync::CancellationToken::new())
            .unwrap_or_default();
        let undefined_fields: Vec<_> = diagnostics
            .iter()
            .filter(|d| {
                d.code
                    == Some(lsp_types::NumberOrString::String(
                        "undefined-field".to_string(),
                    ))
            })
            .collect();

        assert!(
            undefined_fields.is_empty(),
            "table fields written through helper calls should not produce undefined-field diagnostics: {undefined_fields:?}"
        );
    }

    #[test]
    fn test_dynamic_field_setter_helper_unknown_key_still_reports_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local function SetField(t, k, v)
                    t[k] = v
                end

                local data = {}
                local unknownKey = getKey()
                SetField(data, unknownKey, 1)

                local offset = data.buoyancyOffsetZ
            "#,
        ));
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
    fn test_setmetatable_constructor_field_visible_to_sibling_method_with_index() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class TestLocationInfo
                ---@field Name string

                local LOCATION = {}
                LOCATION.__index = LOCATION

                ---@param loc_id integer
                ---@param loc_info TestLocationInfo
                function LOCATION:Init(loc_id, loc_info)
                    local instance = loc_info
                    setmetatable(instance, self)
                    instance._OriginalName = loc_info.Name
                    return instance
                end

                function LOCATION:GetOriginalName()
                    return self._OriginalName
                end
            "#
        ));
    }

    #[test]
    fn test_setmetatable_constructor_field_without_index_still_undefined() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class TestLocationInfoNoIndex
                ---@field Name string

                local LOCATION = {}

                ---@param loc_id integer
                ---@param loc_info TestLocationInfoNoIndex
                function LOCATION:Init(loc_id, loc_info)
                    local instance = loc_info
                    setmetatable(instance, self)
                    instance._OriginalName = loc_info.Name
                    return instance
                end

                function LOCATION:GetOriginalName()
                    return self._OriginalName
                end
            "#
        ));
    }

    #[test]
    fn test_setmetatable_constructor_field_shadowed_instance_stays_undefined() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local LOCATION = {}
                LOCATION.__index = LOCATION

                function LOCATION:Init()
                    local instance = {}
                    do
                        local instance = {}
                        setmetatable(instance, self)
                    end

                    instance._OriginalName = true
                    return instance
                end

                function LOCATION:GetOriginalName()
                    return self._OriginalName
                end
            "#
        ));
    }

    #[test]
    fn test_setmetatable_constructor_field_nested_closure_stays_undefined() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                local LOCATION = {}
                LOCATION.__index = LOCATION

                function LOCATION:Init()
                    local instance = {}

                    local function attach()
                        setmetatable(instance, self)
                    end

                    attach()
                    instance._OriginalName = true
                    return instance
                end

                function LOCATION:GetOriginalName()
                    return self._OriginalName
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

    #[test]
    fn test_return_table_numeric_index_no_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        // Numeric indexing on a value from ---@return table should not trigger
        // undefined-field (the table is generic/open, not a tracked literal).
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@param s string
            ---@return table
            local function string_Split(s) end

            local VecComp = string_Split("1 2 3", " ")
            local ang = VecComp[1]
            "#
        ));
    }

    #[test]
    fn test_return_table_string_index_no_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        // String-key indexing (dot and bracket) on a value from ---@return table
        // should not trigger undefined-field.
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@return table
            local function getConfig() end

            local cfg = getConfig()
            local a = cfg.foo
            local b = cfg["bar"]
            "#
        ));
    }

    #[test]
    fn test_cross_file_snapshot_record_undefined_field_stable_after_reindex() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        ws.def_file(
            "lua/includes/glua_stubs.lua",
            r#"
            net = {}
            util = {}

            ---@return number
            function net.ReadUInt(bits) end

            ---@return boolean
            function net.ReadBool() end

            ---@return string
            function net.ReadString() end

            ---@return number
            function net.ReadFloat() end

            ---@return table?
            function util.JSONToTable(json) end

            ---@class Vector
            ---@return Vector
            function Vector(x, y, z) end

            ---@return number
            function SysTime() end
            "#,
        );

        ws.def_file(
            "lua/glide/sh_network.lua",
            r#"
            Glide = Glide or {}
            Glide.DebugNetwork = Glide.DebugNetwork or {}
            local DebugNet = Glide.DebugNetwork
            local TYPE = {
                BOOL = 1,
                FLOAT = 2,
                STRING = 3,
                VECTOR = 4,
                JSON = 5,
            }

            local function readValue()
                local typeId = net.ReadUInt(3)
                if typeId == TYPE.BOOL then
                    return net.ReadBool()
                end
                if typeId == TYPE.FLOAT then
                    return net.ReadFloat()
                end
                if typeId == TYPE.STRING then
                    return net.ReadString()
                end
                if typeId == TYPE.VECTOR then
                    return Vector(net.ReadFloat(), net.ReadFloat(), net.ReadFloat())
                end
                if typeId == TYPE.JSON then
                    local json = net.ReadString()
                    local tbl = util.JSONToTable(json or "")
                    return tbl or {}
                end

                return nil
            end

            function DebugNet.ReadSnapshot()
                local entId = net.ReadUInt(16)
                local hasVehicle = net.ReadBool()
                local vehicleId = nil
                if hasVehicle then
                    vehicleId = net.ReadUInt(16)
                    if vehicleId == 0 then vehicleId = nil end
                end

                local fieldCount = net.ReadUInt(6)
                local fields = {}
                for _ = 1, fieldCount do
                    local key = net.ReadString()
                    fields[key] = readValue()
                end

                return entId, vehicleId, fields
            end
            "#,
        );

        let network_source = r#"
            local DebugNet = Glide.DebugNetwork
            local commands = {}
            Glide.CMD_DEBUG_SNAPSHOT = 1

            commands[Glide.CMD_DEBUG_SNAPSHOT] = function()
                local entId, vehicleId, fields = DebugNet.ReadSnapshot()
                if not entId then return end

                Glide.DebugSnapshots = Glide.DebugSnapshots or {}
                local rec = Glide.DebugSnapshots[entId]
                if not rec then
                    rec = { data = {}, t = SysTime() }
                    Glide.DebugSnapshots[entId] = rec
                end

                local data = rec.data

                if vehicleId ~= nil then
                    data.vehicle = vehicleId
                else
                    data.vehicle = nil
                end

                for key, value in pairs( fields ) do
                    if key ~= "ent" then
                        data[key] = value
                    end
                end
            end
        "#;
        let network_file = ws.def_file("lua/glide/client/network.lua", network_source);

        let debugging_source = r#"
            local function draw()
                local snaps = Glide.DebugSnapshots or {}

                for entId, rec in pairs(snaps) do
                    if not rec or not rec.data then return end

                    local d = rec.data
                    local contactPos = d.contactPos
                end
            end
        "#;
        let debugging_file = ws.def_file("lua/glide/client/debugging.lua", debugging_source);

        let contact_pos_code = Some(lsp_types::NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        let contact_pos_message = "Undefined field `contactPos`. ";
        let initial_diagnostics = ws
            .analysis
            .diagnose_file(debugging_file, tokio_util::sync::CancellationToken::new())
            .unwrap_or_default();
        assert!(
            !initial_diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == contact_pos_code
                    && diagnostic.message == contact_pos_message),
            "expected no initial undefined-field for contactPos, got {initial_diagnostics:?}"
        );

        let uri = ws
            .virtual_url_generator
            .new_uri("lua/glide/client/network.lua");
        ws.analysis
            .update_file_text_only(&uri, format!("\n{network_source}"));
        ws.analysis.reindex_files(vec![network_file]);

        let after_reindex_diagnostics = ws
            .analysis
            .diagnose_file(debugging_file, tokio_util::sync::CancellationToken::new())
            .unwrap_or_default();
        assert!(
            !after_reindex_diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == contact_pos_code
                    && diagnostic.message == contact_pos_message),
            "expected no post-reindex undefined-field for contactPos, got {after_reindex_diagnostics:?}"
        );
    }

    #[test]
    fn test_panel_self_assignment_to_table_field_preserves_panel_type_cross_file() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);

        ws.def_file(
            "gamemodes/helix/gamemode/cl_init.lua",
            r#"
                ix = ix or { gui = {} }
            "#,
        );
        ws.def(
            r#"
            ---@class DPanel
            ---@field Remove fun(self: DPanel)
            "#,
        );
        ws.def_file(
            "gamemodes/helix-hl2rp/schema/derma/cl_combinedisplay.lua",
            r#"
            local PANEL = {}

            function PANEL:Init()
                ix.gui.combine = self
            end

            function PANEL:Paint(w, h)
            end

            vgui.Register("ixCombineDisplay", PANEL, "DPanel")
            "#,
        );
        let hooks_file = ws.def_file(
            "gamemodes/helix-hl2rp/schema/cl_hooks.lua",
            r#"
                if (IsValid(ix.gui.combine)) then
                    ix.gui.combine:Remove()
                end
            "#,
        );
        let gui_displays = index_expr_type_displays(&ws, hooks_file, "ix.gui");
        let combine_displays = index_expr_type_displays(&ws, hooks_file, "ix.gui.combine");

        let diagnostics = ws
            .analysis
            .diagnose_file(hooks_file, CancellationToken::new())
            .unwrap_or_default();
        let undefined_fields: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedField.get_name().to_string(),
                    ))
            })
            .collect();

        assert!(
            undefined_fields.is_empty(),
            "unexpected UndefinedField diagnostics for panel self-assignment on gui={gui_displays:?} combine={combine_displays:?}: {undefined_fields:#?}"
        );
    }

    #[test]
    fn test_panel_self_assignment_to_table_field_type_is_panel_class() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def_file(
            "gamemodes/helix/gamemode/cl_init.lua",
            r#"
                ix = ix or { gui = {} }
            "#,
        );
        let panel_file = ws.def_file(
            "gamemodes/helix-hl2rp/schema/derma/cl_combinedisplay.lua",
            r#"
            local PANEL = {}

            function PANEL:Init()
                ix.gui.combine = self
            end

            function PANEL:Paint(w, h)
            end

            vgui.Register("ixCombineDisplay", PANEL, "DPanel")
            "#,
        );

        let combine_type = assign_value_type(&mut ws, panel_file, "ix.gui.combine");
        let display = ws.humanize_type(combine_type);
        assert!(
            display.contains("ixCombineDisplay"),
            "expected ix.gui.combine to be typed as ixCombineDisplay, got: {display}"
        );
    }

    #[test]
    fn test_panel_self_assignment_to_table_field_type_visible_cross_file() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def_file(
            "gamemodes/helix/gamemode/cl_init.lua",
            r#"
                ix = ix or { gui = {} }
            "#,
        );
        ws.def_file(
            "gamemodes/helix-hl2rp/schema/derma/cl_combinedisplay.lua",
            r#"
            local PANEL = {}

            function PANEL:Init()
                ix.gui.combine = self
            end

            function PANEL:Paint(w, h)
            end

            vgui.Register("ixCombineDisplay", PANEL, "DPanel")
            "#,
        );
        let hooks_file = ws.def_file(
            "gamemodes/helix-hl2rp/schema/cl_hooks.lua",
            r#"
                local c = ix.gui.combine
            "#,
        );

        let combine_type = local_name_type(&mut ws, hooks_file, "c");
        let display = ws.humanize_type(combine_type);
        assert!(
            display.contains("ixCombineDisplay"),
            "expected cross-file ix.gui.combine to be typed as ixCombineDisplay, got: {display}"
        );
    }

    #[test]
    fn test_boolean_union_narrowing_undefined_field_bug() {
        let mut ws = VirtualWorkspace::new();
        ws.def_gmod_call_arg_builtins();

        let file_id = ws.def(
            r#"
            ---@return any
            local function get_any() return end
            local function test_narrow(a)
                if not a then return false end
                return a
            end
            local function test_main()
                local x = test_narrow(get_any())
                -- The analyzer bug incorrectly drops 'any' and infers 'x' as strictly 'boolean',
                -- causing 'GetTranslation' to emit an undefined-field error.
                x:GetTranslation()
            end
            "#,
        );
        let x_type = local_name_type(&mut ws, file_id, "x");
        assert!(
            x_type.is_any(),
            "the narrowed helper return must preserve any, got {}",
            ws.humanize_type(x_type)
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        assert!(
            diagnostics.is_empty(),
            "VMatrix/GetTranslation-style any-or-false narrowing must preserve any and not report undefined-field"
        );
    }

    #[test]
    fn test_false_or_vmatrix_multi_return_guard_undefined_field_bug() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let library_root = ws.virtual_url_generator.new_path("__test_gmod_annotations");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("annotations.lua")).unwrap();
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
            ---@class VMatrix
            local VMatrix = {}
            function VMatrix:GetTranslation() end

            ---@class Entity
            local Entity = {}
            ---@return VMatrix?
            function Entity:GetBoneMatrix(bone) end
            ---@return integer?
            function Entity:LookupBone(name) end
            ---@return boolean
            function Entity:IsWorld() end

            function TOOL:HandEntity() end
            function TOOL:HandNum() end

            ---@param value any
            ---@return TypeGuard<any>
            ---@return_cast value -NULL
            function IsValid(value) end
            "#
                .to_string(),
            ),
        );

        let file_id = ws.def_file(
            "gamemodes/sandbox/entities/weapons/gmod_tool/stools/finger.lua",
            r#"
            function TOOL:GetHandPositions(pEntity)
                local LeftHand = pEntity:LookupBone("ValveBiped.Bip01_L_Hand")
                local RightHand = pEntity:LookupBone("ValveBiped.Bip01_R_Hand")

                if (!LeftHand || !RightHand) then return false end

                local LeftHandMatrix = pEntity:GetBoneMatrix(LeftHand)
                local RightHandMatrix = pEntity:GetBoneMatrix(RightHand)
                if (!LeftHandMatrix || !RightHandMatrix) then return false end

                return LeftHandMatrix, RightHandMatrix
            end

            function TOOL:DrawHUD()
                local selected = self:HandEntity()
                local hand = self:HandNum()

                if (!IsValid(selected)) then return end
                if (selected:IsWorld()) then return end

                local lefthand, righthand = self:GetHandPositions(selected)

                local BoneMatrix = lefthand
                if hand == 1 then BoneMatrix = righthand end
                if (!BoneMatrix) then return end

                BoneMatrix:GetTranslation()
            end
            "#,
        );

        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedField);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        assert!(
            diagnostics.is_empty(),
            "false-or-VMatrix guard should remove the false path before method lookup, got: {diagnostics:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Metadata-driven member guard tests (Phase 2)
    // -----------------------------------------------------------------------

    /// A custom guard function (not named `isfunction`) with `call_arg("gmod.member_guard", ...)`
    /// metadata should suppress undefined-field for its member-access argument.
    #[test]
    fn test_custom_annotated_member_guard_suppresses_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@meta
            ---@attribute call_arg(domain: string, role: string, priority: integer?)

            ---@[call_arg("gmod.member_guard", "function")]
            ---@param value any
            ---@return boolean
            function isCallable(value) end

            ---@class MyVehicle
            MyVehicle = {}
            "#,
        );

        // Use a non-conditional assignment context so that `in_conditional_statement` does NOT
        // fire.  The suppression must come from the `is_member_guard_call_argument` metadata
        // path, making this a load-bearing test for `call_arg("gmod.member_guard", ...)`.
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type MyVehicle
                local vehicle
                local result = isCallable(vehicle.GetFreeSeat)
            "#
        ));
    }

    /// An unannotated `isfunction` spelling (without `gmod.member_guard` metadata)
    /// should NOT suppress undefined-field when the member access is not in
    /// a conditional context.
    #[test]
    fn test_unannotated_isfunction_does_not_suppress_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param value any
            ---@return boolean
            function isfunction(value) end

            ---@class MyVehicle2
            MyVehicle2 = {}
            "#,
        );

        // Without member_guard metadata, isfunction should NOT suppress.
        // Use an assignment context (not conditional) to test the member guard path.
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@type MyVehicle2
                local vehicle
                local result = isfunction(vehicle.GetFreeSeat)
            "#
        ));
    }
}
