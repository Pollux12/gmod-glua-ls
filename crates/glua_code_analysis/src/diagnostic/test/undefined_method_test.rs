#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, LuaType, TypeVisitTrait, VirtualWorkspace};
    use glua_parser::{LuaAstNode, LuaAstToken};
    use lsp_types::{DiagnosticSeverity, NumberOrString};
    use tokio_util::sync::CancellationToken;

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

    fn contains_named_type(typ: &LuaType, name: &str) -> bool {
        let mut found = false;
        typ.visit_type(&mut |candidate| {
            if let LuaType::Ref(id) | LuaType::Def(id) = candidate
                && id.get_simple_name() == name
            {
                found = true;
            }
        });
        found
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
    fn gmod_name_fallback_method_reports_undefined_field_warning() {
        let diagnostics = gmod_diagnostics(
            r#"
            ---@class Panel
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
                        DiagnosticCode::UndefinedField.get_name().to_string(),
                    ))
            })
            .unwrap_or_else(|| panic!("undefined-field diagnostic: {diagnostics:#?}"));
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::WARNING));
        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn explicit_parameter_type_keeps_undefined_method_error() {
        let diagnostics = gmod_diagnostics(
            r#"
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
    fn real_glide_transmission_tool_panel_help_is_defined() {
        use std::path::PathBuf;

        let annotations = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../annotations-gmod-glua-ls/output");
        let vehicle_base =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../cityrp-vehicle-base");
        let stool = vehicle_base.join("lua/weapons/gmod_tool/stools/glide_transmission_editor.lua");
        let glide_autorun = vehicle_base.join("lua/autorun/sh_glide.lua");
        if !annotations.is_dir() || !stool.is_file() || !glide_autorun.is_file() {
            // Adjacent checkouts are optional on CI; unit fixtures cover the rule.
            return;
        }

        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis.add_library_workspace(annotations.clone());

        // Load a representative subset of panel/tool annotations so the test stays
        // bounded while still using real hierarchy + Help/BuildCPanel signatures.
        for name in [
            "panel.lua",
            "dcollapsiblecategory.lua",
            "dform.lua",
            "controlpanel.lua",
            "controlpresets.lua",
            "dcheckboxlabel.lua",
            "dgrid.lua",
            "dnotify.lua",
            "tool.lua",
            "global.lua",
        ] {
            let path = annotations.join(name);
            if !path.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read annotation");
            let uri = lsp_types::Uri::parse_from_file_path(&path).expect("uri");
            ws.analysis.update_file_by_uri(&uri, Some(text));
        }

        let glide_text = std::fs::read_to_string(&glide_autorun).expect("read glide");
        let glide_uri = lsp_types::Uri::parse_from_file_path(&glide_autorun).expect("glide uri");
        ws.analysis.update_file_by_uri(&glide_uri, Some(glide_text));

        let stool_text = std::fs::read_to_string(&stool).expect("read stool");
        let stool_uri = lsp_types::Uri::parse_from_file_path(&stool).expect("stool uri");
        let file_id = ws
            .analysis
            .update_file_by_uri(&stool_uri, Some(stool_text))
            .expect("stool file id");

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");

        let mut panel_local_types = Vec::new();
        let mut panel_locals = Vec::new();
        for local_name in semantic_model
            .get_root()
            .descendants::<glua_parser::LuaLocalName>()
        {
            if local_name
                .get_name_token()
                .is_some_and(|token| token.get_name_text() == "panel")
            {
                let ty = semantic_model
                    .get_semantic_info(
                        local_name
                            .get_name_token()
                            .expect("name token")
                            .syntax()
                            .clone()
                            .into(),
                    )
                    .map(|info| info.display_typ().clone())
                    .unwrap_or(LuaType::Unknown);
                panel_locals.push(ws.humanize_type(ty.clone()));
                panel_local_types.push(ty);
            }
        }

        let field_types: Vec<String> = semantic_model
            .get_root()
            .descendants::<glua_parser::LuaIndexExpr>()
            .filter(|index| format!("{}", index.syntax().text()).contains("transmissionToolPanel"))
            .filter_map(|index| {
                semantic_model
                    .infer_expr(glua_parser::LuaExpr::IndexExpr(index))
                    .ok()
                    .map(|ty| ws.humanize_type(ty))
            })
            .collect();
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let help_undefined = diagnostics.iter().any(|diagnostic| {
            diagnostic.code
                == Some(NumberOrString::String(
                    DiagnosticCode::UndefinedMethod.get_name().to_string(),
                ))
                && diagnostic.message.contains("Help")
        });
        assert!(
            !help_undefined,
            "real glide transmission editor must not report undefined-method Help; panel_locals={panel_locals:?}; field_types={field_types:?}; diagnostics={diagnostics:?}"
        );
        assert!(
            panel_local_types
                .iter()
                .any(|ty| contains_named_type(ty, "ControlPanel")),
            "expected ControlPanel for local panel from Glide.transmissionToolPanel, got panel_locals={panel_locals:?}; field_types={field_types:?}"
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
            .expect("forwarding helper file should exist");
        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert!(
            metadata
                .get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("ContentHeader"))
                .is_none(),
            "forwarded parent metadata must be invalidated when the helper is deleted"
        );
        assert!(
            metadata
                .get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("ContentIcon"))
                .is_none()
        );
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
    fn stream_editor_tab_button_resolves_parent_chain() {
        use std::path::PathBuf;

        let annotations = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../annotations-gmod-glua-ls/output");
        let vehicle_base =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../cityrp-vehicle-base");
        let stream_editor = vehicle_base.join("lua/glide/client/vgui/stream_editor.lua");
        if !annotations.is_dir() || !stream_editor.is_file() {
            return;
        }

        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.analysis.add_library_workspace(annotations.clone());
        ws.analysis.add_main_workspace(vehicle_base.clone());

        let mut files = Vec::new();
        for entry in std::fs::read_dir(&annotations).expect("read annotations") {
            let path = entry.expect("read annotation entry").path();
            if path.extension().is_none_or(|extension| extension != "lua") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read annotation");
            files.push((path, Some(text)));
        }
        files.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        let vehicle_files =
            crate::load_workspace_files(&vehicle_base, &["**/*.lua".to_string()], &[], &[], None)
                .expect("read vehicle base");
        files.extend(
            vehicle_files
                .into_iter()
                .map(crate::LuaFileInfo::into_tuple),
        );
        ws.analysis.update_files_by_path(files);
        let uri = lsp_types::Uri::parse_from_file_path(&stream_editor).expect("stream editor uri");
        let file_id = ws
            .analysis
            .get_file_id(&uri)
            .expect("stream editor file id");

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert_eq!(
            metadata.get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global(
                "Styled_StreamEditorTabButton",
            )),
            Some(
                [
                    crate::LuaTypeDeclId::global("DDragBase"),
                    crate::LuaTypeDeclId::global("DHorizontalScroller"),
                    crate::LuaTypeDeclId::global("Glide_EngineStreamEditor"),
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
        assert!(
            !undefined_methods.iter().any(|message| {
                message.contains("SetActiveTabById") || message.contains("CloseTabById")
            }),
            "expected the tab button parent chain to reach Glide_EngineStreamEditor, got {undefined_methods:?}"
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
}
