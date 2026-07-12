#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};
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
        assert_eq!(
            undefined_methods,
            ["Undefined method `DefinitelyMissing`. "]
        );
    }
}
