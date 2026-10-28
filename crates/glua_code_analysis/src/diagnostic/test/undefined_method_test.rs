#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};
    use glua_parser::{LuaAstNode, LuaAstToken};
    use lsp_types::{DiagnosticSeverity, NumberOrString};
    use tokio_util::sync::CancellationToken;

    #[test]
    fn metatable_index_resolves_methods_through_each_table_alternative() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_file_for(
            DiagnosticCode::UndefinedMethod,
            "lua/includes/modules/drivers.lua",
            r#"
            local first = {}
            function first:Init() return 1 end
            local second = {}
            function second:Init() return 2 end
            ---@param choose boolean
            local function build(choose)
                local base = choose and first or second
                local method = {}
                setmetatable(method, { __index = base })
                method:Init()
            end
            build(true)
            "#,
        ));
    }

    #[test]
    fn methods_installed_on_a_metatable_parameter_belong_to_its_class() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "lua/annotations/meta.lua",
            "---@meta\n---@class Panel\n---@generic T : table\n---@param metaName `T`\n---@return (definition) T\nfunction FindMetaTable(metaName) end\n",
        );
        let installer_path = "gamemodes/helix/gamemode/core/libs/sh_animation.lua";
        let installer = r#"
            ix = ix or {}
            ix.util = ix.util or {}
            function ix.util.InstallAnimationMethods(meta)
                function meta:CreateAnimation(length, data) end
            end
            local panelMeta = FindMetaTable("Panel")
            ix.util.InstallAnimationMethods(panelMeta)
        "#;
        let user_path = "gamemodes/helix/gamemode/core/derma/cl_notice.lua";
        let user =
            "---@param panel Panel\nlocal function Show(panel) panel:CreateAnimation(1, {}) end\n";
        ws.def_file(installer_path, installer);
        assert!(ws.check_file_for(DiagnosticCode::UndefinedMethod, user_path, user));
        let uri = ws.virtual_url_generator.new_uri(installer_path);
        ws.analysis.update_file_by_uri(&uri, None);
        assert!(!ws.check_file_for(DiagnosticCode::UndefinedMethod, user_path, user));
        ws.def_file(installer_path, installer);
        assert!(ws.check_file_for(DiagnosticCode::UndefinedMethod, user_path, user));
    }

    #[test]
    fn plugin_files_inherit_methods_from_the_table_the_loader_publishes() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let loader_path = "gamemode/core/libs/sh_plugin.lua";
        let loader = r#"
            ix = ix or {}
            ix.plugin = ix.plugin or {}
            function ix.plugin.Load(uniqueID, path, isSingleFile, variable)
                variable = variable or "PLUGIN"
                if (uniqueID == "schema") then
                    variable = "Schema"
                end
                local PLUGIN = {
                    folder = path,
                    uniqueID = uniqueID,
                }
                _G[variable] = PLUGIN
                function PLUGIN:SetData(value, global, ignoreMap) end
            end
        "#;
        let plugin_path = "plugins/3dpanel.lua";
        let plugin = "local PLUGIN = PLUGIN
function PLUGIN:SavePanels()
    self:SetData(self.list)
