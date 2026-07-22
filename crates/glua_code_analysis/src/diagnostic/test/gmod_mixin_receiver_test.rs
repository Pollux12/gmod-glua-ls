#[cfg(test)]
mod tests {
    use crate::{
        DiagnosticCode, Emmyrc, LuaMemberKey, LuaMemberOwner, LuaTypeDeclId, VirtualWorkspace,
    };
    use glua_parser::{LuaAstNode, LuaClosureExpr, LuaFuncStat, PathTrait};
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    fn has_code(diagnostics: &[lsp_types::Diagnostic], code: DiagnosticCode) -> bool {
        let code = Some(NumberOrString::String(code.get_name().to_string()));
        diagnostics.iter().any(|diagnostic| diagnostic.code == code)
    }

    fn gmod_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws
    }

    fn file_id(ws: &VirtualWorkspace, path: &str) -> crate::FileId {
        let uri = ws.virtual_url_generator.new_uri(path);
        ws.analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_id(&uri)
            .unwrap_or_else(|| panic!("test file must exist: {path}"))
    }

    fn has_undefined_method(ws: &VirtualWorkspace, file_id: crate::FileId) -> bool {
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        has_code(&diagnostics, DiagnosticCode::UndefinedMethod)
    }

    fn signature_ids_for_path(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
        function_path: &str,
    ) -> Vec<crate::LuaSignatureId> {
        let db = ws.analysis.compilation.get_db();
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_red_root())
        else {
            return Vec::new();
        };
        let mut signature_ids = root
            .descendants()
            .filter_map(LuaFuncStat::cast)
            .filter_map(|func_stat| {
                (func_stat
                    .get_func_name()
                    .and_then(|name| name.get_access_path())
                    .is_some_and(|path| path.as_str() == function_path))
                .then(|| func_stat.get_closure())
                .flatten()
                .map(|closure| crate::LuaSignatureId::from_closure(file_id, &closure))
            })
            .collect::<Vec<_>>();
        signature_ids.sort_unstable_by_key(|signature_id| signature_id.get_position());
        signature_ids
    }

    fn has_inferred_param(
        ws: &VirtualWorkspace,
        signature_id: crate::LuaSignatureId,
        param_idx: usize,
    ) -> bool {
        ws.analysis
            .compilation
            .get_db()
            .get_call_site_param_index()
            .get_inferred_param(&signature_id, param_idx)
            .is_some()
    }

    fn has_inferred_receiver(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
        function_path: &str,
    ) -> bool {
        let db = ws.analysis.compilation.get_db();
        let Some(signature_id) = signature_ids_for_path(ws, file_id, function_path)
            .first()
            .copied()
        else {
            return false;
        };
        let Some(signature) = db.get_signature_index().get(&signature_id) else {
            return false;
        };
        has_inferred_param(ws, signature_id, signature.params.len())
    }

    fn only_closure_has_inferred_first_param(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
    ) -> bool {
        let db = ws.analysis.compilation.get_db();
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_red_root())
        else {
            return false;
        };
        let mut closures = root.descendants().filter_map(LuaClosureExpr::cast);
        let Some(closure) = closures.next() else {
            return false;
        };
        if closures.next().is_some() {
            return false;
        }
        let signature_id = crate::LuaSignatureId::from_closure(file_id, &closure);
        db.get_call_site_param_index()
            .get_inferred_param(&signature_id, 0)
            .is_some()
    }

    #[test]
    fn cross_file_returned_mixin_dynamic_dispatch_uses_receiver_type() {
        let mut ws = gmod_workspace();

        let _file_ids = ws.def_files(vec![
            (
                "lua/starfall/editor/syntaxmodes/starfall.lua",
                r#"
                local EDITOR = {}

                function EDITOR:BlockCommentSelection()
                    self:MakeSelection()
                    self:GetSelection()
                end

                return EDITOR
                "#,
            ),
            (
                "lua/starfall/editor/tabhandlers/tab_wire.lua",
                r#"
                ---@class TabHandler
                local PANEL = {}
                PANEL.Modes = { Text = {} }

                function PANEL:MakeSelection() end
                function PANEL:GetSelection() end

                function PANEL:LoadModes()
                    self.Modes.Starfall = include("starfall/editor/syntaxmodes/starfall.lua")
                    self.CurrentMode = self.Modes.Starfall
                end

                function PANEL:Dispatch(name, ...)
                    local f = assert(self.CurrentMode, "No current mode set")[name]
                    if not f then
                        f = PANEL.Modes.Text[name]
                    end
                    if f then
                        return f(self, ...)
                    end
                end
                "#,
            ),
        ]);

        assert!(!has_undefined_method(
            &ws,
            file_id(&ws, "lua/starfall/editor/syntaxmodes/starfall.lua")
        ));
    }

    #[test]
    fn annotated_assert_alias_preserves_returned_mixin_source_identity() {
        let mut ws = gmod_workspace();
        ws.def_file(
            "annotations/global.lua",
            r#"
            ---@generic T
            ---@param expression T
            ---@return T
            ---@[return_alias(0)]
            function _G.assert(expression, ...) end
            "#,
        );
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/annotated_assert.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self:ProvidedByReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/annotated_assert_consumer.lua",
                r#"
                local PANEL = { Modes = { Text = {} } }
                function PANEL:ProvidedByReceiver() end
                function PANEL:Load()
                    self.Modes.Selected = include("mixins/annotated_assert.lua")
                    self.CurrentMode = self.Modes.Selected
                end
                function PANEL:Dispatch(name)
                    local callback = assert(self.CurrentMode, "mode required")[name]
                    if not callback then callback = PANEL.Modes.Text[name] end
                    if callback then return callback(self) end
                end
                "#,
            ),
        ]);

        let target_file_id = file_id(&ws, "lua/mixins/annotated_assert.lua");
        assert!(has_inferred_receiver(&ws, target_file_id, "MIXIN.Run"));
        assert!(!has_undefined_method(&ws, target_file_id));
    }

    #[test]
    fn unannotated_assert_override_does_not_claim_returned_value_identity() {
        let mut ws = gmod_workspace();
        ws.def_file(
            "annotations/global.lua",
            r#"
            ---@generic T
            ---@param expression T
            ---@return T
            function _G.assert(expression, ...) end
            "#,
        );
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/unannotated_assert.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self:ProvidedByReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/unannotated_assert_consumer.lua",
                r#"
                local PANEL = { Modes = { Text = {} } }
                function PANEL:ProvidedByReceiver() end
                function PANEL:Load()
                    self.Modes.Selected = include("mixins/unannotated_assert.lua")
                    self.CurrentMode = self.Modes.Selected
                end
                function PANEL:Dispatch(name)
                    local callback = assert(self.CurrentMode, "mode required")[name]
                    if not callback then callback = PANEL.Modes.Text[name] end
                    if callback then return callback(self) end
                end
                "#,
            ),
        ]);

        let target_file_id = file_id(&ws, "lua/mixins/unannotated_assert.lua");
        assert!(!has_inferred_receiver(&ws, target_file_id, "MIXIN.Run"));
        assert!(has_undefined_method(&ws, target_file_id));
    }

    #[test]
    fn undocumented_vgui_panel_dispatches_returned_mixin_with_authoring_self() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/starfall/editor/syntaxmodes/starfall.lua",
                r#"
                local EDITOR = {}
                function EDITOR:Run()
                    self:ProvidedByReceiver()
                end
                return EDITOR
                "#,
            ),
            (
                "lua/starfall/editor/tabhandlers/tab_wire.lua",
                r#"
                local TabHandler = {
                    Modes = {},
                    ControlName = "TabHandler_wire",
                }
                TabHandler.Modes.Text = {}
                function TabHandler:Init()
                    self.Modes.Starfall = include("starfall/editor/syntaxmodes/starfall.lua")
                end
                function TabHandler:Open(content, mode)
                    content.CurrentMode = mode
                end

                local PANEL = {}
                function PANEL:ProvidedByReceiver() end
                function PANEL:Init()
                    self.CurrentMode = assert(TabHandler.Modes.Text)
                end
                function PANEL:SetMode(mode_name)
                    self.CurrentMode = TabHandler.Modes[mode_name or "Text"]
                    if not self.CurrentMode then
                        self.CurrentMode = assert(TabHandler.Modes.Text)
                    end
                end
                function PANEL:DoAction(name, ...)
                    local callback = self.CurrentMode[name]
                    if callback then
                        return callback(self, ...)
                    end
                end

                vgui.Register(TabHandler.ControlName, PANEL, "Panel")
                "#,
            ),
        ]);

        let target_file_id = file_id(&ws, "lua/starfall/editor/syntaxmodes/starfall.lua");
        assert!(has_inferred_receiver(&ws, target_file_id, "EDITOR.Run"));
        assert!(!has_undefined_method(&ws, target_file_id));

        let consumer_file_id = file_id(&ws, "lua/starfall/editor/tabhandlers/tab_wire.lua");
        let db = ws.analysis.compilation.get_db();
        let current_mode_key = crate::LuaMemberKey::Name("CurrentMode".into());
        let current_mode_members = db
            .get_member_index()
            .get_current_members_for_key(&current_mode_key)
            .into_iter()
            .filter(|member| member.get_file_id() == consumer_file_id)
            .collect::<Vec<_>>();
        assert!(current_mode_members.len() >= 3);
        let expected_owner =
            crate::LuaMemberOwner::Type(crate::LuaTypeDeclId::global("TabHandler_wire"));
        assert!(current_mode_members.iter().all(|member| {
            db.get_member_index().get_current_owner(&member.get_id()) == Some(&expected_owner)
        }));
    }

    #[test]
    fn vgui_class_member_history_replays_duplicate_writes_in_source_order() {
        let mut ws = gmod_workspace();
        ws.def_files(vec![(
            "lua/vgui/ordered_panel.lua",
            r#"
                local PANEL = {}
                PANEL.OrderedValue = "stale"
                PANEL.OrderedValue = 42
                vgui.Register("OrderedReplayPanel", PANEL, "Panel")
                "#,
        )]);

        let db = ws.analysis.compilation.get_db();
        let owner = LuaMemberOwner::Type(LuaTypeDeclId::global("OrderedReplayPanel"));
        let key = LuaMemberKey::Name("OrderedValue".into());
        let history = db
            .get_member_index()
            .get_current_owner_members_for_key(&owner, &key);
        assert_eq!(history.len(), 2);
        let latest_member_id = history
            .iter()
            .map(|member| member.get_id())
            .max_by_key(|member_id| member_id.get_position())
            .expect("registered class history must contain the latest write");
        let current_member_ids = db
            .get_member_index()
            .get_member_item(&owner, &key)
            .expect("registered class member must exist")
            .get_member_ids();
        assert_eq!(current_member_ids, vec![latest_member_id]);
    }

    #[test]
    fn named_vgui_callback_uses_all_concrete_receiver_bindings() {
        let mut ws = gmod_workspace();
        ws.def_file(
            "annotations/vgui_buttons.lua",
            r#"
            ---@class DButton: Panel
            local DButton = {}
            function DButton:IsDown() end
            "#,
        );
        let file_id = ws.def_file(
            "lua/starfall/editor/named_button.lua",
            r#"
            local function PaintFlatButton(panel)
                panel:IsDown()
            end

            local ButtonTable = vgui.RegisterTable({
                Paint = PaintFlatButton,
            }, "DButton")
            local first = vgui.CreateFromTable(ButtonTable)
            local second = vgui.CreateFromTable(ButtonTable)
            first.Paint = PaintFlatButton
            second.Paint = PaintFlatButton
            "#,
        );

        assert!(only_closure_has_inferred_first_param(&ws, file_id));
        assert!(!has_undefined_method(&ws, file_id));
    }

    #[test]
    fn named_vgui_callback_rejects_incompatible_receiver_binding() {
        let mut ws = gmod_workspace();
        ws.def_file(
            "annotations/vgui_buttons.lua",
            r#"
            ---@class DButton: Panel
            local DButton = {}
            function DButton:IsDown() end
            ---@class DPanel: Panel
            "#,
        );
        let file_id = ws.def_file(
            "lua/starfall/editor/mixed_button.lua",
            r#"
            local function PaintFlatButton(panel)
                panel:IsDown()
            end

            vgui.RegisterTable({
                Paint = PaintFlatButton,
            }, "DButton")
            local other = vgui.Create("DPanel")
            other.Paint = PaintFlatButton
            "#,
        );

        assert!(!only_closure_has_inferred_first_param(&ws, file_id));
    }

    #[test]
    fn named_vgui_callback_rejects_non_binding_escape() {
        let mut ws = gmod_workspace();
        ws.def_file(
            "annotations/vgui_buttons.lua",
            r#"
            ---@class DButton: Panel
            "#,
        );
        let file_id = ws.def_file(
            "lua/starfall/editor/escaped_button.lua",
            r#"
            local function PaintFlatButton(panel) end

            vgui.RegisterTable({
                Paint = PaintFlatButton,
            }, "DButton")
            consume(PaintFlatButton)
            "#,
        );

        assert!(!only_closure_has_inferred_first_param(&ws, file_id));
    }

    #[test]
    fn unregistered_panel_shaped_table_keeps_table_member_owners() {
        let mut ws = gmod_workspace();
        let source_file_id = ws.def_file(
            "lua/autorun/unregistered_panel.lua",
            r#"
            local PANEL = {}
            function PANEL:First()
                self.SharedValue = 1
            end
            function PANEL:Second()
                self.SharedValue = 2
            end
            "#,
        );

        let db = ws.analysis.compilation.get_db();
        let key = crate::LuaMemberKey::Name("SharedValue".into());
        let members = db
            .get_member_index()
            .get_current_members_for_key(&key)
            .into_iter()
            .filter(|member| member.get_file_id() == source_file_id)
            .collect::<Vec<_>>();
        assert_eq!(members.len(), 2);
        assert!(members.iter().all(|member| matches!(
            db.get_member_index().get_current_owner(&member.get_id()),
            Some(crate::LuaMemberOwner::Element(_))
        )));
    }

    #[test]
    fn reassigned_nonconstant_vgui_control_name_keeps_table_member_owners() {
        let mut ws = gmod_workspace();
        let source_file_id = ws.def_file(
            "lua/autorun/nonconstant_panel_name.lua",
            r#"
            local TabHandler = { ControlName = "InitialPanelName" }
            TabHandler.ControlName = get_runtime_panel_name()

            local PANEL = {}
            function PANEL:First()
                self.SharedValue = 1
            end
            function PANEL:Second()
                self.SharedValue = 2
            end
            vgui.Register(TabHandler.ControlName, PANEL, "Panel")
            "#,
        );

        let db = ws.analysis.compilation.get_db();
        let key = crate::LuaMemberKey::Name("SharedValue".into());
        let members = db
            .get_member_index()
            .get_current_members_for_key(&key)
            .into_iter()
            .filter(|member| member.get_file_id() == source_file_id)
            .collect::<Vec<_>>();
        assert_eq!(members.len(), 2);
        assert!(members.iter().all(|member| matches!(
            db.get_member_index().get_current_owner(&member.get_id()),
            Some(crate::LuaMemberOwner::Element(_))
        )));
    }

    #[test]
    fn dynamic_mode_selection_rejects_aliased_table_owner() {
        let mut ws = gmod_workspace();
        let target_file_id = ws.def_file(
            "lua/mixins/aliased_modes.lua",
            r#"
            local MIXIN = {}
            function MIXIN:Run()
                self:OnlyOnReceiver()
            end
            return MIXIN
            "#,
        );
        ws.def_file(
            "lua/autorun/aliased_modes_consumer.lua",
            r#"
            local Modes = {}
            local TabHandler = {
                Modes = Modes,
                ControlName = "AliasedModesPanel",
            }
            Modes.Selected = include("mixins/aliased_modes.lua")

            local PANEL = {}
            function PANEL:OnlyOnReceiver() end
            function PANEL:Dispatch(name)
                local callback = TabHandler.Modes[name]
                callback(self)
            end
            vgui.Register(TabHandler.ControlName, PANEL, "Panel")
            "#,
        );

        assert!(!has_inferred_receiver(&ws, target_file_id, "MIXIN.Run"));
    }

    #[test]
    fn dynamic_mode_selection_rejects_reassigned_table_owner() {
        let mut ws = gmod_workspace();
        let target_file_id = ws.def_file(
            "lua/mixins/reassigned_modes.lua",
            r#"
            local MIXIN = {}
            function MIXIN:Run()
                self:OnlyOnReceiver()
            end
            return MIXIN
            "#,
        );
        ws.def_file(
            "lua/autorun/reassigned_modes_consumer.lua",
            r#"
            local TabHandler = {
                Modes = {},
                ControlName = "ReassignedModesPanel",
            }
            TabHandler.Modes.Selected = include("mixins/reassigned_modes.lua")
            local originalModes = TabHandler.Modes
            TabHandler.Modes = originalModes

            local PANEL = {}
            function PANEL:OnlyOnReceiver() end
            function PANEL:Dispatch(name)
                local callback = TabHandler.Modes[name]
                callback(self)
            end
            vgui.Register(TabHandler.ControlName, PANEL, "Panel")
            "#,
        );

        assert!(!has_inferred_receiver(&ws, target_file_id, "MIXIN.Run"));
    }

    #[test]
    fn dynamic_mode_selection_does_not_use_unrelated_table() {
        let mut ws = gmod_workspace();
        let target_file_id = ws.def_file(
            "lua/mixins/unrelated_modes.lua",
            r#"
            local MIXIN = {}
            function MIXIN:Run()
                self:OnlyOnReceiver()
            end
            return MIXIN
            "#,
        );
        ws.def_file(
            "lua/autorun/unrelated_modes_consumer.lua",
            r#"
            local TabHandler = {
                Modes = {},
                ControlName = "UnrelatedModesPanel",
            }
            TabHandler.Modes.Selected = include("mixins/unrelated_modes.lua")

            local PANEL = { OtherModes = {} }
            function PANEL:OnlyOnReceiver() end
            function PANEL:Dispatch(name)
                local callback = self.OtherModes[name]
                callback(self)
            end
            vgui.Register(TabHandler.ControlName, PANEL, "Panel")
            "#,
        );

        assert!(!has_inferred_receiver(&ws, target_file_id, "MIXIN.Run"));
    }

    #[test]
    fn minimal_include_returned_mixin_member_uses_explicit_receiver() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/minimal.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self:ProvidedByReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/minimal_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:ProvidedByReceiver() end
                function PANEL:Load()
                    self.Mixin = include("mixins/minimal.lua")
                end
                function PANEL:Dispatch(name)
                    local callback = self.Mixin[name]
                    callback(self)
                end
                "#,
            ),
        ]);

        assert!(!has_undefined_method(
            &ws,
            file_id(&ws, "lua/mixins/minimal.lua")
        ));
    }

    #[test]
    fn compilefile_returned_mixin_member_uses_explicit_receiver() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/compiled_exact.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self:ProvidedByReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/compiled_exact_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:ProvidedByReceiver() end
                function PANEL:Load()
                    self.Mixin = CompileFile("mixins/compiled_exact.lua")()
                end
                function PANEL:Dispatch(name)
                    local callback = self.Mixin[name]
                    callback(self)
                end
                "#,
            ),
        ]);

        let target_file_id = file_id(&ws, "lua/mixins/compiled_exact.lua");
        assert!(has_inferred_receiver(&ws, target_file_id, "MIXIN.Run"));
        assert!(!has_undefined_method(&ws, target_file_id));
    }

    #[test]
    fn dynamic_compilefile_target_does_not_receive_dispatch_receiver() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/compiled_dynamic.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self:ProvidedByReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/compiled_dynamic_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:ProvidedByReceiver() end
                function PANEL:Load(path)
                    self.Mixin = CompileFile(path)()
                end
                function PANEL:Dispatch(name)
                    local callback = self.Mixin[name]
                    callback(self)
                end
                "#,
            ),
        ]);

        assert!(!has_inferred_receiver(
            &ws,
            file_id(&ws, "lua/mixins/compiled_dynamic.lua"),
            "MIXIN.Run"
        ));
    }

    #[test]
    fn ambiguous_compilefile_targets_do_not_receive_dispatch_receiver() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/compiled_first.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self:ProvidedByReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/mixins/compiled_second.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self:ProvidedByReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/compiled_ambiguous_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:ProvidedByReceiver() end
                function PANEL:Load(useFirst)
                    if useFirst then
                        self.Mixin = CompileFile("mixins/compiled_first.lua")()
                    else
                        self.Mixin = CompileFile("mixins/compiled_second.lua")()
                    end
                end
                function PANEL:Dispatch(name)
                    local callback = self.Mixin[name]
                    callback(self)
                end
                "#,
            ),
        ]);

        for path in [
            "lua/mixins/compiled_first.lua",
            "lua/mixins/compiled_second.lua",
        ] {
            assert!(!has_inferred_receiver(&ws, file_id(&ws, path), "MIXIN.Run"));
        }
    }

    #[test]
    fn dot_defined_mixin_member_uses_leading_self_receiver() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/dot_defined.lua",
                r#"
                local MIXIN = {}
                function MIXIN.Run(self)
                    self.cached = true
                    self:ProvidedByReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/dot_defined_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:ProvidedByReceiver() end
                function PANEL:Load()
                    self.Mixin = include("mixins/dot_defined.lua")
                end
                function PANEL:Dispatch(name)
                    local callback = self.Mixin[name]
                    callback(self)
                end
                "#,
            ),
        ]);

        let target_file_id = file_id(&ws, "lua/mixins/dot_defined.lua");
        let signature_id = signature_ids_for_path(&ws, target_file_id, "MIXIN.Run")[0];
        assert!(has_inferred_param(&ws, signature_id, 0));
        assert!(!has_undefined_method(&ws, target_file_id));
    }

    #[test]
    fn table_literal_mixin_member_uses_leading_self_receiver() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/table_literal.lua",
                r#"
                local MIXIN = {
                    Run = function(self)
                        self:ProvidedByReceiver()
                    end,
                }
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/table_literal_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:ProvidedByReceiver() end
                function PANEL:Load()
                    self.Mixin = include("mixins/table_literal.lua")
                end
                function PANEL:Dispatch(name)
                    local callback = self.Mixin[name]
                    callback(self)
                end
                "#,
            ),
        ]);

        let target_file_id = file_id(&ws, "lua/mixins/table_literal.lua");
        assert!(only_closure_has_inferred_first_param(&ws, target_file_id));
        assert!(!has_undefined_method(&ws, target_file_id));
    }

    #[test]
    fn dot_defined_mixin_rejects_renamed_nonleading_and_reassigned_receivers() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/dot_negative.lua",
                r#"
                local MIXIN = {}
                function MIXIN.Renamed(receiver)
                    receiver:OnlyOnReceiver()
                end
                function MIXIN.NonLeading(value, self)
                    self:OnlyOnReceiver()
                end
                function MIXIN.Reassigned(self)
                    self = {}
                    self:OnlyOnReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/dot_negative_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:OnlyOnReceiver() end
                function PANEL:Load()
                    self.Mixin = include("mixins/dot_negative.lua")
                end
                function PANEL:Dispatch(name)
                    local callback = self.Mixin[name]
                    callback(self)
                end
                "#,
            ),
        ]);

        let target_file_id = file_id(&ws, "lua/mixins/dot_negative.lua");
        for path in ["MIXIN.Renamed", "MIXIN.NonLeading", "MIXIN.Reassigned"] {
            let signature_id = signature_ids_for_path(&ws, target_file_id, path)[0];
            assert!(!has_inferred_param(&ws, signature_id, 0), "{path}");
        }
    }

    #[test]
    fn mixin_method_member_write_keeps_receiver_inference() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/member_write.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self.cached = true
                    self:ProvidedByReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/member_write_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:ProvidedByReceiver() end
                function PANEL:Load()
                    self.Mixin = include("mixins/member_write.lua")
                end
                function PANEL:Dispatch(name)
                    local callback = self.Mixin[name]
                    callback(self)
                end
                "#,
            ),
        ]);

        let target_file_id = file_id(&ws, "lua/mixins/member_write.lua");
        assert!(has_inferred_receiver(&ws, target_file_id, "MIXIN.Run"));
        assert!(!has_undefined_method(&ws, target_file_id));
    }

    #[test]
    fn mixin_method_direct_self_rebind_blocks_receiver_inference() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/self_rebind.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self = {}
                    self:OnlyOnOriginalReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/self_rebind_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:OnlyOnOriginalReceiver() end
                function PANEL:Load()
                    self.Mixin = include("mixins/self_rebind.lua")
                end
                function PANEL:Dispatch(name)
                    local callback = self.Mixin[name]
                    callback(self)
                end
                "#,
            ),
        ]);

        assert!(!has_inferred_receiver(
            &ws,
            file_id(&ws, "lua/mixins/self_rebind.lua"),
            "MIXIN.Run"
        ));
    }

    #[test]
    fn nested_captured_self_rebind_blocks_receiver_inference() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/nested_self_rebind.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run(other)
                    local function replace()
                        self = other
                    end
                    replace()
                    self:OnlyOnReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/nested_self_rebind_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:OnlyOnReceiver() end
                function PANEL:Load()
                    self.Mixin = include("mixins/nested_self_rebind.lua")
                end
                function PANEL:Dispatch(name)
                    local callback = self.Mixin[name]
                    callback(self, {})
                end
                "#,
            ),
        ]);

        assert!(!has_inferred_receiver(
            &ws,
            file_id(&ws, "lua/mixins/nested_self_rebind.lua"),
            "MIXIN.Run"
        ));
    }

    #[test]
    fn nested_shadowed_self_rebind_keeps_receiver_inference() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/nested_shadowed_self.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    local function replace(self)
                        self = {}
                    end
                    replace({})
                    self:OnlyOnReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/nested_shadowed_self_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:OnlyOnReceiver() end
                function PANEL:Load()
                    self.Mixin = include("mixins/nested_shadowed_self.lua")
                end
                function PANEL:Dispatch(name)
                    local callback = self.Mixin[name]
                    callback(self)
                end
                "#,
            ),
        ]);

        let target_file_id = file_id(&ws, "lua/mixins/nested_shadowed_self.lua");
        assert!(has_inferred_receiver(&ws, target_file_id, "MIXIN.Run"));
        assert!(!has_undefined_method(&ws, target_file_id));
    }

    #[test]
    fn vararg_mixin_method_uses_slot_after_vararg_param() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/vararg.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run(...)
                    self:ProvidedByReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/vararg_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:ProvidedByReceiver() end
                function PANEL:Load()
                    self.Mixin = include("mixins/vararg.lua")
                end
                function PANEL:Dispatch(name, ...)
                    local callback = self.Mixin[name]
                    callback(self, ...)
                end
                "#,
            ),
        ]);

        let target_file_id = file_id(&ws, "lua/mixins/vararg.lua");
        let signature_id = signature_ids_for_path(&ws, target_file_id, "MIXIN.Run")[0];
        let signature = ws
            .analysis
            .compilation
            .get_db()
            .get_signature_index()
            .get(&signature_id)
            .expect("mixin method signature must exist");
        assert_eq!(signature.params, ["..."]);
        assert!(has_inferred_param(
            &ws,
            signature_id,
            signature.params.len()
        ));
        assert!(!has_undefined_method(&ws, target_file_id));
    }

    #[test]
    fn unrelated_include_returned_table_does_not_receive_dispatch_receiver() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/selected.lua",
                r#"
                local SELECTED = {}
                function SELECTED:Run() end
                return SELECTED
                "#,
            ),
            (
                "lua/mixins/unrelated.lua",
                r#"
                local UNRELATED = {}
                function UNRELATED:Run()
                    self:OnlyOnReceiver()
                end
                return UNRELATED
                "#,
            ),
            (
                "lua/mixins/unrelated_closure.lua",
                r#"
                return function(receiver)
                    receiver:OnlyOnReceiver()
                end
                "#,
            ),
            (
                "lua/autorun/unrelated_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:OnlyOnReceiver() end
                function PANEL:Load()
                    self.Selected = include("mixins/selected.lua")
                    self.Unrelated = include("mixins/unrelated.lua")
                    self.UnrelatedClosure = include("mixins/unrelated_closure.lua")
                end
                function PANEL:Dispatch(name)
                    local callback = self.Selected[name]
                    callback(self)
                end
                "#,
            ),
        ]);

        assert!(!has_inferred_receiver(
            &ws,
            file_id(&ws, "lua/mixins/unrelated.lua"),
            "UNRELATED.Run"
        ));
        assert!(!only_closure_has_inferred_first_param(
            &ws,
            file_id(&ws, "lua/mixins/unrelated_closure.lua")
        ));
    }

    #[test]
    fn non_receiver_first_argument_does_not_pollute_mixin_params() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/non_receiver_first.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run(value)
                    self:OnlyOnReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/non_receiver_first_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:OnlyOnReceiver() end
                function PANEL:Load()
                    self.Selected = include("mixins/non_receiver_first.lua")
                end
                function PANEL:Dispatch(name)
                    local callback = self.Selected[name]
                    callback("other", self)
                end
                "#,
            ),
        ]);

        let signature_id = signature_ids_for_path(
            &ws,
            file_id(&ws, "lua/mixins/non_receiver_first.lua"),
            "MIXIN.Run",
        )[0];
        let signature = ws
            .analysis
            .compilation
            .get_db()
            .get_signature_index()
            .get(&signature_id)
            .expect("mixin method signature must exist");
        assert!(!has_inferred_param(&ws, signature_id, 0));
        assert!(!has_inferred_param(
            &ws,
            signature_id,
            signature.params.len()
        ));
    }

    #[test]
    fn nested_unrelated_source_members_do_not_select_an_include_target() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/pick_selected.lua",
                r#"
                local SELECTED = {}
                function SELECTED:Run()
                    self:OnlyOnReceiver()
                end
                return SELECTED
                "#,
            ),
            (
                "lua/mixins/pick_unrelated.lua",
                r#"
                local UNRELATED = {}
                function UNRELATED:Run()
                    self:OnlyOnReceiver()
                end
                return UNRELATED
                "#,
            ),
            (
                "lua/autorun/pick_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:OnlyOnReceiver() end
                function PANEL:Load()
                    self.Selected = include("mixins/pick_selected.lua")
                    self.Unrelated = include("mixins/pick_unrelated.lua")
                end
                local function pick(first, second)
                    return first
                end
                function PANEL:Dispatch(name)
                    local callback = pick(self.Selected, self.Unrelated)[name]
                    callback(self)
                end
                "#,
            ),
        ]);

        assert!(!has_inferred_receiver(
            &ws,
            file_id(&ws, "lua/mixins/pick_selected.lua"),
            "SELECTED.Run"
        ));
        assert!(!has_inferred_receiver(
            &ws,
            file_id(&ws, "lua/mixins/pick_unrelated.lua"),
            "UNRELATED.Run"
        ));
    }

    #[test]
    fn generic_return_correlation_does_not_select_include_source() {
        let mut ws = gmod_workspace();
        let target_file_id = ws.def_file(
            "lua/mixins/generic_wrapper.lua",
            r#"
            local MIXIN = {}
            function MIXIN:Run()
                self:OnlyOnReceiver()
            end
            return MIXIN
            "#,
        );
        ws.def_file(
            "lua/autorun/generic_wrapper_consumer.lua",
            r#"
            ---@generic T
            ---@param value T
            ---@return T
            local function fabricate(value) end

            local PANEL = {}
            function PANEL:OnlyOnReceiver() end
            function PANEL:Load()
                self.Selected = include("mixins/generic_wrapper.lua")
            end
            function PANEL:Dispatch(name)
                local callback = fabricate(self.Selected)[name]
                callback(self)
            end
            "#,
        );

        assert!(!has_inferred_receiver(&ws, target_file_id, "MIXIN.Run"));
    }

    #[test]
    fn colon_require_guard_dot_call_does_not_unwrap_receiver_argument() {
        let mut ws = gmod_workspace();
        let target_file_id = ws.def_file(
            "lua/mixins/colon_guard.lua",
            r#"
            local MIXIN = {}
            function MIXIN:Run()
                self:OnlyOnReceiver()
            end
            return MIXIN
            "#,
        );
        let consumer_file_id = ws.def_file(
            "lua/autorun/colon_guard_consumer.lua",
            r#"
            local Guard = {}
            function Guard:Check(path)
                local ok = pcall(require, path)
                return ok
            end

            local PANEL = {}
            function PANEL:OnlyOnReceiver() end
            function PANEL:Load()
                self.Selected = include("mixins/colon_guard.lua")
            end
            function PANEL:Dispatch(name)
                local callback = Guard.Check(self.Selected, "not-a-source")[name]
                callback(self)
            end
            "#,
        );

        let guard_signature = signature_ids_for_path(&ws, consumer_file_id, "Guard.Check")[0];
        assert_eq!(
            ws.analysis
                .compilation
                .get_db()
                .get_signature_index()
                .get(&guard_signature)
                .expect("guard signature must exist")
                .require_guard_param(),
            Some(0)
        );
        assert!(!has_inferred_receiver(&ws, target_file_id, "MIXIN.Run"));
    }

    #[test]
    fn reassigned_include_member_does_not_receive_dispatch_receiver() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/reassigned_source.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self:OnlyOnReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/reassigned_source_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:OnlyOnReceiver() end
                function PANEL:Load()
                    self.Selected = include("mixins/reassigned_source.lua")
                    self.Selected = {}
                end
                function PANEL:Dispatch(name)
                    local callback = self.Selected[name]
                    callback(self)
                end
                "#,
            ),
        ]);

        assert!(!has_inferred_receiver(
            &ws,
            file_id(&ws, "lua/mixins/reassigned_source.lua"),
            "MIXIN.Run"
        ));
    }

    #[test]
    fn overwritten_returned_table_member_does_not_receive_dispatch_receiver() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/overwritten_member.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self:OnlyOnReceiver()
                end
                MIXIN.Run = function() end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/overwritten_member_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:OnlyOnReceiver() end
                function PANEL:Load()
                    self.Selected = include("mixins/overwritten_member.lua")
                end
                function PANEL:Dispatch(name)
                    local callback = self.Selected[name]
                    callback(self)
                end
                "#,
            ),
        ]);

        assert!(!has_inferred_receiver(
            &ws,
            file_id(&ws, "lua/mixins/overwritten_member.lua"),
            "MIXIN.Run"
        ));
    }

    #[test]
    fn reassigned_returned_table_does_not_receive_dispatch_receiver() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/reassigned_table.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self:OnlyOnReceiver()
                end
                MIXIN = {}
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/reassigned_table_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:OnlyOnReceiver() end
                function PANEL:Load()
                    self.Selected = include("mixins/reassigned_table.lua")
                end
                function PANEL:Dispatch(name)
                    local callback = self.Selected[name]
                    callback(self)
                end
                "#,
            ),
        ]);

        assert!(!has_inferred_receiver(
            &ws,
            file_id(&ws, "lua/mixins/reassigned_table.lua"),
            "MIXIN.Run"
        ));
    }

    #[test]
    fn overwritten_callback_initializer_does_not_receive_dispatch_receiver() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/overwritten_callback.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self:OnlyOnReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/overwritten_callback_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:OnlyOnReceiver() end
                function PANEL:Load()
                    self.Selected = include("mixins/overwritten_callback.lua")
                end
                function PANEL:Dispatch(name)
                    local callback = self.Selected[name]
                    callback = function() end
                    callback(self)
                end
                "#,
            ),
        ]);

        assert!(!has_inferred_receiver(
            &ws,
            file_id(&ws, "lua/mixins/overwritten_callback.lua"),
            "MIXIN.Run"
        ));
    }

    #[test]
    fn conditionally_reassigned_callback_does_not_receive_dispatch_receiver() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/conditional_callback.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self:OnlyOnReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/conditional_callback_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:OnlyOnReceiver() end
                function PANEL:Load()
                    self.Selected = include("mixins/conditional_callback.lua")
                end
                function PANEL:Dispatch(name, replace)
                    local callback = self.Selected[name]
                    if replace then
                        callback = function() end
                    end
                    callback(self)
                end
                "#,
            ),
        ]);

        assert!(!has_inferred_receiver(
            &ws,
            file_id(&ws, "lua/mixins/conditional_callback.lua"),
            "MIXIN.Run"
        ));
    }

    #[test]
    fn loop_reassigned_callback_does_not_receive_dispatch_receiver() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/loop_callback.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self:OnlyOnReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/loop_callback_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:OnlyOnReceiver() end
                function PANEL:Load()
                    self.Selected = include("mixins/loop_callback.lua")
                end
                function PANEL:Dispatch(name, count)
                    local callback = self.Selected[name]
                    for _ = 1, count do
                        callback = function() end
                    end
                    callback(self)
                end
                "#,
            ),
        ]);

        assert!(!has_inferred_receiver(
            &ws,
            file_id(&ws, "lua/mixins/loop_callback.lua"),
            "MIXIN.Run"
        ));
    }

    #[test]
    fn captured_callback_reassignment_does_not_receive_dispatch_receiver() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/captured_callback.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self:OnlyOnReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/captured_callback_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:OnlyOnReceiver() end
                function PANEL:Load()
                    self.Selected = include("mixins/captured_callback.lua")
                end
                function PANEL:Dispatch(name, replace)
                    local callback = self.Selected[name]
                    local function replaceCallback()
                        callback = function() end
                    end
                    if replace then
                        replaceCallback()
                    end
                    callback(self)
                end
                "#,
            ),
        ]);

        assert!(!has_inferred_receiver(
            &ws,
            file_id(&ws, "lua/mixins/captured_callback.lua"),
            "MIXIN.Run"
        ));
    }

    #[test]
    fn callback_reassignment_after_call_does_not_block_receiver_inference() {
        let mut ws = gmod_workspace();
        let _file_ids = ws.def_files(vec![
            (
                "lua/mixins/later_callback.lua",
                r#"
                local MIXIN = {}
                function MIXIN:Run()
                    self.LastRun = true
                    self:OnlyOnReceiver()
                end
                return MIXIN
                "#,
            ),
            (
                "lua/autorun/later_callback_consumer.lua",
                r#"
                local PANEL = {}
                function PANEL:OnlyOnReceiver() end
                function PANEL:Load()
                    self.Selected = include("mixins/later_callback.lua")
                end
                function PANEL:Dispatch(name)
                    local callback = self.Selected[name]
                    self.LastCallback = callback
                    callback(self)
                    callback = function() end
                end
                "#,
            ),
        ]);

        let target_file_id = file_id(&ws, "lua/mixins/later_callback.lua");
        assert!(has_inferred_receiver(&ws, target_file_id, "MIXIN.Run"));
        assert!(!has_undefined_method(&ws, target_file_id));
    }

    #[test]
    fn server_dispatch_selects_only_server_realm_mixin_method() {
        let mut ws = gmod_workspace();
        let target_file_id = ws.def_file(
            "lua/mixins/realm_split_server.lua",
            r#"
            local MIXIN = {}
            if SERVER then
                function MIXIN:Run()
                    self:ServerReceiverMethod()
                end
            else
                function MIXIN:Run()
                    self:ClientReceiverMethod()
                end
            end
            return MIXIN
            "#,
        );
        ws.def_file(
            "lua/autorun/sv_realm_split_consumer.lua",
            r#"
            local PANEL = {}
            function PANEL:ServerReceiverMethod() end
            function PANEL:Load()
                self.Selected = include("mixins/realm_split_server.lua")
            end
            function PANEL:Dispatch(name)
                if SERVER then
                    local callback = self.Selected[name]
                    callback(self)
                end
            end
            "#,
        );

        let signatures = signature_ids_for_path(&ws, target_file_id, "MIXIN.Run");
        assert_eq!(signatures.len(), 2);
        let db = ws.analysis.compilation.get_db();
        let server_slot = db
            .get_signature_index()
            .get(&signatures[0])
            .expect("server signature must exist")
            .params
            .len();
        let client_slot = db
            .get_signature_index()
            .get(&signatures[1])
            .expect("client signature must exist")
            .params
            .len();
        assert!(has_inferred_param(&ws, signatures[0], server_slot));
        assert!(!has_inferred_param(&ws, signatures[1], client_slot));
    }

    #[test]
    fn client_dispatch_selects_only_client_realm_mixin_method() {
        let mut ws = gmod_workspace();
        let target_file_id = ws.def_file(
            "lua/mixins/realm_split_client.lua",
            r#"
            local MIXIN = {}
            if SERVER then
                function MIXIN:Run()
                    self:ServerReceiverMethod()
                end
            else
                function MIXIN:Run()
                    self:ClientReceiverMethod()
                end
            end
            return MIXIN
            "#,
        );
        ws.def_file(
            "lua/autorun/cl_realm_split_consumer.lua",
            r#"
            local PANEL = {}
            function PANEL:ClientReceiverMethod() end
            function PANEL:Load()
                self.Selected = include("mixins/realm_split_client.lua")
            end
            if CLIENT then
                function PANEL:Dispatch(name)
                    local callback = self.Selected[name]
                    callback(self)
                end
            end
            "#,
        );

        let signatures = signature_ids_for_path(&ws, target_file_id, "MIXIN.Run");
        assert_eq!(signatures.len(), 2);
        let db = ws.analysis.compilation.get_db();
        let server_slot = db
            .get_signature_index()
            .get(&signatures[0])
            .expect("server signature must exist")
            .params
            .len();
        let client_slot = db
            .get_signature_index()
            .get(&signatures[1])
            .expect("client signature must exist")
            .params
            .len();
        assert!(!has_inferred_param(&ws, signatures[0], server_slot));
        assert!(has_inferred_param(&ws, signatures[1], client_slot));
    }

    #[test]
    fn source_include_member_selection_uses_caller_realm() {
        let mut ws = gmod_workspace();
        let server_target = ws.def_file(
            "lua/mixins/sv_source_member.lua",
            r#"
            local MIXIN = {}
            function MIXIN:Run()
                self:ServerReceiverMethod()
            end
            return MIXIN
            "#,
        );
        let client_target = ws.def_file(
            "lua/mixins/cl_source_member.lua",
            r#"
            local MIXIN = {}
            function MIXIN:Run()
                self:ClientReceiverMethod()
            end
            return MIXIN
            "#,
        );
        ws.def_file(
            "lua/autorun/sv_source_member_consumer.lua",
            r#"
            local PANEL = {}
            function PANEL:ServerReceiverMethod() end
            if SERVER then
                PANEL.Selected = include("mixins/sv_source_member.lua")
            else
                PANEL.Selected = include("mixins/cl_source_member.lua")
            end
            if SERVER then
                function PANEL:Dispatch(name)
                    local callback = self.Selected[name]
                    callback(self)
                end
            end
            "#,
        );

        assert!(has_inferred_receiver(&ws, server_target, "MIXIN.Run"));
        assert!(!has_inferred_receiver(&ws, client_target, "MIXIN.Run"));
    }

    #[test]
    fn included_mixin_edit_refreshes_caller_contributions() {
        let mut ws = gmod_workspace();
        let target_uri = ws
            .virtual_url_generator
            .new_uri("lua/mixins/incremental_edit.lua");
        let target_file_id = ws
            .analysis
            .update_file_by_uri(
                &target_uri,
                Some(
                    r#"
                    local MIXIN = {}
                    function MIXIN:Before()
                        self:OnlyOnReceiver()
                    end
                    return MIXIN
                    "#
                    .to_string(),
                ),
            )
            .expect("target file must be created");
        ws.def_file(
            "lua/autorun/incremental_edit_consumer.lua",
            r#"
            local PANEL = {}
            function PANEL:OnlyOnReceiver() end
            function PANEL:Load()
                self.Selected = include("mixins/incremental_edit.lua")
            end
            function PANEL:Dispatch(name)
                local callback = self.Selected[name]
                callback(self)
            end
            "#,
        );

        let before_signature = signature_ids_for_path(&ws, target_file_id, "MIXIN.Before")[0];
        let before_receiver_slot = ws
            .analysis
            .compilation
            .get_db()
            .get_signature_index()
            .get(&before_signature)
            .expect("initial signature must exist")
            .params
            .len();
        assert!(has_inferred_param(
            &ws,
            before_signature,
            before_receiver_slot
        ));

        ws.analysis.update_file_by_uri(
            &target_uri,
            Some(
                r#"
                local MIXIN = {}
                function MIXIN:After()
                    self:OnlyOnReceiver()
                end
                return MIXIN
                "#
                .to_string(),
            ),
        );

        assert!(!has_inferred_param(
            &ws,
            before_signature,
            before_receiver_slot
        ));
        assert!(has_inferred_receiver(&ws, target_file_id, "MIXIN.After"));
    }

    #[test]
    fn included_mixin_delete_and_reopen_refreshes_caller_contributions() {
        let mut ws = gmod_workspace();
        let target_uri = ws
            .virtual_url_generator
            .new_uri("lua/mixins/incremental_reopen.lua");
        let target_content = r#"
            local MIXIN = {}
            function MIXIN:Run()
                self:OnlyOnReceiver()
            end
            return MIXIN
        "#;
        let target_file_id = ws
            .analysis
            .update_file_by_uri(&target_uri, Some(target_content.to_string()))
            .expect("target file must be created");
        ws.def_file(
            "lua/autorun/incremental_reopen_consumer.lua",
            r#"
            local PANEL = {}
            function PANEL:OnlyOnReceiver() end
            function PANEL:Load()
                self.Selected = include("mixins/incremental_reopen.lua")
            end
            function PANEL:Dispatch(name)
                local callback = self.Selected[name]
                callback(self)
            end
            "#,
        );

        let original_signature = signature_ids_for_path(&ws, target_file_id, "MIXIN.Run")[0];
        let receiver_slot = ws
            .analysis
            .compilation
            .get_db()
            .get_signature_index()
            .get(&original_signature)
            .expect("initial signature must exist")
            .params
            .len();
        assert!(has_inferred_param(&ws, original_signature, receiver_slot));

        ws.analysis
            .remove_file_by_uri(&target_uri)
            .expect("target file must be removed");
        assert!(!has_inferred_param(&ws, original_signature, receiver_slot));

        let reopened_file_id = ws
            .analysis
            .update_file_by_uri(&target_uri, Some(target_content.to_string()))
            .expect("target file must reopen");
        assert!(has_inferred_receiver(&ws, reopened_file_id, "MIXIN.Run"));
    }

    #[test]
    fn compilefile_mixin_delete_and_reopen_refreshes_caller_contributions() {
        let mut ws = gmod_workspace();
        let target_uri = ws
            .virtual_url_generator
            .new_uri("lua/mixins/compilefile_reopen.lua");
        let target_content = r#"
            local MIXIN = {}
            function MIXIN:Run()
                self:OnlyOnReceiver()
            end
            return MIXIN
        "#;
        let target_file_id = ws
            .analysis
            .update_file_by_uri(&target_uri, Some(target_content.to_string()))
            .expect("target file must be created");
        ws.def_file(
            "lua/autorun/compilefile_reopen_consumer.lua",
            r#"
            local PANEL = {}
            function PANEL:OnlyOnReceiver() end
            function PANEL:Load()
                self.Selected = CompileFile("mixins/compilefile_reopen.lua")()
            end
            function PANEL:Dispatch(name)
                local callback = self.Selected[name]
                callback(self)
            end
            "#,
        );

        let original_signature = signature_ids_for_path(&ws, target_file_id, "MIXIN.Run")[0];
        let receiver_slot = ws
            .analysis
            .compilation
            .get_db()
            .get_signature_index()
            .get(&original_signature)
            .expect("initial signature must exist")
            .params
            .len();
        assert!(has_inferred_param(&ws, original_signature, receiver_slot));

        ws.analysis
            .remove_file_by_uri(&target_uri)
            .expect("target file must be removed");
        assert!(!has_inferred_param(&ws, original_signature, receiver_slot));

        let reopened_file_id = ws
            .analysis
            .update_file_by_uri(&target_uri, Some(target_content.to_string()))
            .expect("target file must reopen");
        assert!(has_inferred_receiver(&ws, reopened_file_id, "MIXIN.Run"));
    }
}
