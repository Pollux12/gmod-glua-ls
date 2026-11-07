#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    fn gmod_workspace(annotations: &str) -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.def_file("lua/annotations/meta.lua", annotations);
        ws.enable_full_diagnostic();
        ws
    }

    /// The messages `code` reports in the file `path` defines as `content`.
    fn messages(
        ws: &mut VirtualWorkspace,
        code: DiagnosticCode,
        path: &str,
        content: &str,
    ) -> Vec<String> {
        let file_id = ws.def_file(path, content);
        let expected = Some(NumberOrString::String(code.get_name().to_string()));
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| diagnostic.code == expected)
            .map(|diagnostic| diagnostic.message)
            .collect()
    }

    const CONTROL_TABLE_API: &str = r#"
        ---@meta
        ---@class Panel
        ---@field SetVisible fun(self: Panel, visible: boolean)
        ---@field Remove fun(self: Panel)
        ---@class DHTML: Panel
        ---@generic T : table
        ---@param Panelname `T`
        ---@return (definition) `T`?
        function vgui.GetControlTable(Panelname) end
    "#;

    const CONTROL_TABLE_EXTENSION: &str = r#"
        local PANEL = vgui.GetControlTable("DHTML")
        function PANEL:RemoveWhileHidden()
            self:SetVisible(false)
            self:Remove()
        end
        local htmlPanel = vgui.Create("DHTML")
        htmlPanel:RemoveWhileHidden()
    "#;

    #[test]
    fn method_added_through_control_table_is_seen_on_created_panels() {
        let mut ws = gmod_workspace(CONTROL_TABLE_API);
        let undefined = messages(
            &mut ws,
            DiagnosticCode::UndefinedMethod,
            "gamemodes/test/gamemode/cl_test9.lua",
            CONTROL_TABLE_EXTENSION,
        );
        assert_eq!(undefined, Vec::<String>::new());
    }

    #[test]
    fn control_table_of_a_declared_class_is_not_nil() {
        let mut ws = gmod_workspace(CONTROL_TABLE_API);
        let nil_checks = messages(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            "gamemodes/test/gamemode/cl_test9.lua",
            CONTROL_TABLE_EXTENSION,
        );
        assert_eq!(nil_checks, Vec::<String>::new());
    }

    #[test]
    fn control_table_of_an_undeclared_name_may_be_nil() {
        let mut ws = gmod_workspace(CONTROL_TABLE_API);
        let nil_checks = messages(
            &mut ws,
            DiagnosticCode::NeedCheckNil,
            "gamemodes/test/gamemode/cl_missing.lua",
            r#"
            local PANEL = vgui.GetControlTable("NoSuchPanel")
            PANEL.Extra = true
            "#,
        );
        assert_eq!(nil_checks, vec!["PANEL may be nil".to_string()]);
    }

    #[test]
    fn control_table_of_several_declared_classes_is_an_open_non_nil_table() {
        let mut ws = gmod_workspace(
            r#"
            ---@meta
            ---@class Panel
            ---@class DHTML: Panel
            ---@field SubMenuArrow Panel?
            ---@class DPanel: Panel
            ---@field DropButton Panel?
            ---@generic T : table
            ---@param Panelname `T`
            ---@return (definition) `T`?
            function vgui.GetControlTable(Panelname) end
            "#,
        );
        let path = "gamemodes/test/gamemode/cl_overrides.lua";
        let code = r#"
            local PANEL = {}
            local function OverridePanel(name, func)
                PANEL = vgui.GetControlTable(name)
                func()
            end
            OverridePanel("DHTML", function()
                function PANEL:PerformLayout()
                    self:CenterVertical()
                    return self.SubMenuArrow
                end
            end)
            OverridePanel("DPanel", function()
                function PANEL:OpenMenu()
                    return self.DropButton
                end
            end)
        "#;
        assert_eq!(
            messages(&mut ws, DiagnosticCode::NeedCheckNil, path, code),
            Vec::<String>::new()
        );
        assert_eq!(
            messages(&mut ws, DiagnosticCode::UndefinedField, path, code),
            Vec::<String>::new()
        );
        assert_eq!(
            messages(&mut ws, DiagnosticCode::UndefinedMethod, path, code),
            Vec::<String>::new()
        );
    }

    const TABS_API: &str = r#"
        ---@meta
        ---@class Panel
        ---@field GetWide fun(self: Panel): number
        ---@class DPanel: Panel
        ---@class Label: Panel
        ---@field SetFont fun(self: Label, font: string)
        ---@class DLabel: Label
        ---@class DTextEntry: Panel
        ---@field SetFont fun(self: DTextEntry, font: string)
        ---@class DCheckBoxLabel: Panel
        ---@field SetFont fun(self: DCheckBoxLabel, font: string)
        ---@class EditablePanel: Panel
        ---@class DFrame: EditablePanel
    "#;

    const TABS_PANEL: &str = r#"
        local PANEL = {}

        function PANEL:Init()
            self.Tabs = {
                {Title = "A", Icon = "icon16/a.png"},
                {Title = "B", Icon = "icon16/b.png"}
            }

            for i = 1, #self.Tabs do
                local tab = self.Tabs[i]
                self.Tabs[i] = vgui.Create("DPanel", self)
                self.Tabs[i].Title = tab.Title
            end

            self.Tabs[1].Label = vgui.Create("DLabel", self.Tabs[1])
            self.Tabs[1].Label:SetFont("DermaDefault")
            local w = self.Tabs[2]:GetWide()
            return w
        end

        vgui.Register("Test14Tabs", PANEL, "DFrame")
    "#;

    const TABS_PATH: &str = "gamemodes/test/gamemode/cl_test14.lua";

    #[test]
    fn slot_reassigned_in_a_loop_takes_the_panel_type() {
        let mut ws = gmod_workspace(TABS_API);
        let undefined = messages(
            &mut ws,
            DiagnosticCode::UndefinedMethod,
            TABS_PATH,
            TABS_PANEL,
        );
        assert_eq!(undefined, Vec::<String>::new());
    }

    #[test]
    fn label_created_on_a_reassigned_slot_keeps_its_class() {
        let mut ws = gmod_workspace(TABS_API);
        let unguarded = messages(
            &mut ws,
            DiagnosticCode::InferUnguardedChild,
            TABS_PATH,
            TABS_PANEL,
        );
        assert_eq!(unguarded, Vec::<String>::new());
    }

    #[test]
    fn literal_element_read_by_a_bounded_loop_index_is_not_nil() {
        let mut ws = gmod_workspace(TABS_API);
        let nil_checks = messages(&mut ws, DiagnosticCode::NeedCheckNil, TABS_PATH, TABS_PANEL);
        assert!(
            !nil_checks.iter().any(|message| message == "tab may be nil"),
            "{nil_checks:?}"
        );
    }

    const REGISTRATION_HELPER: &str = r#"
        Test13Things = {}

        function Test13Register(name, thingTbl)
            thingTbl.ClassName = name
            thingTbl.STATE = thingTbl.STATE or {}
            thingTbl.STATE.STOPPED = 0

            function thingTbl:GetState()
                return self.CurState
            end

            Test13Things[name] = thingTbl
        end

        local THING = {}

        THING.TEAM = {A = 1}

        function THING:Think()
            local state = self:GetState()
            local stopped = self.STATE.STOPPED
            local team = self.TEAM.A
            return state, stopped, team
        end

        Test13Register("thing", THING)
    "#;

    const REGISTRATION_HELPER_PATH: &str = "gamemodes/test/gamemode/sh_test13.lua";

    #[test]
    fn method_a_registration_helper_defines_on_its_parameter_is_seen_on_the_argument() {
        let mut ws = gmod_workspace("---@meta");
        let undefined = messages(
            &mut ws,
            DiagnosticCode::UndefinedMethod,
            REGISTRATION_HELPER_PATH,
            REGISTRATION_HELPER,
        );
        assert_eq!(undefined, Vec::<String>::new());
    }

    #[test]
    fn field_a_registration_helper_sets_on_its_parameter_is_seen_on_the_argument() {
        let mut ws = gmod_workspace("---@meta");
        let undefined = messages(
            &mut ws,
            DiagnosticCode::UndefinedField,
            REGISTRATION_HELPER_PATH,
            REGISTRATION_HELPER,
        );
        assert!(
            !undefined.iter().any(|message| message.contains("`STATE`")),
            "{undefined:?}"
        );
    }

    #[test]
    fn parameter_members_reach_an_argument_in_another_file() {
        let mut ws = gmod_workspace("---@meta");
        ws.def_file(
            "gamemodes/test/gamemode/sh_register.lua",
            r#"
            function Test13Register(name, thingTbl)
                function thingTbl:GetState() end
            end
            "#,
        );
        let undefined = messages(
            &mut ws,
            DiagnosticCode::UndefinedMethod,
            "gamemodes/test/gamemode/sh_thing.lua",
            r#"
            local THING = {}
            function THING:Think()
                return self:GetState()
            end
            Test13Register("thing", THING)
            "#,
        );
        assert_eq!(undefined, Vec::<String>::new());
    }

    #[test]
    fn parameter_members_do_not_reach_a_table_the_helper_never_receives() {
        let mut ws = gmod_workspace("---@meta");
        let undefined = messages(
            &mut ws,
            DiagnosticCode::UndefinedMethod,
            REGISTRATION_HELPER_PATH,
            r#"
            function Test13Register(name, thingTbl)
                function thingTbl:GetState() end
            end
            local OTHER = {}
            function OTHER:Think()
                return self:GetState()
            end
            "#,
        );
        assert_eq!(undefined.len(), 1, "{undefined:?}");
    }

    #[test]
    fn inline_table_literal_does_not_close_a_method_installer_parameter() {
        let mut ws = gmod_workspace("---@meta");
        let undefined = messages(
            &mut ws,
            DiagnosticCode::UndefinedField,
            REGISTRATION_HELPER_PATH,
            r#"
            function RegisterChat(name, data)
                function data:CanHear()
                    return true
                end
                data.uniqueID = name
                return data.bNoIndicator
            end
            RegisterChat("me", { format = "** %s %s" })
            "#,
        );
        assert_eq!(undefined, Vec::<String>::new());
    }

    const OS_DATE_API: &str = r#"
        ---@meta
        ---@class DateData
        ---@class DLabel
        ---@field SetText fun(self: DLabel, text: string)
        os = {}
        ---@overload fun(format: "*t", time?: number): DateData
        ---@overload fun(format: "!*t", time?: number): DateData
        ---@overload fun(format?: string, time?: number): string
        ---@param format "*t"|"!*t"
        ---@param time? number
        ---@return DateData
        function os.date(format, time) end
        ---@return number
        function os.time() end
    "#;

    #[test]
    fn os_date_with_a_text_format_returns_a_string() {
        let mut ws = gmod_workspace(OS_DATE_API);
        let mismatches = messages(
            &mut ws,
            DiagnosticCode::ParamTypeMismatch,
            "gamemodes/test/gamemode/cl_test10.lua",
            r#"
            ---@type DLabel
            local label
            label:SetText(os.date("%Y-%m-%d %X %Z", os.time()))
            "#,
        );
        assert_eq!(mismatches, Vec::<String>::new());
    }

    #[test]
    fn os_date_with_the_table_format_returns_date_data() {
        let mut ws = gmod_workspace(OS_DATE_API);
        let mismatches = messages(
            &mut ws,
            DiagnosticCode::ParamTypeMismatch,
            "gamemodes/test/gamemode/cl_test10.lua",
            r#"
            ---@type DLabel
            local label
            label:SetText(os.date("*t"))
            "#,
        );
        assert_eq!(mismatches.len(), 1, "{mismatches:?}");
    }
    #[test]
    fn returned_string_template_never_leaks_as_a_raw_template() {
        let mut ws = gmod_workspace(CONTROL_TABLE_API);
        let file_id = ws.def_file(
            "gamemodes/test/gamemode/cl_template.lua",
            r#"
            local name = GetName()
            local literal = vgui.GetControlTable("DHTML")
            local runtime = vgui.GetControlTable(name)
            local unbound = vgui.GetControlTable()
            "#,
        );
        for local in ["literal", "runtime", "unbound"] {
            let typ = ws.local_ty(file_id, local);
            assert!(!typ.contain_tpl(), "{local}: {typ:?}");
        }
    }
}