end
";
        ws.def_file(loader_path, loader);
        assert!(ws.check_file_for(DiagnosticCode::UndefinedMethod, plugin_path, plugin));
        let uri = ws.virtual_url_generator.new_uri(loader_path);
        ws.analysis.update_file_by_uri(&uri, None);
        assert!(!ws.check_file_for(DiagnosticCode::UndefinedMethod, plugin_path, plugin));
        ws.def_file(loader_path, loader);
        assert!(ws.check_file_for(DiagnosticCode::UndefinedMethod, plugin_path, plugin));
    }

    #[test]
    fn metatable_index_without_the_method_reports_it_undefined() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(!ws.check_file_for(
            DiagnosticCode::UndefinedMethod,
            "lua/includes/modules/drivers.lua",
            r#"
            local method = {}
            setmetatable(method, { __index = { Other = function() end } })
            method:Init()
            "#,
        ));
    }

    #[test]
    fn metatable_index_table_alternatives_refresh_after_edit_delete_reopen() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let path = "lua/includes/modules/drivers.lua";
        let definitions = r#"
            local first = {}
            function first:Init() return 1 end
            local second = {}
            function second:Init() return 2 end
            return { first = first, second = second }
        "#;
        let reader_path = "lua/includes/modules/drive_reader.lua";
        let reader = r#"
            local drivers = require("drivers")
            ---@param choose boolean
            local function build(choose)
                local base = choose and drivers.first or drivers.second
                local method = {}
                setmetatable(method, { __index = base })
                method:Init()
            end
            build(true)
        "#;
        ws.def_file(path, definitions);
        assert!(ws.check_file_for(DiagnosticCode::UndefinedMethod, reader_path, reader));
        ws.def_file(path, &definitions.replace("Init", "Other"));
        assert!(!ws.check_file_for(DiagnosticCode::UndefinedMethod, reader_path, reader));
        let uri = ws.virtual_url_generator.new_uri(path);
        ws.analysis.update_file_by_uri(&uri, None);
        ws.def_file(path, definitions);
        assert!(ws.check_file_for(DiagnosticCode::UndefinedMethod, reader_path, reader));
    }

    fn diagnostics(ws: &mut VirtualWorkspace, source: &str) -> Vec<lsp_types::Diagnostic> {
        let file_id = ws.def(source);
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
    }

    fn has_code(diagnostics: &[lsp_types::Diagnostic], code: DiagnosticCode) -> bool {
        let code = Some(NumberOrString::String(code.get_name().to_string()));
        diagnostics.iter().any(|diagnostic| diagnostic.code == code)
    }

    fn gmod_diagnostics(source: &str) -> Vec<lsp_types::Diagnostic> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        diagnostics(&mut ws, source)
    }

    #[test]
    fn gmod_name_fallback_method_reports_an_inferred_type_advisory() {
        let diagnostics = gmod_diagnostics(
            r#"
            ---@class Panel
            local function update_panel(panel)
                panel:MissingMethod()
            end
            "#,
        );

        assert!(
            has_code(&diagnostics, DiagnosticCode::InferUnknown),
            "infer-unknown diagnostic: {diagnostics:#?}"
        );
        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedField));
        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn explicit_parameter_type_keeps_undefined_method_error() {
        let diagnostics = gmod_diagnostics(
            r#"
            ---@class Panel
            local Panel = {}

            ---@param panel Panel
            local function update_panel(panel)
                panel:MissingMethod()
            end
            "#,
        );

        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .expect("undefined-method diagnostic");
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedField));
    }

    #[test]
    fn unknown_colon_call_reports_undefined_method_error_without_undefined_field() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@class MethodTest.Entity
            local Entity = {}

            ---@type MethodTest.Entity
            local entity
            entity:MissingMethod()
            "#,
        );

        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .expect("undefined-method diagnostic");
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.message, "Undefined method `MissingMethod`. ");
        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedField));
    }

    #[test]
    fn unknown_colon_call_in_condition_reports_undefined_method() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@class MethodTest.Conditional
            local Conditional = {}

            ---@type MethodTest.Conditional
            local value
            if value:MissingMethod() then
                print("unreachable")
            end
            "#,
        );

        assert!(has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn known_method_does_not_report_undefined_method() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@class MethodTest.Known
            local Known = {}
            function Known:PresentMethod() end

            ---@type MethodTest.Known
            local value
            value:PresentMethod()
            "#,
        );

        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn loop_body_write_does_not_type_its_own_iteration_variable() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def(
            r#"
            ---@class LoopWrite.Player
            local Player = {}
            function Player:Name() end

            local targets = {}
            for id, tgt in ipairs(targets) do
                targets[id] = string.format("%s", tgt:Name())
            end
            "#,
        );

        let stat = ws.get_node::<glua_parser::LuaForRangeStat>(file_id);
        let element_type = {
            let model = ws
                .analysis
                .compilation
                .get_semantic_model(file_id)
                .expect("semantic model");
            let element = stat
                .get_var_name_list()
                .nth(1)
                .expect("iteration value variable");
            model
                .get_semantic_info(element.syntax().clone().into())
                .expect("semantic info")
                .display_typ()
                .clone()
        };
        assert!(
            !element_type.is_string(),
            "loop body write leaked into its own element type: {element_type:?}"
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn explicit_table_key_type_allows_pairs_receiver_method() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@class Entity
            local Entity = {}
            function Entity:Remove() end

            ---@class RefundData
            ---@field units number
            ---@field cost number?
            ---@field reason number
            ---@field pumpType string|number
            ---@type table<Entity, RefundData>
            Registry = Registry or {}

            local registry = Registry
            for pump in pairs(registry) do
                pump:Remove()
            end
            "#,
        );

        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn short_circuit_guarded_optional_method_does_not_report() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@class MethodTest.Optional
            local Optional = {}

            ---@type MethodTest.Optional
            local value
            if value.OptionalMethod and value:OptionalMethod() then
                print("optional")
            end
            "#,
        );

        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn truthy_player_or_false_result_allows_player_methods() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field IsActive fun(self: Player): boolean
            ---@field Nick fun(self: Player): string

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@param value any
            ---@return TypeGuard<any>
            function IsValid(value) end

            local function IsPlayer(ent)
                return IsValid(ent) and ent:IsPlayer()
            end

            ---@return Entity
            function FindEntity() end

            local function FindPlayer()
                local ent = FindEntity()
                if not ent then return false end
                return (IsPlayer(ent) and ent:IsActive()) and ent or false
            end

            local target = FindPlayer()
            if target then
                target:Nick()
            end
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn gamemode_methods_defined_across_files_are_visible() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "annotations/gm.lua",
            r#"
            ---@class GM
            GM = {}
            ---@type GM
            GAMEMODE = nil
            "#,
        );
        let file_ids = ws.def_files(vec![
            (
                "gamemodes/terrortown/gamemode/cl_init.lua",
                r#"
                function GM:InitializeClient()
                    GAMEMODE:ClearClientState()
                end
                "#,
            ),
            (
                "gamemodes/terrortown/gamemode/client_state.lua",
                r#"
                function GM:ClearClientState() end
                "#,
            ),
        ]);

        let diagnostics = ws
            .analysis
            .diagnose_file(file_ids[0], CancellationToken::new())
            .unwrap_or_default();
        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn early_return_valid_player_guard_preserves_player_methods() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field IsActive fun(self: Player): boolean
            ---@field Nick fun(self: Player): string

            player = {}
            ---@return Player|false
            function player.GetBySteamID64(id) end

            ---@param value any
            ---@return TypeGuard<any>
            ---@return_cast value -NULL
            ---@[valid_guard]
            function IsValid(value) end

            ---@param ply Player
            local function Transfer(id, ply)
                local target = player.GetBySteamID64(id)
                if not IsValid(target) or not target:IsActive() or target == ply then return end
                target:Nick()
            end
            "#,
        );

        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn detached_entity_isvalid_guard_preserves_player_methods() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@attribute self_guard(member: string)

            ---@class Entity
            ---@class Player: Entity
            ---@class NULL: Entity

            Entity = {}
            Player = {}
            function Player:Name() end
            function Player:SteamID() end

            ---@return boolean
            ---@return_cast self Entity
            ---@[self_guard("gmod.entity")]
            function Entity:IsValid() end

            ---@generic T
            ---@param name `T`
            ---@return T
            function FindMetaTable(name) end

            local IsValid = FindMetaTable("Entity").IsValid
            ---@type Player|NULL
            local ply
            if IsValid(ply) then
                ply:Name()
                ply:SteamID()
            end
            "#,
        );

        assert!(
            !has_code(&diagnostics, DiagnosticCode::UndefinedMethod),
            "detached Entity:IsValid should preserve Player methods, got {diagnostics:?}"
        );
    }

    #[test]
    fn dynamic_callback_table_uses_call_site_argument_type() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def(
            r#"
            ---@class ScoreReport
            local ScoreReport = {}
            function ScoreReport:BuildSummaryPanel() end
            function ScoreReport:BuildEventLogPanel() end

            local tabs = {
                summary = function(panel)
                    panel:BuildSummaryPanel()
                end,
                events = function(panel)
                    panel:BuildEventLogPanel()
                end,
            }

            function ScoreReport:Show(selected)
                tabs[selected](self)
            end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn later_same_file_global_field_assign_in_other_function_types_earlier_read() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@class DForm: Panel
            ---@class ControlPanel: DForm
            local ControlPanel = {}
            function ControlPanel:Help(text) end
            function ControlPanel:Clear() end

            ---@param value any
            ---@return TypeGuard<any>
            ---@return_cast value -NULL
            ---@[valid_guard]
            function IsValid(value) end

            G = G or {}

            function G.Use()
                local panel = G.panel
                if not IsValid(panel) then return end
                panel:Help("x")
                panel:Clear()
            end

            ---@param panel ControlPanel
            function G.Init(panel)
                G.panel = panel
            end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        assert!(
            !has_code(&diagnostics, DiagnosticCode::UndefinedMethod),
            "cross-function later FileDefine should type earlier read, got {diagnostics:?}"
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let panel_local = semantic_model
            .get_root()
            .descendants::<glua_parser::LuaLocalName>()
            .find(|local_name| {
                local_name
                    .get_name_token()
                    .is_some_and(|token| token.get_name_text() == "panel")
            })
            .expect("panel local");
        let panel_ty = semantic_model
            .get_semantic_info(
                panel_local
                    .get_name_token()
                    .expect("name token")
                    .syntax()
                    .clone()
                    .into(),
            )
            .map(|info| info.display_typ().clone())
            .expect("panel type");
        let humanized = ws.humanize_type(panel_ty);
        assert!(
            humanized.contains("ControlPanel"),
            "expected ControlPanel for earlier cross-function read, got {humanized}"
        );
    }

    #[test]
    fn later_class_global_field_assign_from_buildcpanel_types_earlier_read() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        // Mirror real annotations: Help/Clear/Button on DForm, AddItem also on
        // other Panel children that must not steal the ControlPanel binding.
        let file_id = ws.def_file(
            "lua/weapons/gmod_tool/stools/glide_transmission_editor.lua",
            r#"
            ---@class Panel
            ---@field x number
            ---@field y number
            function Panel:Clear() end

            ---@class DCollapsibleCategory: Panel
            ---@class DForm: DCollapsibleCategory
            function DForm:Help(text) end
            function DForm:Clear() end
            function DForm:AddItem(left, right) end
            function DForm:Button(text) end

            ---@class ControlPanel: DForm
            ---@class ControlPresets: Panel
            function ControlPresets:AddItem(left, right) end
            ---@class DCheckBoxLabel: Panel
            function DCheckBoxLabel:AddItem(left, right) end
            ---@class DGrid: Panel
            function DGrid:AddItem(left, right) end
            ---@class DNotify: Panel
            function DNotify:AddItem(left, right) end

            ---@param value any
            ---@return TypeGuard<any>
            ---@return_cast value -NULL
            ---@[valid_guard]
            function IsValid(value) end

            ---@class Glide
            Glide = Glide or {}

            ---@class Tool
            ---@field BuildCPanel fun(panel: ControlPanel)
            ---@class TOOL: Tool
            TOOL = {}

            if not CLIENT then return end

            function Glide.RefreshTransmissionToolPanel()
                local panel = Glide.transmissionToolPanel
                if not IsValid(panel) then return end
                panel:Clear()
                panel:Help("desc")
                local row = panel
                panel:AddItem(row)
                panel:Button("add")
            end

            function TOOL.BuildCPanel(panel)
                Glide.transmissionToolPanel = panel
                Glide.RefreshTransmissionToolPanel()
            end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        assert!(
            !has_code(&diagnostics, DiagnosticCode::UndefinedMethod),
            "class-global later FileDefine from BuildCPanel should type earlier read, got {diagnostics:?}"
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let panel_local = semantic_model
            .get_root()
            .descendants::<glua_parser::LuaLocalName>()
            .find(|local_name| {
                local_name
                    .get_name_token()
                    .is_some_and(|token| token.get_name_text() == "panel")
            })
            .expect("panel local");
        let panel_ty = semantic_model
            .get_semantic_info(
                panel_local
                    .get_name_token()
                    .expect("name token")
                    .syntax()
                    .clone()
                    .into(),
            )
            .map(|info| info.display_typ().clone())
            .expect("panel type");
        let humanized = ws.humanize_type(panel_ty);
        assert!(
            humanized.contains("ControlPanel"),
            "expected ControlPanel for Glide.transmissionToolPanel read, got {humanized}"
        );
    }

    #[test]
    fn same_function_later_global_field_assign_stays_order_sensitive() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def(
            r#"
            G = G or {}

            function G.Use()
                A = G.field
                ---@type string
                G.field = "s"
                B = G.field
            end
            "#,
        );

        let before_ty = ws.expr_ty("A");
        let after_ty = ws.expr_ty("B");
        assert_ne!(
            ws.humanize_type(before_ty.clone()),
            ws.humanize_type(after_ty.clone()),
            "same-function later assign must not type earlier read as the later type; before={}, after={}",
            ws.humanize_type(before_ty),
            ws.humanize_type(after_ty)
        );
        assert_eq!(ws.humanize_type(after_ty), "string");
    }

    #[test]
    fn indexed_panel_parent_returns_known_parent_type_and_reports_unknown_methods() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class ParentPanel: Panel
            local ParentPanel = {}
            function ParentPanel:UpdatePlayerData() end
            function ParentPanel:CreateChild()
                return vgui.Create("ChildPanel", self)
            end
            function ParentPanel:CreateAddedChild()
                return self:Add("AddedChildPanel")
            end

            ---@class AlternateParentPanel: Panel
            local AlternateParentPanel = {}
            function AlternateParentPanel:UpdatePlayerData() end
            function AlternateParentPanel:CreateChild()
                return vgui.Create("ChildPanel", self)
            end

            ---@class ChildPanel: Panel
            local ChildPanel = {}
            function ChildPanel:UpdateParent()
                self:GetParent():UpdatePlayerData()
                self:GetParent():DefinitelyMissing()
            end

            ---@class AddedChildPanel: Panel
            local AddedChildPanel = {}
            function AddedChildPanel:UpdateParent()
                self:GetParent():UpdatePlayerData()
            end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let undefined_methods = diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();
        assert_eq!(undefined_methods, Vec::<&str>::new());
        let undefined_fields = diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedField.get_name().to_string(),
                    ))
            })
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();
        assert_eq!(undefined_fields, ["Undefined field `DefinitelyMissing`. "]);
    }

    /// Undefined methods reported when `PopulateFromList` is called as `callers` call it.
    fn installed_tree_method_reports(callers: &str) -> Vec<String> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "lua/annotations/meta.lua",
            "---@meta
---@class Panel
---@class DTree: Panel
---@class SpawnmenuContentPanel: Panel
",
        );
        ws.def_file(
            "gamemodes/sandbox/gamemode/spawnmenu/creationmenu/content/content.lua",
            r#"
            local PANEL = {}
            function PANEL:PopulateFromList(listName, tree, options)
                tree.Categories = {}
                tree.RefreshContent = function(self, item, spawnname) end
            end
            vgui.Register("SpawnmenuContentPanel", PANEL, "Panel")
            "#,
        );
        let file_id = ws.def_file(
            "gamemodes/sandbox/gamemode/spawnmenu/creationmenu/content/contenttypes/weapons.lua",
            &format!(
                r#"
            {callers}

            ---@type DTree
            local navTree
            navTree:RefreshContent({{}}, "weapon_crowbar")
            "#
            ),
        );
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .map(|diagnostic| diagnostic.message)
            .collect()
    }

    #[test]
    fn a_panel_method_installs_a_field_on_the_panel_its_callers_pass() {
        assert_eq!(
            installed_tree_method_reports(
                r#"
            ---@param pnlContent SpawnmenuContentPanel
            ---@param tree DTree
            local function populate(pnlContent, tree)
                pnlContent:PopulateFromList("Weapon", tree, {})
            end
            "#
            ),
            Vec::<String>::new()
        );
    }

    /// A forwarded vararg names no value, so the tree stays untyped.
    #[test]
    fn a_forwarded_vararg_leaves_an_installed_parameter_untyped() {
        assert_eq!(
            installed_tree_method_reports(
                r#"
            ---@param pnlContent SpawnmenuContentPanel
            ---@param tree DTree
            local function populate(pnlContent, tree)
                pnlContent:PopulateFromList("Weapon", tree, {})
            end

            ---@param pnlContent SpawnmenuContentPanel
            local function forward(pnlContent, ...)
                pnlContent:PopulateFromList("Weapon", ...)
            end
            "#
            ),
            ["Undefined method `RefreshContent`. "]
        );
    }

    /// A call through a value holding either handler types neither handler's parameter.
    #[test]
    fn a_pooled_handler_call_does_not_type_an_installed_parameter() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        assert!(!ws.check_file_for(
            DiagnosticCode::UndefinedMethod,
            "lua/autorun/handlers.lua",
            r#"
            ---@class DTree

            local Handlers = {}
            function Handlers.Mark(key, target)
                target.Marked = function() end
            end
            function Handlers.Other(key, target) end

            ---@param self DTree
            ---@param marks boolean
            local function run(self, marks)
                local handler = marks and Handlers.Mark or Handlers.Other
                handler("key", self)
            end

            ---@type DTree
            local tree
            tree:Marked()
            "#,
        ));
    }

    /// A call through a value holding one function types the parameter it installs on.
    #[test]
    fn an_aliased_installer_call_types_its_installed_parameter() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        assert!(ws.check_file_for(
            DiagnosticCode::UndefinedMethod,
            "lua/autorun/handlers.lua",
            r#"
            ---@class DTree

            local Handlers = {}
            function Handlers.Install(key, target)
                target.Marked = function() end
            end

            ---@param self DTree
            local function run(self)
                local handler = Handlers.Install
                handler("key", self)
            end

            ---@type DTree
            local tree
            tree:Marked()
            "#,
        ));
    }

    /// A named table passed to an installer gets the members it installs.
    #[test]
    fn a_named_table_passed_to_an_installer_gets_its_members() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        assert!(ws.check_file_for(
            DiagnosticCode::UndefinedMethod,
            "lua/autorun/install.lua",
            r#"
            function Install(target)
                function target:Marked() end
            end
            local tree = {}
            Install(tree)
            tree:Marked()
            "#,
        ));
    }

    #[test]
    fn vgui_parent_chain_resolves_add_panel_canvas_to_owner() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class PANEL: Panel
            PANEL = Panel
            local PANEL = {}
            function PANEL:EditorMethod() end
            function PANEL:Init()
                self.tabContainer = vgui.Create("DHorizontalScroller", self)
            end
            function PANEL:AddTab()
                local tab = {}
                tab.button = vgui.Create("StreamTabButton")
                self.tabContainer:AddPanel(tab.button)
            end
            vgui.Register("StreamEditor", PANEL, "Panel")

            ---@class StreamTab
            ---@field button StreamTabButton
            ---@class StreamTabButton: Panel
            ---@field GetParent fun(self: StreamTabButton): Panel
            local StreamTabButton = {}
            function StreamTabButton:UseEditor()
                self:GetParent():GetParent():GetParent():EditorMethod()
                self:GetParent():GetParent():GetParent():Missing()
            end

            vgui.Register("StreamTabButton", StreamTabButton, "Panel")
            "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert_eq!(
            metadata.get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("StreamTabButton")),
            Some(
                [
                    crate::LuaTypeDeclId::global("DDragBase"),
                    crate::LuaTypeDeclId::global("DHorizontalScroller"),
                    crate::LuaTypeDeclId::global("StreamEditor"),
                ]
                .as_slice()
            )
        );

        let undefined_methods = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>();
        assert_eq!(undefined_methods, ["Undefined method `Missing`. "]);
    }

    #[test]
    fn vgui_parent_call_through_member_receiver_is_collected_on_a_cold_build() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class DScrollPanel: Panel
            local PANEL = {}
            function PANEL:CardMethod() end
            function PANEL:Init()
                self.infoPanel = self:Add("DScrollPanel")
                local row = self.infoPanel:Add("cityrpListRow")
            end
            vgui.Register("cityrp_character_card", PANEL, "Panel")

            local ROW = {}
            function ROW:Think()
                self:GetParent():GetParent():CardMethod()
                self:GetParent():GetParent():Missing()
            end
            vgui.Register("cityrpListRow", ROW, "Panel")
            "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert_eq!(
            metadata.get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("cityrpListRow")),
            Some(
                [
                    crate::LuaTypeDeclId::global("DScrollPanel"),
                    crate::LuaTypeDeclId::global("cityrp_character_card"),
                ]
                .as_slice()
            )
        );

        let undefined_methods = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>();
        assert_eq!(undefined_methods, ["Undefined method `Missing`. "]);
    }

    #[test]
    fn test_vgui_focus_parent_chain_preserves_drag_base_methods_for_content_container() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class DDragBase: Panel
            ---@field GetParent fun(self: DDragBase): Panel
            ---@field GetReadOnly fun(self: DDragBase): boolean
            ---@class DHorizontalScroller: Panel
            ---@field GetParent fun(self: DHorizontalScroller): Panel
            ---@field pnlCanvas DDragBase

            ---@class ContentContainer: Panel
            local ContentContainer = {}
            function ContentContainer:CanModifyContents()
                self:GetParent():GetReadOnly()
                self:GetParent():Missing()
            end
            vgui.Register("ContentContainer", ContentContainer, "Panel")

            ---@class ContentOwner: Panel
            local ContentOwner = {}
            function ContentOwner:Init()
                self.contentContainer = vgui.Create("DHorizontalScroller", self)
            end
            function ContentOwner:AddContent()
                local content = vgui.Create("ContentContainer")
                self.contentContainer:AddPanel(content)
            end
            vgui.Register("ContentOwner", ContentOwner, "Panel")
            "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert_eq!(
            metadata.get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("ContentContainer")),
            Some(
                [
                    crate::LuaTypeDeclId::global("DDragBase"),
                    crate::LuaTypeDeclId::global("DHorizontalScroller"),
                    crate::LuaTypeDeclId::global("ContentOwner"),
                ]
                .as_slice()
            )
        );

        let undefined_methods = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>();
        assert_eq!(undefined_methods, ["Undefined method `Missing`. "]);
    }

    #[test]
    fn vgui_parent_chain_resolves_content_container_add_through_tile_layout() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "annotations/vgui.lua",
            r#"
            ---@meta
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class DDragBase: Panel
            ---@field GetReadOnly fun(self: DDragBase): boolean
            ---@class DTileLayout: DDragBase
        "#,
        );
        let content_container_path =
            "gamemodes/sandbox/gamemode/spawnmenu/creationmenu/content/contentcontainer.lua";
        let forwarding_helper = r#"
            local PANEL = {}

            function PANEL:Init()
                self.IconList = vgui.Create("DTileLayout")
            end

            function PANEL:Add(pnl)
                self.IconList:Add(pnl)
            end

            vgui.Register("ContentContainer", PANEL, "Panel")
        "#;
        ws.def_file(content_container_path, forwarding_helper);
        let header_file_id = ws.def_file(
            "gamemodes/sandbox/gamemode/spawnmenu/creationmenu/content/contentheader.lua",
            r#"
            local PANEL = {}

            function PANEL:OpenMenu()
                if self:GetParent().GetReadOnly then
                    self:GetParent():GetReadOnly()
                end
                self:GetParent():Missing()
            end

            vgui.Register("ContentHeader", PANEL, "Panel")
        "#,
        );
        let icon_file_id = ws.def_file(
            "gamemodes/sandbox/gamemode/spawnmenu/creationmenu/content/contenticon.lua",
            r#"
            local PANEL = {}

            function PANEL:OpenGenericSpawnmenuRightClickMenu()
                if self:GetParent().GetReadOnly then
                    self:GetParent():GetReadOnly()
                end
                self:GetParent():Missing()
            end

            vgui.Register("ContentIcon", PANEL, "Panel")
        "#,
        );
        ws.def_file(
            "gamemodes/sandbox/gamemode/spawnmenu/creationmenu/content/content.lua",
            r#"
            local container = vgui.Create("ContentContainer")
            local icon = vgui.Create("ContentIcon")
            container:Add(icon)
        "#,
        );
        ws.def_file(
            "gamemodes/sandbox/gamemode/spawnmenu/creationmenu/content/content_whitespace.lua",
            r#"
            local container = vgui.Create("ContentContainer")
            local header = vgui.Create("ContentHeader")
            container : Add(header)
        "#,
        );
        ws.def_file(
            "gamemodes/sandbox/gamemode/spawnmenu/creationmenu/content/content_decoy.lua",
            r#"
            local PANEL = {}
            local unrelated = {}
            local decoy = vgui.Create("ForwardingDecoy")
            local marker = ":Add"
            -- A matching comment may trigger the text prefilter but must not
            -- create a forwarding relation for an unrelated receiver.
            unrelated:Add(decoy)
            vgui.Register("ForwardingDecoy", PANEL, "Panel")
        "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert_eq!(
            metadata.get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("ContentHeader")),
            Some([crate::LuaTypeDeclId::global("DTileLayout")].as_slice())
        );
        assert_eq!(
            metadata.get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("ContentIcon")),
            Some([crate::LuaTypeDeclId::global("DTileLayout")].as_slice())
        );
        assert!(
            metadata
                .get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("ForwardingDecoy"))
                .is_none()
        );

        let undefined_methods = [header_file_id, icon_file_id]
            .into_iter()
            .flat_map(|file_id| {
                ws.analysis
                    .diagnose_file(file_id, CancellationToken::new())
                    .unwrap_or_default()
            })
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>();
        assert_eq!(
            undefined_methods,
            [
                "Undefined method `Missing`. ",
                "Undefined method `Missing`. "
            ]
        );

        ws.def_file(
            content_container_path,
            r#"
            local PANEL = {}

            function PANEL:Add(pnl) end

            vgui.Register("ContentContainer", PANEL, "Panel")
        "#,
        );
        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert!(
            metadata
                .get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("ContentHeader"))
                .is_none(),
            "forwarded parent metadata must be invalidated when the forwarding helper changes"
        );
        assert!(
            metadata
                .get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("ContentIcon"))
                .is_none(),
            "all callers of the changed forwarding helper must be invalidated"
        );

        ws.def_file(content_container_path, forwarding_helper);
        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert_eq!(
            metadata.get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("ContentHeader")),
            Some([crate::LuaTypeDeclId::global("DTileLayout")].as_slice()),
            "forwarded parent metadata must be restored when the helper is reopened"
        );
        assert_eq!(
            metadata.get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("ContentIcon")),
            Some([crate::LuaTypeDeclId::global("DTileLayout")].as_slice())
        );

        let content_container_uri = ws.virtual_url_generator.new_uri(content_container_path);
        ws.analysis
            .update_file_by_uri(&content_container_uri, None)
            .map(|(id, _)| id)
            .expect("forwarding helper file should exist");
        let parent_chains = |ws: &VirtualWorkspace| {
            ["ContentHeader", "ContentIcon"].map(|name| {
                ws.analysis
                    .compilation
                    .get_db()
                    .get_gmod_class_metadata_index()
                    .get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global(name))
                    .map(|chain| chain.to_vec())
            })
        };
        let after_deletion = parent_chains(&ws);
        for chain in &after_deletion {
            assert_eq!(
                chain.as_deref(),
                Some([crate::LuaTypeDeclId::global("ContentContainer")].as_slice()),
                "the inherited Panel:Add replaces the deleted forwarding override"
            );
        }
        ws.analysis.reindex();
        assert_eq!(parent_chains(&ws), after_deletion);
    }

    #[test]
    fn vgui_forwarding_uses_indexed_callers_across_edits_delete_and_reopen() {
        use crate::db_index::derivation::{DerivationId, DerivationKind, FactKey};

        let mut ws = VirtualWorkspace::new();
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "annotations/vgui.lua",
            "---@class Panel
---@class DTileLayout: Panel
---@class Child: Panel
---@class OtherChild: Panel",
        );
        ws.def_file(
            "gamemodes/sandbox/gamemode/cl_container.lua",
            r#"
            local PANEL = {}
            function PANEL:Init()
                self.Body = vgui.Create("DTileLayout")
            end
            function PANEL:Relay(pnl)
                self.Body:Add(pnl)
            end
            vgui.Register("Container", PANEL, "Panel")
            "#,
        );
        let unrelated = ws.def_file(
            "gamemodes/sandbox/gamemode/cl_unrelated.lua",
            "local marker = 'Relay'",
        );
        let caller_path = "gamemodes/sandbox/gamemode/cl_caller.lua";
        let caller = r#"
            local container = vgui.Create("Container")
            local child = vgui.Create("Child")
            local second = vgui.Create("OtherChild")
            container : Relay(child)
        "#;
        let child_id = crate::LuaTypeDeclId::global("Child");
        let chain = |ws: &VirtualWorkspace| {
            ws.analysis
                .compilation
                .get_db()
                .get_gmod_class_metadata_index()
                .get_vgui_panel_parent_chain(&child_id)
                .map(|chain| chain.to_vec())
        };
        assert_eq!(chain(&ws), None);
        ws.def_file(caller_path, caller);
        assert_eq!(
            chain(&ws),
            Some(vec![crate::LuaTypeDeclId::global("DTileLayout")])
        );
        let reads = ws
            .analysis
            .compilation
            .get_db()
            .reader_graph()
            .reads_of(DerivationId::workspace(DerivationKind::VguiChains))
            .map(|key| key.key())
            .collect::<Vec<_>>();
        assert!(
            !reads
                .iter()
                .any(|key| matches!(key, FactKey::FileText(file_id) if *file_id == unrelated))
        );
        assert!(reads.contains(&&FactKey::IndexReferenceFiles("Relay".into())));
        assert!(!reads.contains(&&FactKey::ReferencesByName("Relay".into())));
        ws.def_file(
            "gamemodes/sandbox/gamemode/cl_unrelated.lua",
            "local marker = 'Other'",
        );
        assert_eq!(
            chain(&ws),
            Some(vec![crate::LuaTypeDeclId::global("DTileLayout")])
        );
        assert_eq!(
            ws.analysis
                .compilation
                .last_stats()
                .evaluations(DerivationKind::VguiChains),
            0
        );
        let reference_files_key = FactKey::IndexReferenceFiles("Relay".into());
        let membership = ws
            .analysis
            .compilation
            .get_db()
            .fact_fingerprint(&reference_files_key);
        ws.def_file(
            caller_path,
            &caller.replace("Relay(child)", "Relay(second)"),
        );
        assert_eq!(
            ws.analysis
                .compilation
                .get_db()
                .fact_fingerprint(&reference_files_key),
            membership
        );
        assert!(
            ws.analysis
                .compilation
                .last_stats()
                .evaluations(DerivationKind::VguiChains)
                > 0
        );
        assert_eq!(chain(&ws), None);
        assert_eq!(
            ws.analysis
                .compilation
                .get_db()
                .get_gmod_class_metadata_index()
                .get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("OtherChild")),
            Some([crate::LuaTypeDeclId::global("DTileLayout")].as_slice())
        );
        ws.def_file(caller_path, "local marker = 'Relay'");
        assert_eq!(chain(&ws), None);
        ws.def_file(caller_path, caller);
        assert_eq!(
            chain(&ws),
            Some(vec![crate::LuaTypeDeclId::global("DTileLayout")])
        );
        let uri = ws.virtual_url_generator.new_uri(caller_path);
        ws.analysis.update_file_by_uri(&uri, None);
        assert_eq!(chain(&ws), None);
        ws.def_file(caller_path, caller);
        let reopened = chain(&ws);
        assert_eq!(
            reopened,
            Some(vec![crate::LuaTypeDeclId::global("DTileLayout")])
        );
        ws.analysis.reindex();
        assert_eq!(chain(&ws), reopened);
    }

    #[test]
    fn vgui_parent_chain_resolves_create_parent_field_assignment() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class DPanel: Panel

            ---@class TabButton: Panel
            local TabButton = {}
            function TabButton:Click()
                self:GetParent():GetParent():SetActiveTab()
                self:GetParent():GetParent():Missing()
            end
            vgui.Register("TabButton", TabButton, "Panel")

            ---@class TabbedFrame: Panel
            local TabbedFrame = {}
            function TabbedFrame:SetActiveTab() end
            function TabbedFrame:Init()
                self.tabList = vgui.Create("DPanel", self)
            end
            function TabbedFrame:AddTab()
                local button = vgui.Create("TabButton", self.tabList)
            end
            vgui.Register("TabbedFrame", TabbedFrame, "Panel")
            "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert_eq!(
            metadata.get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("TabButton")),
            Some(
                [
                    crate::LuaTypeDeclId::global("DPanel"),
                    crate::LuaTypeDeclId::global("TabbedFrame"),
                ]
                .as_slice()
            )
        );

        let undefined_methods = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>();
        assert_eq!(undefined_methods, ["Undefined method `Missing`. "]);
    }

    #[test]
    fn vgui_parent_chain_rejects_disagreeing_create_parent_field_owners() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.def(
            r#"
            ---@class Panel
            ---@class DPanel: Panel

            ---@class TabButton: Panel
            local TabButton = {}
            vgui.Register("TabButton", TabButton, "Panel")

            ---@class OwnerA: Panel
            local OwnerA = {}
            function OwnerA:Init()
                self.tabList = vgui.Create("DPanel", self)
            end
            function OwnerA:AddTab()
                local button = vgui.Create("TabButton", self.tabList)
            end
            vgui.Register("OwnerA", OwnerA, "Panel")

            ---@class OwnerB: Panel
            local OwnerB = {}
            function OwnerB:Init()
                self.tabList = vgui.Create("DPanel", self)
            end
            function OwnerB:AddTab()
                local button = vgui.Create("TabButton", self.tabList)
            end
            vgui.Register("OwnerB", OwnerB, "Panel")
            "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert!(
            metadata
                .get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("TabButton"))
                .is_none()
        );
    }

    #[test]
    fn vgui_parent_chain_does_not_use_another_scroller_owner() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let _file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class OwnerA: Panel
            local OwnerA = {}
            function OwnerA:OwnerAMethod() end
            ---@param externalOwner Panel
            function OwnerA:Init(externalOwner)
                self.tabContainer = vgui.Create("DHorizontalScroller", externalOwner)
                self.otherScroller = vgui.Create("DHorizontalScroller", self)
            end
            ---@param child Child
            function OwnerA:AddChild(child)
                self.tabContainer:AddPanel(child)
            end

            ---@class Child: Panel
            local Child = {}
            function Child:UseParent()
                self:GetParent():GetParent():GetParent():OwnerAMethod()
            end
            vgui.Register("Child", Child, "Panel")
            "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        let child = crate::LuaTypeDeclId::global("Child");
        assert_eq!(
            metadata.get_vgui_panel_parent_chain(&child),
            Some(
                [
                    crate::LuaTypeDeclId::global("DDragBase"),
                    crate::LuaTypeDeclId::global("DHorizontalScroller"),
                    crate::LuaTypeDeclId::global("Panel"),
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn vgui_parent_chain_rejects_disagreeing_owners() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let _file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class OwnerA: Panel
            local OwnerA = {}
            function OwnerA:OnlyOwnerA() end
            function OwnerA:MakeChild()
                return vgui.Create("SharedChild", self)
            end

            ---@class OwnerB: Panel
            local OwnerB = {}
            function OwnerB:MakeChild()
                return vgui.Create("SharedChild", self)
            end

            ---@class SharedChild: Panel
            local SharedChild = {}
            function SharedChild:UseParent()
                self:GetParent():OnlyOwnerA()
            end
            "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert!(
            metadata
                .get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("SharedChild"))
                .is_none()
        );
    }

    #[test]
    fn vgui_parent_chain_supports_typed_set_parent_and_fails_closed_at_depth() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let _file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class TypedOwner: Panel
            ---@field GetParent fun(self: TypedOwner): Panel
            local TypedOwner = {}
            function TypedOwner:OwnerMethod() end

            ---@class TypedChild: Panel
            ---@field GetParent fun(self: TypedChild): Panel
            local TypedChild = {}
            ---@param owner TypedOwner
            function TypedChild:Attach(owner)
                local child = self
                child:SetParent(owner)
            end
            function TypedChild:UseOwner()
                self:GetParent():OwnerMethod()
                self:GetParent():GetParent():OwnerMethod()
            end

            vgui.Register("TypedChild", TypedChild, "Panel")
            "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert_eq!(
            metadata.get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("TypedChild")),
            Some([crate::LuaTypeDeclId::global("TypedOwner")].as_slice())
        );
    }

    #[test]
    fn vgui_parent_chain_marks_omitted_set_parent_incomplete() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let _file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class Child: Panel
            local Child = {}
            function Child:Detach()
                self:SetParent()
            end
            vgui.Register("Child", Child, "Panel")
            "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        let child = crate::LuaTypeDeclId::global("Child");
        assert!(metadata.get_vgui_panel_parent_chain(&child).is_none());
        assert!(!metadata.vgui_panel_parent_chain_is_complete(&child));
    }

    #[test]
    fn unresolved_vgui_parent_method_reports_undefined_field_warning() {
        let diagnostics = gmod_diagnostics(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class CustomPanel: Panel
            local PANEL = {}
            function PANEL:Test()
                local parent = self:GetParent()
                parent:MissingMethod()
            end
            "#,
        );

        let diagnostic = diagnostics
            .iter()
            .find(|d| {
                d.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedField.get_name().to_string(),
                    ))
            })
            .unwrap_or_else(|| panic!("expected undefined-field diagnostic: {diagnostics:#?}"));
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::WARNING));
        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn starfall_shaped_unresolved_parent_method_is_warning() {
        let diagnostics = gmod_diagnostics(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class StarfallEditorTab: Panel
            local PANEL = {}
            function PANEL:GetParent()
                return self.ParentTab
            end
            function PANEL:Init()
                self:GetParent():GetNumTabs()
                self:GetParent():SetActiveTabIndex(1)
            end
            "#,
        );

        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
        assert!(has_code(&diagnostics, DiagnosticCode::UndefinedField));
    }

    #[test]
    fn chained_unresolved_vgui_parent_is_warning() {
        let diagnostics = gmod_diagnostics(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class CustomSubPanel: Panel
            local PANEL = {}
            function PANEL:Test()
                self:GetParent():GetParent():SomethingMissing()
            end
            "#,
        );

        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
        assert!(has_code(&diagnostics, DiagnosticCode::UndefinedField));
    }

    #[test]
    fn concrete_indexed_vgui_parent_remains_strict_error() {
        let diagnostics = gmod_diagnostics(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class ParentPanel: Panel
            local ParentPANEL = {}
            function ParentPANEL:KnownParentMethod() end
            vgui.Register("ParentPanel", ParentPANEL, "Panel")

            ---@class ChildPanel: Panel
            local ChildPANEL = {}
            function ChildPANEL:Init()
                local parent = vgui.Create("ParentPanel")
                self:SetParent(parent)
                self:GetParent():DefinitelyMissing()
            end
            vgui.Register("ChildPanel", ChildPANEL, "Panel")
            "#,
        );

        assert!(has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
        let diagnostic = diagnostics
            .iter()
            .find(|d| {
                d.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .unwrap();
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn explicit_concrete_return_annotation_remains_strict_error() {
        let diagnostics = gmod_diagnostics(
            r#"
            ---@class Panel
            ---@class StarfallEditorFrame: Panel
            function StarfallEditorFrame:KnownFrameMethod() end

            ---@class AnnotatedPanel: Panel
            local PANEL = {}
            ---@return StarfallEditorFrame
            function PANEL:GetParent() end

            function PANEL:Test()
                local parent = self:GetParent()
                parent:DefinitelyMissing()
            end
            "#,
        );

        assert!(has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
        let diagnostic = diagnostics
            .iter()
            .find(|d| {
                d.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .unwrap();
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn entity_get_parent_is_unaffected() {
        let diagnostics = gmod_diagnostics(
            r#"
            ---@class Entity
            ---@field GetParent fun(self: Entity): Entity
            local function test(ent)
                ---@cast ent Entity
                local parent = ent:GetParent()
                parent:MissingEntityMethod()
            end
            "#,
        );

        assert!(has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
        let diagnostic = diagnostics
            .iter()
            .find(|d| {
                d.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .unwrap();
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
    }

    /// A nil check on a call result speaks for the value, not for the method
    /// name that produced it, so it cannot excuse an undefined method.
    #[test]
    fn nil_check_of_the_call_result_does_not_excuse_the_method() {
        let diagnostics = gmod_diagnostics(
            r#"
            ---@class Entity
            ---@class Holder
            ---@field owner Entity
            ---@type Holder
            local holder = nil
            local trace = holder.owner:MissingEntityMethod()
            if trace.Entity then print(1) end
            "#,
        );

        assert!(has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    /// `obj.method and obj:method()` tests the member itself before calling it,
    /// which is a presence check the diagnostic must respect.
    #[test]
    fn short_circuit_presence_check_excuses_the_call_it_guards() {
        let diagnostics = gmod_diagnostics(
            r#"
            ---@class Entity
            ---@type Entity
            local ent = nil
            local owned = ent.CPPIGetOwner and ent:CPPIGetOwner() == nil
            print(owned)
            "#,
        );

        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    /// A table a function fills fields into is that caller's input, not the parameter's type.
    #[test]
    fn a_registered_table_does_not_close_the_input_it_fills() {
        for table in ["CLASS", "{ format = \"x\" }", "self"] {
            let mut ws = VirtualWorkspace::new();
            let mut emmyrc = Emmyrc::default();
            emmyrc.gmod.enabled = true;
            ws.update_emmyrc(emmyrc);
            assert!(
                ws.check_file_for(
                    DiagnosticCode::UndefinedField,
                    "lua/autorun/register.lua",
                    &format!(
                        r#"
            function Register(name, data)
                data.uniqueID = name
                return {{ bNoIndicator = data.bNoIndicator }}
            end
            local CLASS = {{}}
            local self = {{}}
            Register("chat", {table})
            "#
                    ),
                ),
                "{table}"
            );
        }
    }

    const CLASS_NAME_ANNOTATIONS: &str = r#"
        ---@meta
        ---@class Entity
        local Entity = {}
        ---@return string
        function Entity:GetClass() end
        ---@class farmplot : Entity
        local farmplot = {}
        function farmplot:GetPlotSize() end
        ---@class composter : Entity
        local composter = {}
        function composter:GetComposting() end
        ---@class plant : Entity
        local plant = {}
        function plant:GetGrowth() end
        ---@class ground_plant : plant
        local ground_plant = {}
        ---@class TraceResult
        ---@field Entity Entity
        local TraceResult = {}
    "#;

    fn class_name_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_file("lua/annotations/entity.lua", CLASS_NAME_ANNOTATIONS);
        ws
    }

    /// `local class = ent:GetClass()` compared with a class name narrows `ent` as the call itself would.
    #[test]
    fn a_class_name_local_narrows_the_entity_it_was_read_from() {
        let source = r#"
            ---@param trace TraceResult
            local function draw(trace)
                local class = trace.Entity:GetClass()
                if class == "farmplot" then
                    print(trace.Entity:GetPlotSize())
                elseif "composter" == class then
                    print(trace.Entity:GetComposting())
                elseif (class == "plant" or class == "ground_plant") then
                    print(trace.Entity:GetGrowth())
                end
            end
            print(draw)
        "#;
        for code in [
            DiagnosticCode::UndefinedMethod,
            DiagnosticCode::InferUnguardedChild,
        ] {
            let mut ws = class_name_workspace();
            assert!(
                ws.check_file_for(code, "lua/autorun/client/cl_draw.lua", source),
                "{code:?}"
            );
        }
    }

    /// The class name a local holds stops describing the entity once either is reassigned, and
    /// says nothing inside a closure that may run later.
    #[test]
    fn a_class_name_local_does_not_narrow_past_a_write_or_into_a_closure() {
        for body in [
            "local class = trace.Entity:GetClass()\ntrace.Entity = other\nif class == \"composter\" then print(trace.Entity:GetComposting()) end",
            "local class = trace.Entity:GetClass()\ntrace = { Entity = other }\nif class == \"composter\" then print(trace.Entity:GetComposting()) end",
            "local class = trace.Entity:GetClass()\nclass = other:GetClass()\nif class == \"composter\" then print(trace.Entity:GetComposting()) end",
            "local class = trace.Entity:GetClass()\nlocal function later() if class == \"composter\" then print(trace.Entity:GetComposting()) end end\nlater()",
        ] {
            let mut ws = class_name_workspace();
            let source = format!(
                "---@param trace TraceResult\n---@param other Entity\nlocal function draw(trace, other)\n{body}\nend\nprint(draw)\n"
            );
            assert!(
                !ws.check_file_for(
                    DiagnosticCode::InferUnguardedChild,
                    "lua/autorun/client/cl_draw.lua",
                    &source,
                ),
                "{body}"
            );
        }
    }

    /// A missing method a test reads for presence may be installed at run time, so the read is
    /// unknown rather than nil; a plain read of it stays nil.
    #[test]
    fn a_presence_tested_missing_method_reads_as_unknown() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let file = ws.def_file(
            "lua/autorun/server/sv_car.lua",
            r#"
            ---@class Car
            local Car = {}
            ---@param hasPhoton boolean
            function Car:OnSeatInput(hasPhoton)
                local controller = hasPhoton and self.GetPhotonController and self:GetPhotonController()
                local plain = self.GetPhotonController
                print(controller, plain)
            end
            "#,
        );
        let controller = ws.humanize_type(ws.local_ty(file, "controller"));
        assert!(
            controller.contains("unknown") && !controller.contains("nil"),
            "{controller}"
        );
        assert_eq!(ws.humanize_type(ws.local_ty(file, "plain")), "nil");
    }

    /// Methods a caller defines on the table a factory returns are members of that table,
    /// which the factory's own methods may call; a class instance's other instances
    /// do not gain them.
    #[test]
    fn a_method_defined_on_a_factory_result_belongs_to_the_table_it_made() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let factory_path = "lua/includes/util/workshop_files.lua";
        let factory = r#"
            function WorkshopFileBase(namespace)
                local ret = {}
                function ret:Fetch(kind)
                    if kind == "local" then return self:FetchLocal(1) end
                    if kind == "dot" then return self.FetchDot() end
                    return self:FetchOther()
                end
                return ret
            end
        "#;
        let consumer_path = "lua/menu/menu_save.lua";
        let consumer = r#"
            ws_save = WorkshopFileBase("save")
            function ws_save:FetchLocal(offset) return offset end
            function ws_save.FetchDot() return 1 end
            local ws_dupe = WorkshopFileBase("dupe")
            function ws_dupe:FetchOther() return 2 end
        "#;
        ws.def_file(consumer_path, consumer);
        for code in [
            DiagnosticCode::UndefinedMethod,
            DiagnosticCode::UndefinedField,
        ] {
            assert!(ws.check_file_for(code, factory_path, factory), "{code:?}");
        }

        let uri = ws.virtual_url_generator.new_uri(consumer_path);
        ws.analysis.update_file_by_uri(&uri, None);
        assert!(!ws.check_file_for(DiagnosticCode::UndefinedMethod, factory_path, factory));
        ws.def_file(consumer_path, consumer);
        assert!(ws.check_file_for(DiagnosticCode::UndefinedMethod, factory_path, factory));

        ws.def_file(
            "lua/annotations/panel.lua",
            "---@meta\n---@class Panel\nlocal Panel = {}\n---@return Panel\nfunction CreatePanel() end\n",
        );
        assert!(!ws.check_file_for(
            DiagnosticCode::UndefinedMethod,
            "lua/autorun/client/cl_panel.lua",
            "local a = CreatePanel()\nfunction a:Custom() end\n---@param b Panel\nlocal function Use(b) b:Custom() end\nprint(Use)\n",
        ));
    }

    #[test]
    fn method_on_a_class_that_does_not_exist_is_left_to_type_not_found() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            ---@return Missing
            local function get() end

            get():Update()
            "#,
        ));
    }
}
