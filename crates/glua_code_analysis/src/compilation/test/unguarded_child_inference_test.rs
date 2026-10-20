#[cfg(test)]
mod property_callback_guard_test {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};
    use tokio_util::sync::CancellationToken;

    const API: &str = r#"
        ---@class Entity
        ---@field GetClass fun(self: Entity): string
        ---@class NULL: Entity
        ---@class Player: Entity
        ---@class ix_vendor: Entity
        ---@field receivers Player[]
        ---@field SetDescription fun(self: ix_vendor, description: string)
        ---@class ix_container: Entity
        ---@field receivers Player[]
        ---@field SetLocked fun(self: ix_container, locked: boolean)
        ---@field SetDisplayName fun(self: ix_container, name: string)
        ---@class PropertyAdd
        ---@field Filter fun(self: PropertyAddRuntime, entity: Entity, client: Player): boolean
        ---@field Receive? fun(self: PropertyAddRuntime, length: number, client: Player)
        ---@class PropertyAddRuntime: PropertyAdd
        local PropertyAddRuntime = {}
        properties = {}
        ---@param name string
        ---@param data PropertyAdd
        function properties.Add(name, data) end
        ---@param value any
        ---@return TypeGuard<any>
        ---@return_cast value -NULL
        ---@[valid_guard]
        function IsValid(value) end
        CAMI = {}
        ---@param client Player
        ---@return boolean?
        ---@return string?
        function CAMI.PlayerHasAccess(client) end
        ---@[call_arg("gmod.net_message", "receive")]
        ---@param messageName string
        ---@[call_arg("gmod.net_message", "callback")]
        ---@param callback fun(length: number, client: Player)
        function net.Receive(messageName, callback) end
    "#;
    const PATH: &str = "gamemodes/helix/plugins/vendor/sh_plugin.lua";

    fn source(filter: &str, before_call: &str, receiver: &str) -> String {
        format!(
            r#"
            properties.Add("vendor_edit", {{
                Filter = function(self, entity, client)
                    {filter}
                end,
                Receive = function(self, length, client)
                    local entity = net.ReadEntity()
                    if not IsValid(entity) then return end
                    {before_call}
                    if not {receiver}:Filter(entity, client) then return end
                    entity.receivers[#entity.receivers + 1] = client
                    client.ixVendor = entity
                end,
            }})
            if SERVER then
                net.Receive("ixVendorEdit", function(length, client)
                    local entity = client.ixVendor
                    if not IsValid(entity) then return end
                    entity:SetDescription("changed")
                end)
            end
        "#
        )
    }

    const FILTER: &str = r#"
        if not IsValid(entity) then return false end
        if entity:GetClass() ~= "ix_vendor" then return false end
        return CAMI.PlayerHasAccess(client)
    "#;

    fn workspace(source: &str) -> (VirtualWorkspace, crate::FileId) {
        let mut ws = VirtualWorkspace::new();
        let mut config = Emmyrc::default();
        config.gmod.enabled = true;
        ws.update_emmyrc(config);
        ws.def_gmod_call_arg_builtins();
        ws.def_file("lua/includes/property_callback_api.lua", API);
        let file = ws.def_file(PATH, source);
        (ws, file)
    }

    fn reports(ws: &mut VirtualWorkspace, file: crate::FileId) -> Vec<lsp_types::Diagnostic> {
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::InferUnguardedChild);
        ws.analysis
            .diagnose_file(file, CancellationToken::new())
            .unwrap_or_default()
    }

    #[test]
    fn a_predicate_with_a_boolean_first_result_can_also_return_a_reason() {
        let text = r#"
            ---@param entity Entity
            ---@param client Player
            ---@return boolean
            ---@return string?
            local function IsVendor(entity, client)
                if entity:GetClass() ~= "ix_vendor" then return false end
                return CAMI.PlayerHasAccess(client)
            end
            ---@param entity Entity
            ---@param client Player
            local function edit(entity, client)
                if not IsVendor(entity, client) then return end
                entity:SetDescription("changed")
            end
        "#;
        let (mut ws, file) = workspace(text);
        assert!(reports(&mut ws, file).is_empty());
    }

    #[test]
    fn property_callback_guard_does_not_survive_receiver_or_method_replacement() {
        for (before_call, receiver) in [
            (
                "self.Filter = function(self, entity, client) return true end",
                "self",
            ),
            (
                "local alias = self\nalias.Filter = function(self, entity, client) return true end",
                "self",
            ),
            (
                "---@type PropertyAddRuntime\nlocal other\nself = other",
                "self",
            ),
            ("---@type PropertyAddRuntime\nlocal other", "other"),
        ] {
            let text = source(FILTER, before_call, receiver);
            let (mut ws, file) = workspace(&text);
            assert!(!reports(&mut ws, file).is_empty(), "{before_call}");
        }
    }
}

#[cfg(test)]
mod dynamic_marker_value_test {
    use crate::VirtualWorkspace;

    #[test]
    fn literal_field_comparisons_keep_every_possible_union_arm() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "gamemodes/helix/gamemode/core/hooks/sv_literal_guard.lua",
            r#"
            ---@class MatchingA
            ---@field kind "selected"
            ---@class MatchingB
            ---@field kind "selected"
            ---@class Possible
            ---@field kind string
            ---@class Different
            ---@field kind "other"
            ---@param value MatchingA|MatchingB|Possible|Different
            local function inspect(value)
                if value.kind == "selected" then
                    on_equal = value
                else
                    on_different = value
                end
            end
        "#,
        );
        assert_eq!(
            ws.expr_ty("on_equal"),
            ws.ty("MatchingA|MatchingB|Possible")
        );
        assert_eq!(ws.expr_ty("on_different"), ws.ty("Possible|Different"));
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashSet;

    use crate::{DiagnosticCode, Emmyrc, SemanticInfoOrigin, VirtualWorkspace};
    use glua_parser::{LuaAstNode, LuaExpr, LuaIndexExpr, LuaLocalName, LuaNameExpr};
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    fn enable_gmod(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
    }

    const ABSENCE_EXIT_RECEIVER_API: &str = r#"
        ---@class ExitReceiverBase
        ---@field Existing boolean
        local ExitReceiverBase = {}
        ---@class ExitReceiverChild: ExitReceiverBase
        ---@field Capability boolean
        ---@field ChildOnly fun(self: ExitReceiverChild)
        local ExitReceiverChild = {}
        ---@return ExitReceiverBase
        function exit_receiver() end
    "#;

    #[test]
    fn absence_exit_on_a_child_member_covers_a_different_child_member() {
        for body in [
            "if not pnl.Capability then return end pnl:ChildOnly()",
            "while true do if not pnl.Capability then continue end pnl:ChildOnly() end",
            "if false then elseif not pnl.Capability then return end pnl:ChildOnly()",
        ] {
            let mut ws = VirtualWorkspace::new();
            enable_gmod(&mut ws);
            ws.def(ABSENCE_EXIT_RECEIVER_API);
            let id = ws.def_file(
                "lua/autorun/client/absence_exit.lua",
                &format!("local function run() local pnl = exit_receiver() {body} end"),
            );
            assert_eq!(
                diagnostic_count(&mut ws, id, DiagnosticCode::InferUnguardedChild),
                0,
                "{body}"
            );
        }
    }

    #[test]
    fn a_goto_to_a_later_label_carries_the_state_before_a_bypassed_cast() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "lua/autorun/shared/goto_cast.lua",
            r#"
            ---@param value string|number
            ---@param skip boolean
            local function run(value, skip)
                if skip then goto after_cast end
                ---@cast value string
                ::after_cast::
                goto_cast_result = value
            end
        "#,
        );
        assert_eq!(ws.expr_ty("goto_cast_result"), ws.ty("string|number"));
    }

    #[test]
    fn absence_exit_does_not_cover_declared_guards_reassignments_labels_or_closures() {
        for body in [
            "if not pnl.Existing then return end pnl:ChildOnly()",
            "local other = exit_receiver() if not other.Capability then return end pnl:ChildOnly()",
            "if not pnl.Capability then return end pnl = exit_receiver() pnl:ChildOnly()",
            "if skip then goto after_guard end if not pnl.Capability then return end ::after_guard:: pnl:ChildOnly()",
            "local function probe() if not pnl.Capability then return end end pnl:ChildOnly()",
            "if skip then elseif not pnl.Capability then return end pnl:ChildOnly()",
        ] {
            let mut ws = VirtualWorkspace::new();
            enable_gmod(&mut ws);
            ws.def(ABSENCE_EXIT_RECEIVER_API);
            let id = ws.def_file(
                "lua/autorun/client/absence_exit_control.lua",
                &format!("local function run() local pnl = exit_receiver() {body} end"),
            );
            assert_eq!(
                diagnostic_count(&mut ws, id, DiagnosticCode::InferUnguardedChild),
                1,
                "{body}"
            );
        }
    }

    #[test]
    fn absence_exit_parent_coverage_requires_a_compatible_indexed_child_owner() {
        for guard in ["pnl[key]", "pnl.UnownedCapability", "pnl.OtherCapability"] {
            let mut ws = VirtualWorkspace::new();
            enable_gmod(&mut ws);
            ws.def(ABSENCE_EXIT_RECEIVER_API);
            ws.def(
                r#"
                ---@class DisjointExitReceiver: ExitReceiverBase
                ---@field OtherCapability boolean
                local DisjointExitReceiver = {}
            "#,
            );
            let id = ws.def_file(
                "lua/autorun/client/absence_exit_owner.lua",
                &format!(
                    r#"
                ---@param pnl ExitReceiverBase
                ---@param key string
                local function run(pnl, key)
                    if not {guard} then return end
                    pnl:ChildOnly()
                end
            "#
                ),
            );
            {
                let model = ws.analysis.compilation.get_semantic_model(id).unwrap();
                let read = model
                    .get_root()
                    .descendants::<LuaIndexExpr>()
                    .find(|expr| expr.syntax().text() == "pnl:ChildOnly")
                    .unwrap();
                assert!(
                    !crate::compilation::analyzer::is_guarded_by_enclosing_condition(
                        model.get_db(),
                        &mut model.get_cache().borrow_mut(),
                        &read,
                    ),
                    "{guard}"
                );
            }
            let child = diagnostic_count(&mut ws, id, DiagnosticCode::InferUnguardedChild);
            let missing = diagnostic_count(&mut ws, id, DiagnosticCode::UndefinedMethod);
            assert!(child + missing > 0, "{guard}");
        }
    }

    #[test]
    fn spawnicon_capability_exit_covers_its_model_member_in_a_selection_loop() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        enable_gmod(&mut ws);
        ws.def(
            r#"
            ---@class Panel
            local Panel = {}
            ---@class SpawnIcon: Panel
            ---@field InternalAddResizeMenu fun(self: SpawnIcon)
            ---@field GetModelName fun(self: SpawnIcon): string
            local SpawnIcon = {}
        "#,
        );
        let id = ws.def_file(
            "gamemodes/sandbox/gamemode/spawnmenu/creationmenu/content/contentcontainer.lua",
            r#"
            ---@param selection Panel[]
            local function resize(selection)
                for _, pnl in pairs(selection) do
                    if not pnl.InternalAddResizeMenu then continue end
                    local model = pnl:GetModelName()
                end
            end
        "#,
        );
        let model = ws.analysis.compilation.get_semantic_model(id).unwrap();
        let read = model
            .get_root()
            .descendants::<LuaIndexExpr>()
            .find(|expr| expr.syntax().text() == "pnl:GetModelName")
            .unwrap();
        assert!(
            crate::compilation::analyzer::is_guarded_by_enclosing_condition(
                model.get_db(),
                &mut model.get_cache().borrow_mut(),
                &read,
            )
        );
        assert_eq!(
            diagnostic_count(&mut ws, id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn absence_exit_coverage_tracks_edits_delete_and_reopen() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def(ABSENCE_EXIT_RECEIVER_API);
        let path = "lua/autorun/client/absence_exit_incremental.lua";
        let guarded = "local function run() local pnl = exit_receiver() if not pnl.Capability then return end pnl:ChildOnly() end";
        let unguarded = "local function run() local pnl = exit_receiver() if not pnl.Existing then return end pnl:ChildOnly() end";
        for (source, expected) in [(guarded, 0), (unguarded, 1), (guarded, 0)] {
            let id = ws.def_file(path, source);
            assert_eq!(
                diagnostic_count(&mut ws, id, DiagnosticCode::InferUnguardedChild),
                expected
            );
        }
        let id = ws.def_file(path, guarded);
        let uri = ws.analysis.get_uri(id).unwrap();
        ws.analysis
            .remove_file_by_uri(&uri)
            .0
            .expect("removed guard consumer");
        let id = ws.def_file(path, unguarded);
        assert_eq!(
            diagnostic_count(&mut ws, id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    const CONTEXTUAL_RECEIVER_API: &str = r#"
        ---@class Panel
        local Panel = {}
        ---@class DPanel: Panel
        ---@field SetBackgroundColor fun(self: DPanel, color: any)
        local DPanel = {}
        ---@class DLabel: Panel
        local DLabel = {}
        ---@class DScrollPanel: Panel
        ---@field Add fun(self: DScrollPanel, name: string): DPanel
        local DScrollPanel = {}
        ---@return Panel
        function contextual_panel() end
    "#;

    fn contextual_receiver_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        enable_gmod(&mut ws);
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "lua/annotations/contextual_receiver.lua",
            CONTEXTUAL_RECEIVER_API,
        );
        ws
    }

    #[test]
    fn a_contextually_guessed_local_reports_usage_hints_for_child_and_missing_members() {
        for member in ["SetBackgroundColor", "Nonexistent"] {
            let mut ws = contextual_receiver_workspace();
            let source = format!(
                r#"
                local function update(listPanel)
                    local holder = listPanel:Add("DPanel")
                    holder:{member}({{}})
                    local label = vgui.Create("DLabel", holder)
                end
            "#
            );
            let id = ws.def_file("lua/autorun/client/contextual_receiver.lua", &source);
            assert_eq!(
                diagnostic_count(&mut ws, id, DiagnosticCode::InferUnguardedChild),
                0,
                "{member}"
            );
            assert_eq!(
                diagnostic_count(&mut ws, id, DiagnosticCode::UndefinedMethod),
                0,
                "{member}"
            );
            let hints = diagnostics_for(&mut ws, id, DiagnosticCode::InferUnknown);
            let read_line = source
                .lines()
                .position(|line| line.contains(&format!("holder:{member}")))
                .unwrap() as u32;
            assert!(
                hints.iter().any(|hint| hint.range.start.line == read_line),
                "{member}: {hints:?}"
            );
        }
    }

    #[test]
    fn concrete_declared_inferred_and_reassigned_panel_locals_keep_child_advisories() {
        for setup in [
            "---@type Panel\nlocal holder",
            "local holder = contextual_panel()",
            "local holder = vgui.Create(\"Panel\")",
            "local holder = listPanel:Add(\"DPanel\") holder = contextual_panel()",
        ] {
            let mut ws = contextual_receiver_workspace();
            let id = ws.def_file(
                "lua/autorun/client/contextual_receiver_control.lua",
                &format!(
                    r#"
                local function update(listPanel)
                    {setup}
                    holder:SetBackgroundColor({{}})
                    local label = vgui.Create("DLabel", holder)
                end
            "#
                ),
            );
            assert_eq!(
                diagnostic_count(&mut ws, id, DiagnosticCode::InferUnguardedChild),
                1,
                "{setup}"
            );
        }
    }

    #[test]
    fn a_type_test_on_a_guessed_local_keeps_child_advisories() {
        let mut ws = contextual_receiver_workspace();
        ws.def_gmod_type_predicates();
        let id = ws.def_file(
            "lua/autorun/client/contextual_receiver_type_test.lua",
            r#"
            local function update(listPanel)
                local holder = listPanel:Add("DPanel")
                if ispanel(holder) then
                    holder:SetBackgroundColor({})
                end
                local label = vgui.Create("DLabel", holder)
            end
        "#,
        );
        assert_eq!(
            diagnostic_count(&mut ws, id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn explicit_casts_of_a_guessed_local_restore_concrete_member_diagnostics() {
        for (read, expected) in [
            ("holder:Nonexistent()", DiagnosticCode::UndefinedMethod),
            (
                "local value = holder.Nonexistent",
                DiagnosticCode::UndefinedField,
            ),
            (
                "holder:SetBackgroundColor({})",
                DiagnosticCode::InferUnguardedChild,
            ),
        ] {
            let mut ws = contextual_receiver_workspace();
            let source = format!(
                r#"
                local function update(listPanel)
                    local holder = listPanel:Add("DPanel")
                    ---@cast holder Panel
                    {read}
                    local label = vgui.Create("DLabel", holder)
                end
            "#
            );
            let id = ws.def_file("lua/autorun/client/contextual_receiver_cast.lua", &source);
            assert_eq!(diagnostic_count(&mut ws, id, expected), 1, "{read}");
            let read_line = source.lines().position(|line| line.trim() == read).unwrap() as u32;
            assert!(
                !diagnostics_for(&mut ws, id, DiagnosticCode::InferUnknown)
                    .iter()
                    .any(|hint| hint.range.start.line == read_line),
                "{read}"
            );
        }
    }

    #[test]
    fn removal_and_falsy_only_casts_do_not_establish_a_guessed_receivers_class() {
        for cast in [
            "-string", "-NULL", "-nil", "-?", "+NULL", "+false", "+nil", "+?",
        ] {
            for read in [
                "holder:Nonexistent()",
                "local value = holder.Nonexistent",
                "holder:SetBackgroundColor({})",
            ] {
                let mut ws = contextual_receiver_workspace();
                ws.def("---@class NULL\nlocal NULL = {}");
                let source = format!(
                    r#"
                    local function update(listPanel)
                        local holder = listPanel:Add("DPanel")
                        ---@cast holder {cast}
                        {read}
                        local label = vgui.Create("DLabel", holder)
                    end
                "#
                );
                let id = ws.def_file(
                    "lua/autorun/client/contextual_receiver_removal.lua",
                    &source,
                );
                for code in [
                    DiagnosticCode::UndefinedMethod,
                    DiagnosticCode::UndefinedField,
                    DiagnosticCode::InferUnguardedChild,
                ] {
                    assert_eq!(
                        diagnostic_count(&mut ws, id, code),
                        0,
                        "{cast}: {read}: {code:?}"
                    );
                }
                let read_line = source.lines().position(|line| line.trim() == read).unwrap() as u32;
                assert!(
                    diagnostics_for(&mut ws, id, DiagnosticCode::InferUnknown)
                        .iter()
                        .any(|hint| hint.range.start.line == read_line),
                    "{cast}: {read}"
                );
            }
        }
    }

    #[test]
    fn unrelated_and_nullable_only_casts_leave_a_local_receiver_guessed() {
        for cast in ["---@cast other Panel", "---@cast holder -?"] {
            let mut ws = contextual_receiver_workspace();
            let source = format!(
                r#"
                local function update(listPanel)
                    local holder = listPanel:Add("DPanel")
                    local other = contextual_panel()
                    {cast}
                    holder:Nonexistent()
                    local label = vgui.Create("DLabel", holder)
                end
            "#
            );
            let id = ws.def_file(
                "lua/autorun/client/contextual_receiver_other_cast.lua",
                &source,
            );
            assert_eq!(
                diagnostic_count(&mut ws, id, DiagnosticCode::UndefinedMethod),
                0,
                "{cast}"
            );
            let read_line = source
                .lines()
                .position(|line| line.contains("holder:Nonexistent"))
                .unwrap() as u32;
            assert!(
                diagnostics_for(&mut ws, id, DiagnosticCode::InferUnknown)
                    .iter()
                    .any(|hint| hint.range.start.line == read_line),
                "{cast}"
            );
        }
    }

    #[test]
    fn contextual_receiver_hints_track_guessing_use_edits_delete_and_reopen() {
        let mut ws = contextual_receiver_workspace();
        let path = "lua/autorun/client/contextual_receiver_incremental.lua";
        let source = r#"
            local function update(listPanel)
                local holder = listPanel:Add("DPanel")
                holder:SetBackgroundColor({})
                local label = vgui.Create("DLabel", holder)
            end
        "#;
        let without_guess = source.replace("local label = vgui.Create(\"DLabel\", holder)", "");
        let typed = source.replace(
            "local function update(listPanel)",
            "---@param listPanel DScrollPanel\nlocal function update(listPanel)",
        );
        for (text, expected_hints) in [
            (source, true),
            (without_guess.as_str(), false),
            (source, true),
            (typed.as_str(), false),
        ] {
            let id = ws.def_file(path, text);
            assert_eq!(
                diagnostic_count(&mut ws, id, DiagnosticCode::InferUnguardedChild),
                0
            );
            let hints = diagnostics_for(&mut ws, id, DiagnosticCode::InferUnknown);
            assert_eq!(!hints.is_empty(), expected_hints, "{text}");
        }
        let id = ws.def_file(path, source);
        let uri = ws.analysis.get_uri(id).unwrap();
        ws.analysis
            .remove_file_by_uri(&uri)
            .0
            .expect("removed guessed receiver consumer");
        let id = ws.def_file(path, source);
        assert_eq!(
            diagnostic_count(&mut ws, id, DiagnosticCode::InferUnguardedChild),
            0
        );
        assert!(!diagnostics_for(&mut ws, id, DiagnosticCode::InferUnknown).is_empty());
    }

    fn last_name_type(ws: &VirtualWorkspace, file_id: crate::FileId, name: &str) -> crate::LuaType {
        last_name_info(ws, file_id, name).display_typ().clone()
    }

    fn nth_name_type_from_end(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
        name: &str,
        nth_from_end: usize,
    ) -> crate::LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let name_exprs = semantic_model
            .get_root()
            .descendants::<LuaNameExpr>()
            .filter(|expr| expr.get_name_text().as_deref() == Some(name))
            .collect::<Vec<_>>();
        let name_expr = name_exprs
            .into_iter()
            .rev()
            .nth(nth_from_end)
            .expect("name expression");
        semantic_model
            .get_semantic_info(name_expr.syntax().clone().into())
            .expect("semantic info")
            .display_typ()
            .clone()
    }

    fn last_name_info(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
        name: &str,
    ) -> crate::SemanticInfo {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let name_expr = semantic_model
            .get_root()
            .descendants::<LuaNameExpr>()
            .filter(|expr| expr.get_name_text().as_deref() == Some(name))
            .last()
            .expect("name expression");
        semantic_model
            .get_semantic_info(name_expr.syntax().clone().into())
            .expect("semantic info")
    }

    fn diagnostic_count(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        code: DiagnosticCode,
    ) -> usize {
        diagnostics_for(ws, file_id, code).len()
    }

    fn diagnostics_for(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        code: DiagnosticCode,
    ) -> Vec<lsp_types::Diagnostic> {
        ws.analysis.diagnostic.enable_only(code);
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .iter()
            .filter(|diagnostic| {
                diagnostic.code == Some(NumberOrString::String(code.get_name().to_string()))
            })
            .cloned()
            .collect()
    }

    fn member_path_receiver_types(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
        path_text: &str,
    ) -> Vec<String> {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        semantic_model
            .get_root()
            .descendants::<LuaIndexExpr>()
            .filter_map(|index_expr| match index_expr.get_prefix_expr() {
                Some(LuaExpr::IndexExpr(receiver))
                    if receiver.syntax().text().to_string().trim() == path_text =>
                {
                    semantic_model.infer_expr(LuaExpr::IndexExpr(receiver)).ok()
                }
                _ => None,
            })
            .map(|typ| ws.humanize_type(typ))
            .collect()
    }

    #[derive(Debug, PartialEq, Eq)]
    struct NestedCallbackState {
        receiver_types: Vec<String>,
        undefined_methods: usize,
        unguarded_children: usize,
    }

    fn nested_callback_state(
        ws: &mut VirtualWorkspace,
        consumer_file_id: crate::FileId,
    ) -> NestedCallbackState {
        let undefined_methods =
            diagnostic_count(ws, consumer_file_id, DiagnosticCode::UndefinedMethod);
        let unguarded_children =
            diagnostic_count(ws, consumer_file_id, DiagnosticCode::InferUnguardedChild);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(consumer_file_id)
            .expect("consumer semantic model");
        let receiver_types = semantic_model
            .get_root()
            .descendants::<LuaIndexExpr>()
            .filter_map(|index_expr| match index_expr.get_prefix_expr() {
                Some(LuaExpr::IndexExpr(receiver))
                    if receiver.syntax().text().to_string().trim() == "data.proc" =>
                {
                    semantic_model.infer_expr(LuaExpr::IndexExpr(receiver)).ok()
                }
                _ => None,
            })
            .map(|typ| ws.humanize_type(typ))
            .collect();
        NestedCallbackState {
            receiver_types,
            undefined_methods,
            unguarded_children,
        }
    }

    fn fresh_nested_callback_state(producer: Option<&str>, consumer: &str) -> NestedCallbackState {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let mut files = Vec::new();
        if let Some(producer) = producer {
            files.push(("lua/autorun/shared/transfer.lua", producer));
        }
        files.push(("lua/autorun/client/consumer.lua", consumer));
        let consumer_file_id = ws
            .def_files(files)
            .into_iter()
            .find(|file_id| {
                ws.analysis
                    .compilation
                    .get_semantic_model(*file_id)
                    .is_some_and(|model| {
                        model
                            .get_root()
                            .syntax()
                            .text()
                            .to_string()
                            .contains("Transfer.Read(function")
                    })
            })
            .expect("consumer file");
        nested_callback_state(&mut ws, consumer_file_id)
    }

    #[test]
    fn unguarded_child_member_evidence_selects_player_and_resolves_member_result() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);

        let file_id = ws.def(
            r#"
            ---@class Vector

            ---@class Entity
            ---@class Player: Entity
            ---@field GetShootPos fun(self: Player): Vector
            ---@field IsActiveCannibal fun(self: Player): boolean
            ---@field IsRoleAbilityDisabled fun(self: Player): boolean
            ---@class Cannibal: Entity
            ---@field IsActiveCannibal fun(self: Cannibal): boolean

            ---@type Entity
            local owner
            local spos = owner:GetShootPos()
            local active = owner:IsActiveCannibal()
            local disabled = owner:IsRoleAbilityDisabled()
            print(owner)
            print(spos)
            "#,
        );

        let owner_type = last_name_type(&ws, file_id, "owner");
        let spos_type = last_name_type(&ws, file_id, "spos");
        let owner_info = last_name_info(&ws, file_id, "owner");
        assert_eq!(ws.humanize_type(owner_type), "Player");
        assert_eq!(owner_info.origin, SemanticInfoOrigin::ContextualExpected);
        assert_eq!(ws.humanize_type(spos_type), "Vector");
    }

    #[test]
    fn guarded_nested_member_receiver_selects_scripted_entity_child() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@attribute self_guard(member: string)

            ---@class Entity
            ---@class starfall_processor: Entity
            ---@field Compile fun(self: starfall_processor, data: table)
            ---@field Error fun(self: starfall_processor, err: table)

            ---@return boolean
            ---@return_cast self Entity
            ---@[self_guard("gmod.entity")]
            function Entity:IsValid() end

            ---@generic T
            ---@param name `T`
            ---@return T
            function FindMetaTable(name) end

            local Ent_IsValid = FindMetaTable("Entity").IsValid
            local function Read(callback)
                ---@type unknown
                local proc
                callback({ proc = proc })
                callback({ owner = proc })
            end

            Read(function(sfdata)
                if not Ent_IsValid(sfdata.proc) then return end
                sfdata.proc:Compile(sfdata)
                sfdata.proc:Error({ message = "failed" })
            end)
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            0,
            "nested Entity receiver should use child-only methods as scripted-class evidence"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1,
            "nested Entity receiver should report one heuristic child inference"
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let receiver_types = semantic_model
            .get_root()
            .descendants::<LuaIndexExpr>()
            .filter_map(|index_expr| match index_expr.get_prefix_expr() {
                Some(LuaExpr::IndexExpr(receiver)) if receiver.syntax().text() == "sfdata.proc" => {
                    semantic_model.infer_expr(LuaExpr::IndexExpr(receiver)).ok()
                }
                _ => None,
            })
            .map(|typ| ws.humanize_type(typ))
            .collect::<Vec<_>>();
        assert_eq!(
            receiver_types,
            vec!["starfall_processor", "starfall_processor"]
        );
    }

    #[test]
    fn cross_file_callback_preserves_entity_guard_for_nested_child_inference() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_ids = ws.def_files(vec![
            (
                "lua/autorun/shared/transfer.lua",
                r#"
                ---@attribute self_guard(member: string)
                ---@class Entity
                ---@class starfall_processor: Entity
                ---@field Compile fun(self: starfall_processor, data: table)
                ---@field Error fun(self: starfall_processor, err: table)

                Entity = {}
                ---@return boolean
                ---@return_cast self Entity
                ---@[self_guard("gmod.entity")]
                function Entity:IsValid() end

                ---@param value any
                ---@return TypeGuard<Entity>
                function isentity(value) end

                Transfer = {}
                function Transfer.Read(callback)
                    local data = {}
                    ---@type unknown
                    local proc
                    if isentity(proc) then data.proc = proc end
                    callback(true, data)
                end
                "#,
            ),
            (
                "lua/autorun/client/consumer.lua",
                r#"
                ---@generic T
                ---@param name `T`
                ---@return T
                function FindMetaTable(name) end

                local Ent_IsValid = FindMetaTable("Entity").IsValid
                Transfer.Read(function(ok, data)
                    if ok and Ent_IsValid(data.proc) then
                        data.proc:Compile(data)
                        data.proc:Error({ message = "failed" })
                    end
                end)
                "#,
            ),
        ]);
        let consumer_file_id = file_ids
            .into_iter()
            .find(|file_id| {
                ws.analysis
                    .compilation
                    .get_semantic_model(*file_id)
                    .is_some_and(|model| {
                        model
                            .get_root()
                            .syntax()
                            .text()
                            .to_string()
                            .contains("Transfer.Read")
                    })
            })
            .expect("consumer file");

        assert_eq!(
            diagnostic_count(&mut ws, consumer_file_id, DiagnosticCode::UndefinedMethod),
            0
        );
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(consumer_file_id)
            .expect("consumer semantic model");
        let receiver_types = semantic_model
            .get_root()
            .descendants::<LuaIndexExpr>()
            .filter_map(|index_expr| match index_expr.get_prefix_expr() {
                Some(LuaExpr::IndexExpr(receiver))
                    if receiver.syntax().text().to_string().trim() == "data.proc" =>
                {
                    semantic_model.infer_expr(LuaExpr::IndexExpr(receiver)).ok()
                }
                _ => None,
            })
            .map(|typ| ws.humanize_type(typ))
            .collect::<Vec<_>>();
        assert_eq!(
            receiver_types,
            vec!["starfall_processor", "starfall_processor"]
        );
        assert_eq!(
            diagnostic_count(
                &mut ws,
                consumer_file_id,
                DiagnosticCode::InferUnguardedChild
            ),
            1
        );
    }

    #[test]
    fn nested_callback_inference_matches_fresh_analysis_across_edit_delete_and_reopen() {
        const STRUCTURAL_PRODUCER: &str = r#"
            ---@attribute self_guard(member: string)
            ---@class Entity
            ---@class starfall_processor: Entity
            ---@field Compile fun(self: starfall_processor, data: table)
            ---@field Error fun(self: starfall_processor, err: table)

            Entity = {}
            ---@return boolean
            ---@return_cast self Entity
            ---@[self_guard("gmod.entity")]
            function Entity:IsValid() end

            ---@param value any
            ---@return TypeGuard<Entity>
            function isentity(value) end

            Transfer = {}
            function Transfer.Read(callback)
                local data = {}
                ---@type unknown
                local proc
                if isentity(proc) then data.proc = proc end
                callback(false, { owner = proc })
                callback(true, data)
            end
        "#;
        const NON_STRUCTURAL_PRODUCER: &str = r#"
            ---@attribute self_guard(member: string)
            ---@class Entity
            ---@class starfall_processor: Entity
            ---@field Compile fun(self: starfall_processor, data: table)
            ---@field Error fun(self: starfall_processor, err: table)

            Entity = {}
            ---@return boolean
            ---@return_cast self Entity
            ---@[self_guard("gmod.entity")]
            function Entity:IsValid() end

            Transfer = {}
            function Transfer.Read(callback)
                callback(true, nil)
            end
        "#;
        const CONSUMER: &str = r#"
            ---@generic T
            ---@param name `T`
            ---@return T
            function FindMetaTable(name) end

            local Ent_IsValid = FindMetaTable("Entity").IsValid
            Transfer.Read(function(ok, data)
                if ok and Ent_IsValid(data.proc) then
                    data.proc:Compile(data)
                    data.proc:Error({ message = "failed" })
                end
            end)
        "#;
        const EDITED_CONSUMER: &str = r#"
            ---@generic T
            ---@param name `T`
            ---@return T
            function FindMetaTable(name) end

            local unused = true
            local Ent_IsValid = FindMetaTable("Entity").IsValid
            Transfer.Read(function(ok, data)
                if ok and Ent_IsValid(data.proc) then
                    data.proc:Compile(data)
                    data.proc:Error({ message = "failed" })
                end
            end)
        "#;

        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let producer_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/shared/transfer.lua");
        let consumer_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/client/consumer.lua");
        ws.analysis
            .update_file_by_uri(&producer_uri, Some(STRUCTURAL_PRODUCER.to_string()))
            .map(|(id, _)| id)
            .expect("initial producer");
        let mut consumer_file_id = ws
            .analysis
            .update_file_by_uri(&consumer_uri, Some(CONSUMER.to_string()))
            .map(|(id, _)| id)
            .expect("initial consumer");
        assert_eq!(
            nested_callback_state(&mut ws, consumer_file_id),
            fresh_nested_callback_state(Some(STRUCTURAL_PRODUCER), CONSUMER)
        );

        ws.analysis
            .update_file_by_uri(&producer_uri, Some(NON_STRUCTURAL_PRODUCER.to_string()))
            .map(|(id, _)| id)
            .expect("producer without structural callback data");
        assert_eq!(
            nested_callback_state(&mut ws, consumer_file_id),
            fresh_nested_callback_state(Some(NON_STRUCTURAL_PRODUCER), CONSUMER)
        );

        ws.analysis
            .update_file_by_uri(&producer_uri, Some(STRUCTURAL_PRODUCER.to_string()))
            .map(|(id, _)| id)
            .expect("restored structural producer");
        assert_eq!(
            nested_callback_state(&mut ws, consumer_file_id),
            fresh_nested_callback_state(Some(STRUCTURAL_PRODUCER), CONSUMER)
        );

        consumer_file_id = ws
            .analysis
            .update_file_by_uri(&consumer_uri, Some(EDITED_CONSUMER.to_string()))
            .map(|(id, _)| id)
            .expect("edited consumer");
        assert_eq!(
            nested_callback_state(&mut ws, consumer_file_id),
            fresh_nested_callback_state(Some(STRUCTURAL_PRODUCER), EDITED_CONSUMER)
        );

        ws.analysis
            .remove_file_by_uri(&producer_uri)
            .0
            .expect("removed producer");
        assert_eq!(
            nested_callback_state(&mut ws, consumer_file_id),
            fresh_nested_callback_state(None, EDITED_CONSUMER)
        );

        ws.analysis
            .update_file_by_uri(&producer_uri, Some(STRUCTURAL_PRODUCER.to_string()))
            .map(|(id, _)| id)
            .expect("reopened producer");
        assert_eq!(
            nested_callback_state(&mut ws, consumer_file_id),
            fresh_nested_callback_state(Some(STRUCTURAL_PRODUCER), EDITED_CONSUMER)
        );

        ws.analysis
            .remove_file_by_uri(&consumer_uri)
            .0
            .expect("removed consumer");
        consumer_file_id = ws
            .analysis
            .update_file_by_uri(&consumer_uri, Some(CONSUMER.to_string()))
            .map(|(id, _)| id)
            .expect("reopened consumer");
        assert_eq!(
            nested_callback_state(&mut ws, consumer_file_id),
            fresh_nested_callback_state(Some(STRUCTURAL_PRODUCER), CONSUMER)
        );
    }

    #[test]
    fn nullable_parent_member_evidence_selects_child_and_resolves_member_result() {
        for parent_type in ["Entity|NULL", "Entity|nil"] {
            let mut ws = VirtualWorkspace::new();
            enable_gmod(&mut ws);

            let file_id = ws.def(&format!(
                r#"
                ---@class TraceResult

                ---@class Entity
                ---@class NULL: Entity
                ---@class Player: Entity
                ---@field GetEyeTrace fun(self: Player): TraceResult

                ---@type {parent_type}
                local owner
                local trace = owner:GetEyeTrace()
                print(owner)
                print(trace)
                "#,
            ));

            assert_eq!(
                ws.humanize_type(last_name_type(&ws, file_id, "owner")),
                "Player",
                "nullable parent {parent_type} should retain child inference"
            );
            assert_eq!(
                ws.humanize_type(last_name_type(&ws, file_id, "trace")),
                "TraceResult",
                "nullable parent {parent_type} should resolve the child method return"
            );
            assert_eq!(
                diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
                0,
                "nullable parent {parent_type} should resolve the child method"
            );
            assert_eq!(
                diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
                1,
                "nullable parent {parent_type} should keep the unguarded-child warning"
            );
        }
    }

    #[test]
    fn null_union_resolves_member_owned_by_parent_without_child_inference() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);

        let file_id = ws.def(
            r#"
            ---@class Vector

            ---@class Entity
            ---@field EyePos fun(self: Entity): Vector
            ---@class NULL: Entity

            ---@type Entity|NULL
            local owner
            local position = owner:EyePos()
            print(owner)
            print(position)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "owner")),
            "(Entity|NULL)"
        );
        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "position")),
            "Vector"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            0
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn nullable_concrete_class_resolves_its_member_without_child_inference() {
        for receiver_type in ["Player|NULL", "Player|nil"] {
            let mut ws = VirtualWorkspace::new();
            enable_gmod(&mut ws);

            let file_id = ws.def(&format!(
                r#"
                ---@class TraceResult

                ---@class Entity
                ---@class NULL: Entity
                ---@class Player: Entity
                ---@field GetEyeTrace fun(self: Player): TraceResult

                ---@type {receiver_type}
                local owner
                local trace = owner:GetEyeTrace()
                print(owner)
                print(trace)
                "#,
            ));

            assert_eq!(last_name_type(&ws, file_id, "owner"), ws.ty(receiver_type));
            assert_eq!(
                ws.humanize_type(last_name_type(&ws, file_id, "trace")),
                "TraceResult"
            );
            assert_eq!(
                diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
                0
            );
            assert_eq!(
                diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
                0
            );
        }
    }

    #[test]
    fn generated_finite_player_method_names_drive_child_inference() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);

        ws.def_file(
            "gamemodes/terrortown/gamemode/shared.lua",
            r#"
            ---@class Entity
            ---@class Player: Entity

            ROLE_MAX = 2
            ROLE_STRINGS = {
                [0] = "Innocent",
                [1] = "Killer",
                [2] = "Old Man",
            }
            "#,
        );

        ws.def_file(
            "gamemodes/terrortown/gamemode/player_ext_shd.lua",
            r#"
            ---@type Player
            local plymeta = {}

            for role = 0, ROLE_MAX do
                local name = string.gsub(ROLE_STRINGS[role], "%s+", "")
                plymeta["Get" .. name] = function(self)
                    return true
                end
                plymeta["Is" .. name] = plymeta["Get" .. name]
                plymeta["IsActive" .. name] = function(self)
                    return true
                end
            end
            "#,
        );

        let file_id = ws.def_file(
            "gamemodes/terrortown/entities/entities/ttt_crowbar/shared.lua",
            r#"
            ---@type Entity
            local activator
            local is_killer = activator:IsKiller()
            print(activator)
            print(is_killer)
            "#,
        );

        let player_id = ws
            .analysis
            .compilation
            .get_db()
            .get_type_index()
            .find_type_decl(file_id, "Player")
            .expect("Player type")
            .get_id();
        assert!(
            ws.analysis
                .compilation
                .get_db()
                .get_dynamic_field_index()
                .has_field(
                    &crate::DynamicFieldOwner::Type(player_id.clone()),
                    "IsKiller"
                ),
            "generated IsKiller dynamic field"
        );
        assert!(
            ws.analysis
                .compilation
                .get_db()
                .get_dynamic_field_index()
                .has_field(
                    &crate::DynamicFieldOwner::Type(player_id.clone()),
                    "IsOldMan"
                ),
            "generated names apply their constant string transform"
        );
        let mut cache = crate::LuaInferCache::new(file_id, Default::default());
        assert!(
            crate::semantic::resolve_dynamic_field_member(
                ws.analysis.compilation.get_db(),
                &mut cache,
                &crate::LuaType::Ref(player_id.clone()),
                &crate::LuaMemberKey::Name("IsKiller".into()),
                None,
            )
            .is_ok_and(|resolution| resolution.is_some()),
            "generated IsKiller dynamic field resolves"
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "activator")),
            "Player"
        );
        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "is_killer")),
            "true"
        );
    }

    #[test]
    fn unguarded_child_fact_refines_a_declared_parameter_for_hover_and_members() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field GetShootPos fun(self: Player): Vector
            ---@class Vector

            ---@param owner Entity
            local function use_owner(owner)
                local pos = owner:GetShootPos()
                print(owner)
                print(pos)
            end
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "owner")),
            "Player"
        );
        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "pos")),
            "Vector"
        );
    }

    #[test]
    fn unguarded_child_fact_rebinds_an_earlier_cross_file_local_type_cache_in_the_same_batch() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = false;
        ws.update_emmyrc(emmyrc);
        ws.def_files(vec![
            (
                "definitions.lua",
                r#"
                ---@class Vector
                ---@class Entity
                ---@class Player: Entity
                ---@field GetShootPos fun(self: Player): Vector

                SharedTable = {}
                "#,
            ),
            (
                "consumer.lua",
                r#"
                local cross_file_anchor = SharedTable

                ---@type Entity
                local owner
                local method = owner.GetShootPos
                local position = owner:GetShootPos()
                "#,
            ),
        ]);

        let definitions_file_id = ws
            .analysis
            .get_file_id(&ws.virtual_url_generator.new_uri("definitions.lua"))
            .expect("expected definitions file id");
        let consumer_file_id = ws
            .analysis
            .get_file_id(&ws.virtual_url_generator.new_uri("consumer.lua"))
            .expect("expected consumer file id");
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(consumer_file_id)
            .expect("expected consumer semantic model");
        let method_name = semantic_model
            .get_root()
            .descendants::<LuaLocalName>()
            .find(|local_name| {
                local_name
                    .get_name_token()
                    .is_some_and(|token| token.get_name_text() == "method")
            })
            .expect("expected local method name");
        let method_decl_id =
            crate::LuaDeclId::new(consumer_file_id, method_name.get_range().start());

        let db = ws.analysis.compilation.get_db();
        let cross_file_dependents = db
            .get_type_index()
            .files_with_cross_file_type_caches_referencing_files(&HashSet::from([
                definitions_file_id,
            ]));
        assert!(
            cross_file_dependents.contains(&consumer_file_id),
            "consumer must depend on a cross-file type cache for this stabilization regression"
        );

        let method_type = db
            .get_type_index()
            .get_type_cache(&method_decl_id.into())
            .expect("expected persistent type cache for local method")
            .as_type();
        assert!(
            method_type.is_function(),
            "late unguarded-child inference must rebind the earlier local method cache, got {method_type:?}"
        );
    }

    #[test]
    fn unguarded_child_ties_produce_a_deterministic_union() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Bravo: Base
            ---@field Shared fun(self: Bravo)
            ---@class Alpha: Base
            ---@field Shared fun(self: Alpha)
            ---@type Base
            local value
            value:Shared()
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "(Alpha|Bravo)"
        );
    }

    #[test]
    fn unguarded_child_does_not_refine_parent_member_override() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@field Shared fun(self: Base)
            ---@class Child: Base
            ---@field Shared fun(self: Child)
            ---@type Base
            local value
            value:Shared()
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
    }

    #[test]
    fn unguarded_child_uses_a_grandchild() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Middle: Base
            ---@class Leaf: Middle
            ---@field LeafOnly fun(self: Leaf)
            ---@type Base
            local value
            value:LeafOnly()
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Leaf"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            0
        );
    }

    #[test]
    fn truly_absent_child_member_stays_an_undefined_method() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field Present fun(self: Child)
            ---@type Base
            local value
            value:Absent()
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            1
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn unguarded_child_respects_member_realm_at_the_use_site() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def_file(
            "lua/autorun/server/sv_child.lua",
            r#"
            ---@class Base
            ---@class ClientChild: Base
            ---@realm client
            function ClientChild:ClientOnly() end
            ---@type Base
            local value
            value:ClientOnly()
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            1
        );
    }

    #[test]
    fn unguarded_child_evidence_does_not_cross_assignments() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class First: Base
            ---@field FirstOnly fun(self: First)
            ---@class Second: Base
            ---@field SecondOnly fun(self: Second)
            ---@type Base
            local value
            value:FirstOnly()
            value = value
            value:SecondOnly()
            print(value)
            "#,
        );

        // The self-assignment makes the second use a distinct reaching definition;
        // both regions receive independent child facts rather than one mixed score.
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            2
        );
    }

    #[test]
    fn later_concrete_assignment_overrides_declaration_child_fact_for_semantic_info() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@return Base
            local function make_base() end
            ---@type Base
            local value
            value:ChildOnly()
            value = make_base()
            print(value)
            "#,
        );

        let info = last_name_info(&ws, file_id, "value");
        assert_eq!(
            (ws.humanize_type(info.display_typ().clone()), info.origin),
            ("Base".to_string(), SemanticInfoOrigin::Actual)
        );
    }

    #[test]
    fn explicit_field_guard_suppresses_unguarded_child_provenance() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field OnlyChild fun(self: Child)
            ---@type Base
            local value
            if value.OnlyChild then
                value:OnlyChild()
                print(value)
            end
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn member_assignment_is_not_unguarded_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            value.ChildOnly = function() end
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn matching_short_circuit_member_guard_is_not_unguarded_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local result = value.ChildOnly and value:ChildOnly()
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn optional_member_fallback_is_not_unguarded_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly string
            ---@type Base
            local value
            local result = value.ChildOnly or "fallback"
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn guarded_method_and_optional_field_fallbacks_are_not_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@field GetClass fun(self: Base): string
            ---@class Child: Base
            ---@field GetPrintName fun(self: Child): string
            ---@field PrintName string
            ---@type Base
            local value
            local result = value.GetPrintName and value:GetPrintName() or value.PrintName or value:GetClass() or "..."
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn child_method_call_controlling_fallback_is_still_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child): string?
            ---@type Base
            local value
            local result = value:ChildOnly() or "fallback"
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn child_member_used_before_fallback_is_still_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly number
            ---@type Base
            local value
            local result = value.ChildOnly + 1 or 0
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn outer_fallback_does_not_suppress_deferred_closure_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly string
            ---@type Base
            local value
            local callback = (function()
                return value.ChildOnly
            end) or function() end
            print(callback)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn parenthesized_positive_short_circuit_guard_suppresses_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local result = (value.ChildOnly) and value:ChildOnly()
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn nested_positive_short_circuit_guard_suppresses_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local enabled = true
            local result = enabled and value.ChildOnly and value:ChildOnly()
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn deferred_closure_call_does_not_inherit_short_circuit_member_guard() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local callback = value.ChildOnly and function()
                value:ChildOnly()
            end
            print(callback)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn deferred_named_function_does_not_inherit_outer_member_guard() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            if value.ChildOnly then
                function callback()
                    value:ChildOnly()
                end
            end
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn deferred_local_function_does_not_inherit_outer_member_guard() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            if value.ChildOnly then
                local function callback()
                    value:ChildOnly()
                end
            end
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn deferred_closure_local_member_guard_remains_guarded() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local callback = function()
                if value.ChildOnly then
                    value:ChildOnly()
                end
            end
            print(callback)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Base"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn deferred_closure_keeps_concrete_assignment_before_creation() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            ---@type Child
            local child
            value = child
            local callback = function()
                value:ChildOnly()
            end
            print(callback)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(nth_name_type_from_end(&ws, file_id, "value", 1)),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn deferred_closure_does_not_capture_assignment_after_creation() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local callback = function()
                print(value)
            end
            ---@type Child
            local child
            value = child
            print(callback)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(nth_name_type_from_end(&ws, file_id, "value", 2)),
            "Base"
        );
    }

    #[test]
    fn nested_deferred_closure_ignores_each_outer_member_guard() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local outer = value.ChildOnly and function()
                local inner = value.ChildOnly and function()
                    value:ChildOnly()
                end
                print(inner)
            end
            print(outer)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn negated_short_circuit_member_check_does_not_guard_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local result = not value.ChildOnly and value:ChildOnly()
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn comparison_short_circuit_member_check_does_not_guard_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local result = value.ChildOnly == nil and value:ChildOnly()
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn table_short_circuit_member_check_does_not_guard_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field ChildOnly fun(self: Child)
            ---@type Base
            local value
            local result = { value.ChildOnly } and value:ChildOnly()
            print(result)
            print(value)
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn child_method_call_used_as_a_condition_is_still_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field IsReady fun(self: Child): boolean
            ---@type Base
            local value
            if value:IsReady() then
                print(value)
            end
            "#,
        );

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, file_id, "value")),
            "Child"
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn immutable_return_cast_guard_suppresses_closure_child_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field GetPlayerColor fun(self: Player): number

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@param ent Entity
            local function make(ent)
                if ent:IsPlayer() then
                    local direct = ent:GetPlayerColor()
                    local callback = function()
                        return ent:GetPlayerColor()
                    end
                    return direct, callback
                end
            end
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn a_member_read_a_guard_on_the_same_path_proves_present_is_not_an_unguarded_child() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_gmod_type_predicates();
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class prop_effect: Entity
            ---@field AttachedEntity Entity

            ---@param ent Entity
            local function target(ent)
                if IsValid(ent.AttachedEntity) then ent = ent.AttachedEntity end
                local other = IsValid(ent.AttachedEntity) and ent.AttachedEntity or ent
                return ent, other
            end

            ---@param ent Entity
            local function unguarded(ent)
                return ent.AttachedEntity
            end
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].range.start.line, 14);
    }

    #[test]
    fn a_guard_does_not_cover_a_read_after_its_root_is_replaced_or_shadowed() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_gmod_type_predicates();
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class prop_effect: Entity
            ---@field AttachedEntity Entity

            ---@param ent Entity
            ---@param other Entity
            local function replaced(ent, other, flag)
                if IsValid(ent.AttachedEntity) then
                    if flag then ent = other end
                    return ent.AttachedEntity
                end
            end

            ---@param ent Entity
            ---@param other Entity
            local function shadowed(ent, other)
                if IsValid(ent.AttachedEntity) then
                    local ent = other
                    return ent.AttachedEntity
                end
            end
            "#,
        );

        let lines = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild)
            .into_iter()
            .map(|diagnostic| diagnostic.range.start.line)
            .collect::<Vec<_>>();
        assert_eq!(lines, vec![10, 19]);
    }

    fn guarded_lines(ws: &mut VirtualWorkspace, code: &str) -> Vec<u32> {
        enable_gmod(ws);
        ws.def_gmod_type_predicates();
        let file_id = ws.def(code);
        diagnostics_for(ws, file_id, DiagnosticCode::InferUnguardedChild)
            .into_iter()
            .map(|diagnostic| diagnostic.range.start.line)
            .collect()
    }

    const ATTACHED: &str = r#"
            ---@class Entity
            ---@class prop_effect: Entity
            ---@field AttachedEntity Entity
            ---@class Panel
            ---@class DVScrollBar: Panel
            ---@field btnUp Panel
            ---@field btnDown Panel
    "#;

    #[test]
    fn a_guard_is_not_undone_by_a_write_in_a_closure_it_never_calls() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@param ent Entity
            ---@param other Entity
            local function target(ent, other)
                if IsValid(ent.AttachedEntity) then
                    local reset = function() ent = other end
                    return ent.AttachedEntity, reset
                end
            end
            "
            ),
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn a_guard_through_a_constant_key_covers_the_named_read() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@param ent Entity
            local function target(ent)
                local key = \"AttachedEntity\"
                if IsValid(ent[key]) then
                    return ent.AttachedEntity
                end
            end
            "
            ),
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn sibling_reads_inside_a_guard_are_covered_in_either_order() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@param parent Panel
            local function paint(parent)
                if IsValid(parent.btnUp) then
                    local down = parent.btnDown
                    local up = parent.btnUp
                    return up, down
                end
            end
            "
            ),
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn an_unguarded_read_through_a_nested_receiver_is_reported_after_a_guarded_one() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@class Holder
            ---@field panel Panel

            ---@param holder Holder
            local function paint(holder)
                if IsValid(holder.panel.btnUp) then
                    local up = holder.panel.btnUp
                end
                return holder.panel.btnDown
            end
            "
            ),
        );
        assert_eq!(lines, vec![17]);
    }

    #[test]
    fn a_type_guard_whose_alias_allows_nil_does_not_cover_its_reads() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@alias MaybeEntity Entity|nil
            ---@param value any
            ---@return TypeGuard<MaybeEntity>
            function isMaybeEntity(value) end

            ---@param ent Entity
            local function target(ent)
                if isMaybeEntity(ent.AttachedEntity) then
                    return ent.AttachedEntity
                end
            end
            "
            ),
        );
        assert_eq!(lines, vec![17]);
    }

    #[test]
    fn a_colon_presence_predicate_tests_its_receiver() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Probe
            ---@return TypeGuard<Probe>
            function Probe:Holds(other) end

            ---@param probe Probe
            local function check(probe, other)
                return probe:Holds(other.Child)
            end
            "#,
        );
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let call = semantic_model
            .get_root()
            .descendants::<glua_parser::LuaCallExpr>()
            .next()
            .expect("call");
        let target = crate::semantic::presence_guard_target(
            semantic_model.get_db(),
            &mut semantic_model.get_cache().borrow_mut(),
            &call,
        )
        .expect("a presence guard");
        assert_eq!(target.syntax().text().to_string(), "probe");
    }

    #[test]
    fn a_guard_on_the_read_path_covers_it_for_a_local_the_guard_reassigns() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@class Trace
            ---@field Entity Entity

            ---@param trace Trace
            local function click(trace)
                local ent = trace.Entity
                if IsValid(ent.AttachedEntity) then ent = ent.AttachedEntity end
                return ent
            end
            "
            ),
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn a_guard_on_a_member_the_base_type_has_does_not_cover_a_child_read() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            r#"
            ---@class Entity
            ---@field Enabled boolean
            ---@class Player: Entity
            ---@field PlayerOnly number

            ---@param ent Entity
            local function read(ent)
                if ent.Enabled then
                    return ent.PlayerOnly
                end
            end
            "#,
        );
        assert_eq!(lines, vec![9]);
    }

    #[test]
    fn a_short_circuit_guard_through_a_constant_key_covers_the_named_read() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@param ent Entity
            local function target(ent)
                local key = \"AttachedEntity\"
                return IsValid(ent[key]) and ent.AttachedEntity or ent
            end
            "
            ),
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn a_type_guard_whose_union_hides_a_nullable_alias_does_not_cover_its_reads() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@alias MaybeEntity Entity|nil
            ---@alias GuardedValue MaybeEntity|string
            ---@param value any
            ---@return TypeGuard<GuardedValue>
            function isMaybeEntity(value) end

            ---@param ent Entity
            local function target(ent)
                if isMaybeEntity(ent.AttachedEntity) then
                    return ent.AttachedEntity
                end
            end
            "
            ),
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
    }

    #[test]
    fn a_nested_inference_is_reported_at_a_read_of_the_child_it_names() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            r#"
            ---@class Panel
            ---@class Scroll: Panel
            ---@field up Panel
            ---@field down Panel
            ---@class Label: Panel
            ---@field text string
            ---@class Holder
            ---@field panel Panel

            ---@param holder Holder
            local function paint(holder)
                if IsValid(holder.panel.up) then
                    local up = holder.panel.up
                    local down = holder.panel.down
                end
                return holder.panel.text
            end
            "#,
        );
        assert!(!lines.contains(&16), "{lines:?}");
    }

    #[test]
    fn an_inherited_member_read_is_reported_for_the_child_that_inherits_it() {
        for (param, receiver) in [("holder Holder", "holder.panel"), ("panel Panel", "panel")] {
            let mut ws = VirtualWorkspace::new();
            enable_gmod(&mut ws);
            ws.def_gmod_type_predicates();
            let file_id = ws.def(&format!(
                r#"
            ---@class Panel
            ---@class Scroll: Panel
            ---@field up Panel
            ---@class SpecialScroll: Scroll
            ---@field special Panel
            ---@class Holder
            ---@field panel Panel

            ---@param {param}
            local function paint(holder, panel)
                local up = {receiver}.up
                return {receiver}.special, up
            end
            "#
            ));
            let reports = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild)
                .into_iter()
                .map(|diagnostic| (diagnostic.range.start.line, diagnostic.message))
                .collect::<Vec<_>>();
            assert_eq!(reports.len(), 1, "{receiver}: {reports:?}");
            assert_eq!(reports[0].0, 11, "{receiver}: {reports:?}");
            assert!(
                reports[0].1.contains("SpecialScroll"),
                "{receiver}: {reports:?}"
            );
        }
    }

    #[test]
    fn a_guard_on_a_member_path_base_field_does_not_cover_a_child_read() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            r#"
            ---@class Entity
            ---@field Enabled boolean
            ---@class Player: Entity
            ---@field PlayerOnly number
            ---@class Holder
            ---@field ent Entity

            ---@param holder Holder
            local function read(holder)
                if holder.ent.Enabled then
                    return holder.ent.PlayerOnly
                end
            end
            "#,
        );
        assert_eq!(lines, vec![11]);
    }

    #[test]
    fn a_type_guard_whose_multiline_alias_allows_nil_does_not_cover_its_reads() {
        let mut ws = VirtualWorkspace::new();
        let lines = guarded_lines(
            &mut ws,
            &format!(
                "{ATTACHED}
            ---@alias MaybeEntity
            ---| Entity
            ---| nil
            ---@param value any
            ---@return TypeGuard<MaybeEntity>
            function isMaybeEntity(value) end

            ---@param ent Entity
            local function target(ent)
                if isMaybeEntity(ent.AttachedEntity) then
                    return ent.AttachedEntity
                end
            end
            "
            ),
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
    }

    #[test]
    fn a_presence_guard_still_lets_its_reads_infer_the_child_for_sibling_members() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_gmod_type_predicates();
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@class DVScrollBar: Panel
            ---@field btnUp Panel
            ---@field btnDown Panel

            ---@param parent Panel
            local function paint(parent)
                if IsValid(parent.btnUp) then
                    local up = parent.btnUp
                    local down = parent.btnDown
                    return up, down
                end
            end
            "#,
        );

        for code in [
            DiagnosticCode::InferUnguardedChild,
            DiagnosticCode::UndefinedField,
        ] {
            let diagnostics = diagnostics_for(&mut ws, file_id, code);
            assert!(diagnostics.is_empty(), "{code:?}: {diagnostics:?}");
        }
    }

    #[test]
    fn an_unguarded_read_is_reported_even_when_a_guarded_read_comes_first() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_gmod_type_predicates();
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@class DVScrollBar: Panel
            ---@field btnUp Panel
            ---@field btnDown Panel

            ---@param parent Panel
            local function paint(parent)
                if IsValid(parent.btnUp) then
                    local up = parent.btnUp
                end
                return parent.btnDown
            end
            "#,
        );

        let lines = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild)
            .into_iter()
            .map(|diagnostic| diagnostic.range.start.line)
            .collect::<Vec<_>>();
        assert_eq!(lines, vec![11]);
    }

    #[test]
    fn mutable_return_cast_guard_reports_only_closure_child_access() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field GetPlayerColor fun(self: Player): number

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@param ent Entity
            ---@param replacement Entity
            local function make(ent, replacement)
                if ent:IsPlayer() then
                    local direct = ent:GetPlayerColor()
                    local callback = function()
                        return ent:GetPlayerColor()
                    end
                    ent = replacement
                    return direct, callback
                end
            end
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].range.start.line, 15);
        assert_eq!(
            diagnostics[0].message,
            "expected `Player` but found `Entity`. Add a guard to narrow the parent to `Player`."
        );
    }

    #[test]
    fn unguarded_child_diagnostic_uses_receiver_type_after_return_propagation() {
        for _ in 0..32 {
            let mut ws = VirtualWorkspace::new();
            enable_gmod(&mut ws);
            let file_id = ws.def(
                r#"
                ---@class Entity
                ---@class Player: Entity
                ---@field SteamID fun(self: Player): string

                ---@param user Entity
                local function read_steam_id(user)
                    local steam_id = user:SteamID()
                    return steam_id
                end
                "#,
            );

            let diagnostics =
                diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(
                diagnostics[0].message,
                "expected `Player` but found `Entity`. Add a guard to narrow the parent to `Player`."
            );
        }
    }

    #[test]
    fn return_cast_closure_capture_tracks_incremental_mutability_and_reopen() {
        const IMMUTABLE_SOURCE: &str = r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field GetPlayerColor fun(self: Player): number

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@param ent Entity
            ---@param replacement Entity
            local function make(ent, replacement)
                if ent:IsPlayer() then
                    local callback = function()
                        return ent:GetPlayerColor()
                    end
                    return callback
                end
            end
        "#;
        const MUTABLE_SOURCE: &str = r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field GetPlayerColor fun(self: Player): number

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@param ent Entity
            ---@param replacement Entity
            local function make(ent, replacement)
                if ent:IsPlayer() then
                    local callback = function()
                        return ent:GetPlayerColor()
                    end
                    ent = replacement
                    return callback
                end
            end
        "#;

        let fresh_diagnostic_count = |source: &str| {
            let mut fresh = VirtualWorkspace::new();
            enable_gmod(&mut fresh);
            let file_id = fresh.def(source);
            diagnostic_count(&mut fresh, file_id, DiagnosticCode::InferUnguardedChild)
        };

        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/shared/issue_49_incremental.lua");
        let file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(IMMUTABLE_SOURCE.to_string()))
            .map(|(id, _)| id)
            .expect("initial immutable file");
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            fresh_diagnostic_count(IMMUTABLE_SOURCE)
        );

        let updated_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(MUTABLE_SOURCE.to_string()))
            .map(|(id, _)| id)
            .expect("mutable update");
        assert_eq!(updated_file_id, file_id);
        assert_eq!(
            diagnostic_count(
                &mut ws,
                updated_file_id,
                DiagnosticCode::InferUnguardedChild
            ),
            fresh_diagnostic_count(MUTABLE_SOURCE)
        );

        let restored_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(IMMUTABLE_SOURCE.to_string()))
            .map(|(id, _)| id)
            .expect("restored immutable update");
        assert_eq!(
            diagnostic_count(
                &mut ws,
                restored_file_id,
                DiagnosticCode::InferUnguardedChild
            ),
            fresh_diagnostic_count(IMMUTABLE_SOURCE)
        );

        ws.analysis
            .remove_file_by_uri(&uri)
            .0
            .expect("removed incremental file");
        let reopened_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(IMMUTABLE_SOURCE.to_string()))
            .map(|(id, _)| id)
            .expect("reopened immutable file");
        assert_eq!(
            diagnostic_count(
                &mut ws,
                reopened_file_id,
                DiagnosticCode::InferUnguardedChild
            ),
            fresh_diagnostic_count(IMMUTABLE_SOURCE)
        );
    }

    #[test]
    fn unguarded_child_reports_once_and_keeps_unknown_diagnostic_separate() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class Child: Base
            ---@field One fun(self: Child)
            ---@field Two fun(self: Child)
            ---@type Base
            local value
            value:One()
            value:Two()
            "#,
        );

        let child_diagnostics =
            diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(child_diagnostics.len(), 1, "{child_diagnostics:?}");
        assert_eq!(child_diagnostics[0].range.start.line, 7);
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnknown),
            0
        );
    }

    #[test]
    fn declared_field_narrows_to_the_only_child_defining_the_member() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field ConCommand fun(self: Player, cmd: string)
            ---@class Holder
            ---@field Owner Entity
            ---@type Holder
            local h
            h.Owner:ConCommand("kill")
            h.Owner:ConCommand("say")
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            0
        );
        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "expected `Player` but found `Entity`. Add a guard to narrow the parent to `Player`."
        );
        assert_eq!(
            member_path_receiver_types(&ws, file_id, "h.Owner"),
            vec!["Player", "Player"]
        );
    }

    #[test]
    fn declared_field_narrows_from_a_single_use() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field ConCommand fun(self: Player, cmd: string)
            ---@class Holder
            ---@field Owner Entity
            ---@type Holder
            local h
            h.Owner:ConCommand("kill")
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            0
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
        assert_eq!(
            member_path_receiver_types(&ws, file_id, "h.Owner"),
            vec!["Player"]
        );
    }

    #[test]
    fn declared_field_narrowing_follows_a_deeper_member_path() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field ConCommand fun(self: Player, cmd: string)
            ---@class Inner
            ---@field Owner Entity
            ---@class Outer
            ---@field data Inner
            ---@type Outer
            local o
            o.data.Owner:ConCommand("kill")
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            0
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
        assert_eq!(
            member_path_receiver_types(&ws, file_id, "o.data.Owner"),
            vec!["Player"]
        );
    }

    #[test]
    fn declared_field_member_owned_by_the_base_is_not_narrowed() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@field GetClass fun(self: Entity): string
            ---@class Player: Entity
            ---@field GetClass fun(self: Player): string
            ---@class Holder
            ---@field Owner Entity
            ---@type Holder
            local h
            local class = h.Owner:GetClass()
            print(class)
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
        assert_eq!(
            member_path_receiver_types(&ws, file_id, "h.Owner"),
            vec!["Entity"]
        );
    }

    #[test]
    fn guarded_declared_field_is_not_unguarded_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field ConCommand fun(self: Player, cmd: string)

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@class Holder
            ---@field Owner Entity
            ---@type Holder
            local h
            if h.Owner:IsPlayer() then
                h.Owner:ConCommand("kill")
            end
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            0
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn declared_field_tie_names_the_member_instead_of_an_unwritable_union() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Bravo: Entity
            ---@field Shared fun(self: Bravo)
            ---@class Alpha: Entity
            ---@field Shared fun(self: Alpha)
            ---@class Holder
            ---@field Owner Entity
            ---@type Holder
            local h
            h.Owner:Shared()
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "`Shared` is not defined on `Entity`. Add a guard that narrows the parent to one of \
             `Alpha`, `Bravo`."
        );
        assert_eq!(
            member_path_receiver_types(&ws, file_id, "h.Owner"),
            vec!["(Alpha|Bravo)"]
        );
    }

    #[test]
    fn unguarded_child_tie_caps_the_listed_candidate_types() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@class DAlpha: Panel
            ---@field Shared fun(self: DAlpha)
            ---@class DBravo: Panel
            ---@field Shared fun(self: DBravo)
            ---@class DCharlie: Panel
            ---@field Shared fun(self: DCharlie)
            ---@class DDelta: Panel
            ---@field Shared fun(self: DDelta)
            ---@class DEcho: Panel
            ---@field Shared fun(self: DEcho)
            ---@type Panel
            local value
            value:Shared()
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "`Shared` is not defined on `Panel`. Add a guard that narrows the parent to one of \
             `DAlpha`, `DBravo`, `DCharlie` and 2 more."
        );
    }

    /// `Alpha` and `Bravo` sit inside a `return`, so the early evidence pass
    /// narrows their receiver to `Middle` and publishes it. The late pass then
    /// sees that narrowed receiver again at the `Shared` use, and `Shared` is
    /// defined on `Leaf`, a child of `Middle`. Scoring against the published
    /// type would descend one more level of the class tree per pass, so every
    /// use has to keep scoring against `Entity`.
    #[test]
    fn unguarded_child_path_does_not_rescore_against_its_own_narrowing() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Middle: Entity
            ---@field Alpha fun(self: Middle)
            ---@field Bravo fun(self: Middle)
            ---@class Other: Entity
            ---@field Shared fun(self: Other)
            ---@class Leaf: Middle
            ---@field Shared fun(self: Leaf)

            ---@param value any
            ---@return TypeGuard<Entity>
            function isentity(value) end

            ---@class Holder
            ---@field proc unknown

            ---@type Holder
            local h

            local function run(a, b)
                if not isentity(h.proc) then return end
                if a then return h.proc:Alpha() end
                if b then return h.proc:Bravo() end
                return h.proc:Shared()
            end
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "expected `Leaf` but found `Entity`. Add a guard to narrow the parent to `Leaf`."
        );
        assert_eq!(
            member_path_receiver_types(&ws, file_id, "h.proc"),
            vec!["Leaf", "Leaf", "Leaf"]
        );
    }

    #[test]
    fn table_literal_through_declared_field_does_not_report_unguarded_child() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity

            ---@class ContextChildA: Entity
            local A = {}
            function A:special() return true end

            ---@class ContextChildB: Entity
            local B = {}
            function B:special() return true end

            ---@class ContextHolder
            ---@field condition (fun(id: string, ent: ContextChildA|ContextChildB): boolean?)?

            ---@class ContextMeta
            ---@field templates table<string, ContextHolder>?

            ---@type ContextMeta
            local meta = {}
            meta.templates = {
                example = {
                    condition = function(id, ent)
                        return ent:special()
                    end,
                },
            }
            "#,
        );

        let child_diagnostics =
            diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(
            child_diagnostics.len(),
            0,
            "declared field contract should type `ent` as ContextChildA|ContextChildB: \
             {child_diagnostics:?}"
        );
    }

    #[test]
    fn local_fallback_reads_a_class_binding_another_files_local_fallback_writes() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_files(vec![
            (
                "lua/autorun/radio_tuner.lua",
                r#"
                ---@class Entity
                ---@class Radio: Entity
                ---@field Tune fun(self: Radio)

                ---@type Entity
                local tuned
                tuned:Tune()
                "#,
            ),
            (
                "lua/autorun/radio_muter.lua",
                r#"
                ---@class Radio

                ---@type Entity
                local muted
                muted:Mute()
                "#,
            ),
        ]);
        let tuner_file_id = ws
            .analysis
            .get_file_id(
                &ws.virtual_url_generator
                    .new_uri("lua/autorun/radio_tuner.lua"),
            )
            .expect("tuner file id");

        assert_eq!(
            ws.humanize_type(last_name_type(&ws, tuner_file_id, "tuned")),
            "Radio"
        );
    }

    const TYPES: &str = "lua/autorun/sh_types.lua";
    const LIB: &str = "lua/autorun/sh_lib.lua";
    const USER: &str = "lua/autorun/sh_user.lua";
    const TYPES_TEXT: &str = "---@class Player
---@class Vehicle
---@class Dummy1
---@param ply Player
function Kick(ply) end
---@param car Vehicle
function Tow(car) end
";
    const LIB_TEXT: &str = "Lib = {}
function Lib.Get(a) return Mystery end
";
    const USER_TEXT: &str = "local target = Lib.Get(1)
Kick(target)
local towed = Lib.Get(2)
Tow(towed)
";

    fn retyped_types_text() -> String {
        TYPES_TEXT.replace("---@param ply Player", "---@param ply Dummy1")
    }

    fn file_diagnostics(ws: &VirtualWorkspace, file_id: crate::FileId) -> Vec<String> {
        let mut diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .map(|diagnostic| {
                format!(
                    "{:?} {:?} {}",
                    diagnostic.range, diagnostic.code, diagnostic.message
                )
            })
            .collect::<Vec<_>>();
        diagnostics.sort();
        diagnostics
    }

    /// An edit that retypes one local's fallback leaves the types and
    /// diagnostics a cold build of the edited text has.
    #[test]
    fn an_edit_leaves_the_local_fallbacks_and_diagnostics_a_cold_build_has() {
        let edited = retyped_types_text();
        let mut warm = VirtualWorkspace::new();
        let file_ids = warm.def_files(vec![
            (TYPES, TYPES_TEXT),
            (LIB, LIB_TEXT),
            (USER, USER_TEXT),
        ]);
        warm.def_file(TYPES, &edited);
        let mut cold = VirtualWorkspace::new();
        cold.def_files(vec![
            (TYPES, edited.as_str()),
            (LIB, LIB_TEXT),
            (USER, USER_TEXT),
        ]);

        let settled = warm.settled_digest();
        assert!(settled.iter().any(|line| line.ends_with("target = Dummy1")));
        assert!(settled.iter().any(|line| line.ends_with("towed = Vehicle")));
        assert_eq!(settled, cold.settled_digest());
        assert_eq!(
            file_diagnostics(&warm, file_ids[2]),
            file_diagnostics(&cold, file_ids[2])
        );
        assert!(warm.stale_derivations().is_empty());
        assert!(warm.phantom_derivations().is_empty());
    }

    /// Editing what types one local re-runs the fallbacks of that local's
    /// file alone.
    #[test]
    fn editing_what_types_one_local_re_runs_only_its_files_fallbacks() {
        let fallbacks_after_the_edit = |files: Vec<(&str, &str)>| {
            let mut ws = VirtualWorkspace::new();
            ws.def_files(files);
            ws.def_file(TYPES, &retyped_types_text());
            ws.analysis
                .compilation
                .last_stats()
                .evaluations(crate::db_index::derivation::DerivationKind::LocalFallbacks)
        };

        let with_user = fallbacks_after_the_edit(vec![
            (TYPES, TYPES_TEXT),
            (LIB, LIB_TEXT),
            (USER, USER_TEXT),
        ]);
        let without_user = fallbacks_after_the_edit(vec![(TYPES, TYPES_TEXT), (LIB, LIB_TEXT)]);

        assert_eq!(with_user, without_user + 1);
    }

    const BLAST: &str = "lua/autorun/sh_blast.lua";
    const FIRE: &str = "lua/autorun/sh_fire.lua";
    const BLAST_TEXT: &str = "---@class Entity
---@class Player: Entity
---@param inflictor Entity
---@param attacker Player
function Blast(inflictor, attacker) end
";
    const FIRE_TEXT: &str = "local attacker, inflictor
function Fire(params)
    attacker = params.attacker
    inflictor = params.inflictor or attacker
    Blast(inflictor, attacker)
end
";

    /// Locals whose guesses feed each other, one assigned from the other, are
    /// guessed together: a cold build settles them, and an edit of what types
    /// them leaves what a cold build of the edited text has.
    #[test]
    fn locals_whose_guesses_feed_each_other_settle_as_a_cold_build_has_them() {
        let edited = BLAST_TEXT.replace("---@param attacker Player", "---@param attacker Entity");
        let mut warm = VirtualWorkspace::new();
        warm.def_files(vec![(BLAST, BLAST_TEXT), (FIRE, FIRE_TEXT)]);
        assert!(warm.stale_derivations().is_empty());
        warm.def_file(BLAST, &edited);
        let mut cold = VirtualWorkspace::new();
        cold.def_files(vec![(BLAST, edited.as_str()), (FIRE, FIRE_TEXT)]);

        assert_eq!(warm.settled_digest(), cold.settled_digest());
        assert!(warm.stale_derivations().is_empty());
        assert!(warm.phantom_derivations().is_empty());
    }

    const PANEL_TREE: &str = r#"
        ---@class Panel
        ---@class TextEntry: Panel
        ---@class DTextEntry: TextEntry
        ---@field SetUpdateOnType fun(self: DTextEntry, update: boolean)
        ---@class DPanel: Panel
        ---@class DNumPad: DPanel
        ---@field SetUpdateOnType fun(self: DNumPad, update: boolean)
        ---@class DNumPadSub: DNumPad
        "#;

    fn panel_tree_diagnostics(consumer: &str) -> (usize, usize, Vec<String>) {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def(PANEL_TREE);
        let file_id = ws.def(consumer);
        let unguarded = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        (
            unguarded.len(),
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            unguarded.into_iter().map(|d| d.message).collect(),
        )
    }

    #[test]
    fn unguarded_child_finds_a_grandchild_that_declares_the_member() {
        let (unguarded, undefined, messages) = panel_tree_diagnostics(
            r#"
            ---@type Panel
            local filter
            filter:SetUpdateOnType(true)
            "#,
        );

        assert_eq!(undefined, 0);
        assert_eq!(unguarded, 1);
        assert!(messages[0].contains("`DTextEntry`"), "{messages:?}");
    }

    #[test]
    fn unguarded_child_lists_the_nearest_declaring_class_only() {
        let (_, _, messages) = panel_tree_diagnostics(
            r#"
            ---@type Panel
            local filter
            filter:SetUpdateOnType(true)
            "#,
        );

        assert!(
            messages[0].contains("one of `DNumPad`, `DTextEntry`."),
            "{messages:?}"
        );
    }

    /// A subclass of the parent that does not declare the used member leaves the
    /// unguarded use alone; one that declares it re-evaluates the use.
    #[test]
    fn adding_a_subclass_re_runs_unguarded_uses_only_when_it_declares_the_member() {
        const CONSUMER: &str = "lua/autorun/client/consumer.lua";
        const CONSUMER_TEXT: &str = "---@type Panel
local filter
filter:SetUpdateOnType(true)
";
        let fallbacks_after_adding = |file: &str, text: &str| {
            let mut ws = VirtualWorkspace::new();
            enable_gmod(&mut ws);
            ws.def_files(vec![
                ("lua/autorun/sh_types.lua", PANEL_TREE),
                (CONSUMER, CONSUMER_TEXT),
            ]);
            ws.def_file(file, text);
            ws.analysis
                .compilation
                .last_stats()
                .evaluations(crate::db_index::derivation::DerivationKind::LocalFallbacks)
        };

        let unrelated_class =
            fallbacks_after_adding("lua/autorun/sh_other.lua", "---@class Unrelated");
        let unrelated_subclass =
            fallbacks_after_adding("lua/autorun/sh_other.lua", "---@class DLabel: Panel");
        let declaring_subclass = fallbacks_after_adding(
            "lua/autorun/sh_other.lua",
            "---@class DSlider: Panel
---@field SetUpdateOnType fun(self: DSlider, update: boolean)",
        );

        assert_eq!(unrelated_subclass, unrelated_class);
        assert!(declaring_subclass > unrelated_class);
    }

    #[test]
    fn unguarded_child_picks_the_class_that_has_every_used_member() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Weapon
            ---@class SWEP: Weapon
            ---@field Clip fun(self: SWEP): number
            ---@class SubSWEP: SWEP
            ---@field Reload fun(self: SubSWEP)

            ---@param wep Weapon
            local function use(wep)
                wep:Reload()
                return wep:Clip()
            end
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            0
        );
        let unguarded = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert!(
            unguarded
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("`SWEP`")),
            "{unguarded:?}"
        );
    }

    #[test]
    fn child_method_is_not_a_candidate_for_a_field_indexed_into() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class D: Base
            ---@field Thing fun(self: D)

            ---@param base Base
            local function use(base)
                base.Thing:Other()
            end
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn child_method_is_a_candidate_when_only_the_receiver_is_a_member_path() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Weapon: Entity
            ---@field IsCarried fun(self: Weapon): boolean
            ---@class Holder
            ---@field WeaponEnt Entity

            ---@param holder Holder
            local function use(holder)
                holder.WeaponEnt:IsCarried()
            end
            "#,
        );

        let unguarded = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(unguarded.len(), 1, "{unguarded:?}");
        assert!(unguarded[0].message.contains("`Weapon`"), "{unguarded:?}");
    }

    #[test]
    fn child_method_is_a_candidate_for_a_dotted_call_on_a_member_path() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field GetActive fun(self: Player): Entity
            ---@class NPC: Entity
            ---@field GetActive fun(self: NPC): Entity
            ---@class Effect
            ---@field Player Entity

            ---@param x Effect
            local function use(x)
                x.Player:GetActive()
            end
            "#,
        );

        let unguarded = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(unguarded.len(), 1, "{unguarded:?}");
        assert!(
            unguarded[0].message.contains("`NPC`") && unguarded[0].message.contains("`Player`"),
            "{unguarded:?}"
        );
    }

    #[test]
    fn child_method_is_a_candidate_on_an_assigned_member_path() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Weapon: Entity
            ---@field IsCarried fun(self: Weapon): boolean
            ---@class CEffectData
            ---@field GetEntity fun(self: CEffectData): Entity
            ---@class Effect

            ---@param effect Effect
            ---@param data CEffectData
            local function init(effect, data)
                effect.WeaponEnt = data:GetEntity()
                effect.WeaponEnt:IsCarried()
            end
            "#,
        );

        let unguarded = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(unguarded.len(), 1, "{unguarded:?}");
        assert!(unguarded[0].message.contains("`Weapon`"), "{unguarded:?}");
    }

    #[test]
    fn child_method_is_still_a_candidate_for_a_method_call() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Base
            ---@class D: Base
            ---@field Thing fun(self: D)

            ---@param base Base
            local function use(base)
                base:Thing()
            end
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }

    #[test]
    fn assigned_member_path_is_not_unguarded_child_evidence() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@class AvatarImage: Panel
            ---@field SetPaintedManually fun(self: AvatarImage, painted: boolean)
            ---@class DFrame: Panel
            ---@field Avatar AvatarImage

            vgui = vgui or {}

            ---@generic T : Panel
            ---@param classname `T`
            ---@param parent? Panel
            ---@return T
            function vgui.Create(classname, parent) end

            ---@param pnl Panel
            local function build(pnl)
                pnl.Avatar = vgui.Create("AvatarImage", pnl)
                pnl.Avatar:SetPaintedManually(true)
            end
            "#,
        );

        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    fn guard_form_diagnostic_count(use_site: &str) -> usize {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_gmod_type_predicates();
        let source = format!(
            r#"
            ---@class Entity
            ---@class Car: Entity
            ---@field Honk fun(self: Car)
            ---@param ent Entity
            local function use(ent)
                {use_site}
            end
            "#
        );
        let file_id = ws.def(&source);
        diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild)
    }

    /// The unguarded-child and usage-inference hints on a callback's read of
    /// a child-only member off its `panel` param.
    fn callback_panel_hints(param_doc: &str) -> (usize, usize) {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let source = format!(
            r#"
            ---@class Panel
            ---@class FacePanel: Panel
            ---@field charData table
            local data = {{
                {param_doc}
                Think = function(animation, panel)
                    return panel.charData
                end,
            }}
            return data
            "#
        );
        let file_id = ws.def(&source);
        (
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnknown),
        )
    }

    #[test]
    fn a_child_read_off_a_param_only_its_name_types_is_inferred_from_usage() {
        assert_eq!(callback_panel_hints(""), (0, 1));
    }

    #[test]
    fn a_child_read_off_a_param_declared_as_the_parent_needs_a_guard() {
        assert_eq!(callback_panel_hints("---@param panel Panel"), (1, 0));
    }

    #[test]
    fn a_member_an_anonymous_panel_class_declares_suggests_no_other_class() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "lua/menu/annotations.lua",
            r#"
            ---@class Panel
            ---@class DPanel: Panel
            ---@class PublishWindow: Panel
            ---@field UpdateProgress Panel

            ---@generic T: table
            ---@[call_arg("gmod.load", "include")]
            ---@[call_arg("gmod.vgui_panel", "register_file")]
            ---@param file string
            ---@return T
            function vgui.RegisterFile(file) end
            "#,
        );
        ws.def_file(
            "lua/menu/vgui/workshop.lua",
            r#"
            PANEL.Base = "DPanel"

            function PANEL:UpdateProgress(done) end
            "#,
        );
        let file_id = ws.def_file(
            "lua/menu/mount.lua",
            r#"
            local pnlWorkshop = vgui.RegisterFile("vgui/workshop.lua")

            ---@param pnl Panel
            local function show(pnl)
                pnl:UpdateProgress(1)
            end
            return pnlWorkshop, show
            "#,
        );

        assert_eq!(
            diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            Vec::new()
        );
    }

    /// A child-only member read straight off a call returning the parent,
    /// `self:GetOwner():GetAimVector()`, needs a guard as the same read off a
    /// local does; a member no child defines stays undefined.
    #[test]
    fn a_child_read_off_a_call_needs_a_guard_as_off_a_local() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field GetAimVector fun(self: Player): number
            ---@class NPC: Entity
            ---@field GetAimVector fun(self: NPC): number

            ---@return Entity
            local function owner() end

            local through_local = owner()
            local a = through_local:GetAimVector()
            local b = owner():GetAimVector()
            local c = owner():NoSuchMethod()
            "#,
        );

        let messages = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild)
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>();
        let undefined = diagnostics_for(&mut ws, file_id, DiagnosticCode::UndefinedMethod)
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>();

        let advice = "`GetAimVector` is not defined on `Entity`. Add a guard that narrows the parent to one of `NPC`, `Player`.";
        assert_eq!(messages, vec![advice.to_string(), advice.to_string()]);
        assert_eq!(
            undefined,
            vec!["Undefined method `NoSuchMethod`. ".to_string()]
        );
    }

    #[test]
    fn unguarded_use_fires_without_any_guard() {
        assert_eq!(guard_form_diagnostic_count("ent:Honk()"), 1);
    }

    #[test]
    fn member_presence_if_guard_does_not_fire() {
        assert_eq!(
            guard_form_diagnostic_count("if ent.Honk then ent:Honk() end"),
            0
        );
    }

    #[test]
    fn member_presence_and_guard_does_not_fire() {
        assert_eq!(
            guard_form_diagnostic_count("local _ = ent.Honk and ent:Honk()"),
            0
        );
    }

    #[test]
    fn member_presence_early_return_guard_does_not_fire() {
        assert_eq!(
            guard_form_diagnostic_count(
                "if not ent.Honk then return end
                ent:Honk()"
            ),
            0
        );
    }

    #[test]
    fn isfunction_member_guard_does_not_fire() {
        assert_eq!(
            guard_form_diagnostic_count("if isfunction(ent.Honk) then ent:Honk() end"),
            0
        );
    }

    #[test]
    fn class_name_comparison_guard_does_not_fire() {
        assert_eq!(
            guard_form_diagnostic_count(r#"if ent:GetClass() == "Car" then ent:Honk() end"#),
            0
        );
    }

    #[test]
    fn class_name_early_return_guard_does_not_fire() {
        assert_eq!(
            guard_form_diagnostic_count(
                r#"if ent:GetClass() ~= "Car" then return end
                ent:Honk()"#
            ),
            0
        );
    }

    #[test]
    fn a_field_written_through_a_maybe_null_entity_is_not_declared_on_null() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class NULL: Entity
            ---@class Car: Entity

            ---@return Car|NULL
            local function make() end
            local car = make()
            car.Driver = 1

            ---@param ent Entity
            local function use(ent)
                return ent.Driver
            end
            return use
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "expected `Car` but found `Entity`. Add a guard to narrow the parent to `Car`."
        );
    }

    fn entity_table_guard_hint_count(use_site: &str) -> usize {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let source = format!(
            r#"
            ---@class Entity
            Entity = {{}}

            ---@return tableof<self>
            function Entity:GetTable() end

            ---@class Car: Entity
            ---@field Honk fun(self: Car)

            local GetTable = Entity.GetTable

            ---@param ent Entity
            ---@param other Entity
            local function use(ent, other)
                {use_site}
            end
            return use
            "#
        );
        let file_id = ws.def(&source);
        diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild)
    }

    #[test]
    fn a_member_check_on_the_entity_table_guards_the_entity() {
        assert_eq!(
            entity_table_guard_hint_count(
                "local tbl = ent:GetTable()
                if tbl and tbl.Honk then ent:Honk() end"
            ),
            0
        );
        assert_eq!(
            entity_table_guard_hint_count(
                "local tbl = GetTable(ent)
                if not tbl or not tbl.Honk then return end
                ent:Honk()"
            ),
            0
        );
        assert_eq!(
            entity_table_guard_hint_count(
                "ent = other
                local tbl = GetTable(ent)
                if tbl.Honk then ent:Honk() end"
            ),
            0
        );
    }

    /// A field the variable is given through a cast is evidence of what the
    /// variable may be: an unguarded read of it before the cast is reported.
    #[test]
    fn a_field_written_under_a_cast_names_the_child_for_an_earlier_read() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class ReadCar: Entity

            ---@param ent Entity
            local function stamp(ent)
                local before = ent.Boost
                ---@cast ent ReadCar
                ent.Boost = 1
                return before
            end
            return stamp
            "#,
        );
        let model = ws.analysis.compilation.get_semantic_model(file_id).unwrap();
        let before = model
            .get_root()
            .descendants::<LuaIndexExpr>()
            .find(|index| index.syntax().text() == "ent.Boost")
            .unwrap();
        let class = crate::LuaTypeDeclId::global("ReadCar");
        let key = crate::LuaMemberKey::Name("Boost".into());
        let mut cache = model.get_cache().borrow_mut();
        assert!(
            crate::semantic::resolve_dynamic_field_member(
                model.get_db(),
                &mut cache,
                &crate::LuaType::Ref(class),
                &key,
                Some(before.get_position()),
            )
            .unwrap()
            .is_none()
        );
        drop(cache);
        drop(model);
        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].message.contains("ReadCar"));
    }

    #[test]
    fn a_member_check_on_a_replaced_entity_table_does_not_guard_the_entity() {
        assert_eq!(
            entity_table_guard_hint_count(
                "local tbl = GetTable(ent)
                ent = other
                if tbl.Honk then ent:Honk() end"
            ),
            1
        );
        assert_eq!(
            entity_table_guard_hint_count(
                "local tbl = ent:GetTable()
                for _ = 1, 2 do
                    if tbl.Honk then ent:Honk() end
                    ent = other
                end"
            ),
            1
        );
    }

    #[test]
    fn is_valid_guard_alone_does_not_narrow_to_child() {
        assert_eq!(
            guard_form_diagnostic_count("if IsValid(ent) then ent:Honk() end"),
            1
        );
    }

    #[test]
    fn unguarded_child_message_names_the_flow_narrowed_receiver() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_gmod_type_predicates();
        let file_id = ws.def(
            r#"
            ---@class NULL
            ---@class Entity
            ---@class Car: Entity
            ---@field GetFuelCapacity fun(self: Car): number
            ---@param veh Entity|NULL
            local function use(veh)
                if IsValid(veh) then
                    return veh:GetFuelCapacity()
                end
            end
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "expected `Car` but found `Entity`. Add a guard to narrow the parent to `Car`."
        );
    }

    #[test]
    fn unguarded_child_message_names_the_receiver_an_early_return_narrowed() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_gmod_type_predicates();
        let file_id = ws.def(
            r#"
            ---@class NULL
            ---@class Entity
            ---@class Car: Entity
            ---@field GetFuelCapacity fun(self: Car): number
            ---@return Entity|NULL
            local function find() end
            local function use()
                local veh = find()
                if not IsValid(veh) then return end
                return veh:GetFuelCapacity()
            end
            return use
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "expected `Car` but found `Entity`. Add a guard to narrow the parent to `Car`."
        );
    }

    #[test]
    fn the_runtime_isvalid_in_util_lua_narrows_like_the_annotated_one() {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_file(
            "lua/includes/util.lua",
            r#"
            function IsValid(object)
                if ( !object ) then return false end
                local isvalid = object.IsValid
                if ( !isvalid ) then return false end
                return isvalid( object )
            end
            "#,
        );
        let file_id = ws.def_file(
            "lua/autorun/fuel.lua",
            r#"
            ---@class NULL
            ---@class Entity
            ---@class Car: Entity
            ---@field GetFuelCapacity fun(self: Car): number
            ---@return Entity|NULL
            local function find() end
            local function use()
                local veh = find()
                if not IsValid(veh) then return end
                return veh:GetFuelCapacity()
            end
            return use
            "#,
        );

        let diagnostics = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            "expected `Car` but found `Entity`. Add a guard to narrow the parent to `Car`."
        );
    }

    fn closure_receiver_hint_count(write_avatar: bool) -> usize {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let avatar_write = if write_avatar {
            r#"pnl.Avatar = vgui.Create("AvatarImage", pnl)"#
        } else {
            ""
        };
        let file_id = ws.def(&format!(
            r#"
            ---@class Panel
            ---@field Paint fun(self: Panel, w: number, h: number)
            ---@field PerformLayout fun(self: Panel)
            ---@field SetPlayer fun(self: Panel, ply: any, size: number)
            ---@class AvatarImage: Panel
            ---@field SetPlayer fun(self: AvatarImage, ply: any, size: number)
            ---@class DFrame: Panel
            ---@field Avatar AvatarImage

            vgui = vgui or {{}}

            ---@generic T : Panel
            ---@param classname `T`
            ---@param parent? Panel
            ---@return T
            function vgui.Create(classname, parent) end

            ---@param pnl Panel
            local function build(pnl)
                {avatar_write}
                pnl.Paint = function(s, w, h)
                    s.Avatar:SetPlayer(nil, 1)
                end
                pnl.PerformLayout = function(s)
                    s.Avatar:SetPlayer(nil, 2)
                end
                pnl.SetPlayer = function(s, ply, size) s.Avatar:SetPlayer(ply, size) end
            end
            "#
        ));
        diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild)
    }

    #[test]
    fn closure_assigned_to_member_sees_receiver_member_writes() {
        assert_eq!(closure_receiver_hint_count(true), 0);
    }

    #[test]
    fn closure_assigned_to_member_without_receiver_write_still_hints() {
        assert!(closure_receiver_hint_count(false) > 0);
    }

    fn presence_probe_source(body: &str) -> String {
        format!(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field Armor number
            ---@field Shield number
            ---@class NPC: Entity
            ---@field Shield number

            ---@param value any
            ---@return TypeGuard<Entity>
            function IsValid(value) end

            ---@param ent Entity
            local function probe(ent)
            {body}
            end
            "#
        )
    }

    fn presence_probe_child_lines(body: &str) -> Vec<u32> {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(&presence_probe_source(body));
        diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild)
            .into_iter()
            .map(|diagnostic| diagnostic.range.start.line)
            .collect()
    }

    fn presence_probe_child_count(body: &str) -> usize {
        presence_probe_child_lines(body).len()
    }

    fn line_of(body: &str, marker: &str) -> u32 {
        presence_probe_source(body)
            .lines()
            .position(|line| line.contains(marker))
            .expect("marker") as u32
    }

    #[test]
    fn presence_probes_and_guards_are_not_unguarded_child_reads() {
        let bodies = [
            "if not ent.Armor then return end\n local x = ent.Armor + 1",
            "local armor = ent.Armor\n if not armor then return end",
            "local rear = ent.Armor == 5",
            "local bad = not ent.Armor or ent.Armor > 5",
            "local ok = IsValid(ent.Armor)",
        ];
        for body in bodies {
            assert_eq!(presence_probe_child_count(body), 0, "{body}");
        }
    }

    #[test]
    fn a_parenthesized_initializer_is_bound_to_its_tested_local() {
        for body in [
            "local armor = (ent.Armor)\n if not armor then return end",
            "local armor = ((ent.Armor))\n if not armor then return end",
        ] {
            assert_eq!(presence_probe_child_count(body), 0, "{body}");
        }
        for body in [
            "local armor = (ent.Armor + 1)\n if not armor then return end",
            "local armor = (ent.Armor)\n armor = nil\n if not armor then return end",
        ] {
            assert!(presence_probe_child_count(body) > 0, "{body}");
        }
    }

    #[test]
    fn unprobed_child_reads_stay_unguarded_child_reads() {
        let bodies = [
            "local x = ent.Armor + 1",
            "if not ent.Shield then return end\n local x = ent.Armor + 1",
            "local armor = ent.Armor\n armor = nil\n if not armor then return end",
        ];
        for body in bodies {
            assert!(presence_probe_child_count(body) > 0, "{body}");
        }
    }

    #[test]
    fn a_probe_does_not_hide_a_later_or_earlier_unguarded_read() {
        let bodies = [
            "local rear = ent.Armor == 5\n local s = ent.Shield + 1 -- unguarded",
            "local s = ent.Shield + 1 -- unguarded\n local rear = ent.Armor == 5",
            "local armor = ent.Armor\n if armor then print(armor) end\n local s = ent.Shield + 1 -- unguarded",
            "local ok = IsValid(ent.Armor)\n local s = ent.Shield + 1 -- unguarded",
            "if IsValid(ent.Armor) then\n return 1\n else\n local s = ent.Shield + 1 -- unguarded\n end",
        ];
        for body in bodies {
            assert_eq!(
                presence_probe_child_lines(body),
                vec![line_of(body, "-- unguarded")],
                "{body}"
            );
        }
    }

    #[test]
    fn a_read_inside_a_presence_guard_stays_covered() {
        let body = "if IsValid(ent.Armor) then\n local s = ent.Shield + 1\n end";
        assert_eq!(presence_probe_child_count(body), 0, "{body}");
    }

    #[test]
    fn a_colon_predicate_does_not_probe_its_other_arguments() {
        let source = r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field Child number

            ---@class Probe
            ---@return TypeGuard<Probe>
            function Probe:Holds(other) end

            ---@param probe Probe
            ---@param other Entity
            local function check(probe, other)
                local direct = probe:Holds(other.Child)
                return direct
            end

            ---@param probe Probe
            ---@param other Entity
            local function check_bound(probe, other)
                local value = other.Child
                return probe:Holds(value)
            end
            "#;
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        let file_id = ws.def(source);
        let lines = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild)
            .into_iter()
            .map(|diagnostic| diagnostic.range.start.line)
            .collect::<Vec<_>>();
        let line_of = |needle: &str| {
            source
                .lines()
                .position(|line| line.contains(needle))
                .unwrap() as u32
        };
        assert_eq!(
            lines,
            vec![
                line_of("probe:Holds(other.Child)"),
                line_of("local value = other.Child")
            ]
        );
    }

    #[test]
    fn a_real_is_valid_guard_call_probes_its_argument() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        enable_gmod(&mut ws);
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "lua/annotations/getter_api.lua",
            include_str!("fixtures/getter_guard/api.lua"),
        );
        let source = r#"
            ---@class ProbeChild: Entity
            ---@field Armor number
            ---@field Shield number

            ---@param ent Entity
            function ProbeRealIsValid(ent)
                local ok = IsValid(ent.Armor)
                local s = ent.Shield + 1 -- unguarded
            end
            "#;
        let file_id = ws.def_file("lua/autorun/real_is_valid.lua", source);
        let lines = diagnostics_for(&mut ws, file_id, DiagnosticCode::InferUnguardedChild)
            .into_iter()
            .map(|diagnostic| diagnostic.range.start.line)
            .collect::<Vec<_>>();
        let unguarded = source
            .lines()
            .position(|line| line.contains("-- unguarded"))
            .unwrap() as u32;
        assert_eq!(lines, vec![unguarded]);
    }
}

#[cfg(test)]
mod panel_receiver_provenance_test {
    use crate::{
        DiagnosticCode, Emmyrc, FileId, VirtualWorkspace, config::DiagnosticSeveritySetting,
    };
    use lsp_types::{DiagnosticSeverity, NumberOrString};
    use tokio_util::sync::CancellationToken;

    const API: &str = r#"
        ---@class Panel
        local Panel = {}
        ---@return Panel
        function Panel:GetParent() end
        ---@return Panel
        function Panel:FindContainer() end
        ---@class SpawnIcon: Panel
        ---@field InternalAddResizeMenu fun(self: SpawnIcon)
        local SpawnIcon = {}
        ---@class DTileLayout: Panel
        ---@field Layout fun(self: DTileLayout)
        local DTileLayout = {}
        ---@class SpawnmenuContentPanel: Panel
        ---@field ContentNavBar table
        local SpawnmenuContentPanel = {}
        ---@class CreationMenuTab
        ---@field ContentPanel? Panel
        ---@field Panel Panel
        local CreationMenuTab = {}
        ---@class CreationMenu: Panel
        local CreationMenu = {}
        ---@param name string
        ---@return CreationMenuTab?
        function CreationMenu:GetCreationTab(name) end
        ---@class SpawnMenu: Panel
        ---@field CreateMenu CreationMenu
        local SpawnMenu = {}
        ---@type SpawnMenu
        g_SpawnMenu = {}
        ---@param value any
        ---@return boolean
        function IsValid(value) end
    "#;

    fn workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_file("lua/annotations/panel_receiver.lua", API);
        ws
    }

    fn severities(ws: &mut VirtualWorkspace, id: FileId) -> Vec<Option<DiagnosticSeverity>> {
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::InferUnguardedChild);
        ws.analysis
            .diagnose_file(id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::InferUnguardedChild.get_name().to_string(),
                    ))
            })
            .map(|diagnostic| diagnostic.severity)
            .collect()
    }

    #[test]
    fn broad_panel_parent_contracts_report_child_inference_as_hints() {
        for body in [
            "pnl:GetParent():Layout()",
            "local parent = pnl:GetParent() parent:Layout()",
            "pnl:FindContainer():Layout()",
        ] {
            let mut ws = workspace();
            let id = ws.def_file(
                "lua/autorun/client/selection_parent.lua",
                &format!(
                    r#"
                ---@param pnl Panel
                local function resize(pnl)
                    if not pnl.InternalAddResizeMenu then return end
                    {body}
                end
            "#
                ),
            );
            assert_eq!(
                severities(&mut ws, id),
                vec![Some(DiagnosticSeverity::HINT)],
                "{body}"
            );
        }
    }

    #[test]
    fn creation_tab_broad_panel_field_reports_child_inference_as_a_hint() {
        for key in ["\"#spawnmenu.category.weapons\"", "\"other\"", "name"] {
            let mut ws = workspace();
            let id = ws.def_file("gamemodes/sandbox/gamemode/spawnmenu/contenttypes/weapons.lua", &format!(r#"
            ---@param name string
            local function refresh(name)
                local swepTab = g_SpawnMenu.CreateMenu:GetCreationTab({key})
                if not swepTab or not swepTab.ContentPanel or not IsValid(swepTab.Panel) then return end
                local tree = swepTab.ContentPanel.ContentNavBar.Tree
            end
            "#));
            assert_eq!(
                severities(&mut ws, id),
                vec![Some(DiagnosticSeverity::HINT)],
                "{key}"
            );
        }
    }

    #[test]
    fn explicit_panel_locals_parameters_factories_and_casts_keep_warnings() {
        for body in [
            "---@type Panel\nlocal panel\npanel:Layout()",
            "---@param panel Panel\nlocal function use(panel) panel:Layout() end",
            "local panel = vgui.Create(\"Panel\") panel:Layout()",
            "local panel = pnl:GetParent()\n---@cast panel Panel\npanel:Layout()",
            "local panel = pnl:GetParent() panel = pnl panel:Layout()",
        ] {
            let mut ws = workspace();
            let id = ws.def_file(
                "lua/autorun/client/explicit_panel.lua",
                &format!(
                    r#"
                ---@param pnl Panel
                local function run(pnl)
                    {body}
                end
            "#
                ),
            );
            assert_eq!(
                severities(&mut ws, id),
                vec![Some(DiagnosticSeverity::WARNING)],
                "{body}"
            );
        }
    }

    #[test]
    fn explicit_panel_field_casts_and_assignments_keep_warnings() {
        for body in [
            "---@cast tab.ContentPanel Panel\ntab.ContentPanel:Layout()",
            "tab.ContentPanel = panel\ntab.ContentPanel:Layout()",
        ] {
            let mut ws = workspace();
            let id = ws.def_file(
                "lua/autorun/client/explicit_panel_field.lua",
                &format!(
                    r#"
                ---@param tab CreationMenuTab
                ---@param panel Panel
                local function run(tab, panel)
                    {body}
                end
            "#
                ),
            );
            assert_eq!(
                severities(&mut ws, id),
                vec![Some(DiagnosticSeverity::WARNING)],
                "{body}"
            );
        }
    }

    #[test]
    fn panel_fields_cast_or_type_tested_to_the_child_are_clean() {
        for body in [
            "---@cast tab.ContentPanel DTileLayout\ntab.ContentPanel:Layout()",
            "if type(tab.ContentPanel) == \"DTileLayout\" then tab.ContentPanel:Layout() end",
        ] {
            let mut ws = workspace();
            let id = ws.def_file(
                "lua/autorun/client/narrowed_panel_field.lua",
                &format!(
                    r#"
                ---@param tab CreationMenuTab
                local function run(tab)
                    {body}
                end
            "#
                ),
            );
            assert!(severities(&mut ws, id).is_empty(), "{body}");
        }
    }

    #[test]
    fn configured_child_inference_severity_wins_over_broad_panel_hint() {
        let mut ws = workspace();
        let mut config = Emmyrc::default();
        config.diagnostics.severity.insert(
            DiagnosticCode::InferUnguardedChild,
            DiagnosticSeveritySetting::Error,
        );
        ws.update_emmyrc(config);
        let id = ws.def_file(
            "lua/autorun/client/configured_parent.lua",
            r#"
            ---@param pnl Panel
            local function run(pnl) pnl:GetParent():Layout() end
        "#,
        );
        let severities = ws
            .analysis
            .diagnose_file(id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::InferUnguardedChild.get_name().to_string(),
                    ))
            })
            .map(|diagnostic| diagnostic.severity)
            .collect::<Vec<_>>();
        assert_eq!(severities, vec![Some(DiagnosticSeverity::ERROR)]);
    }

    #[test]
    fn a_parent_result_cast_to_the_child_is_clean() {
        let mut ws = workspace();
        let id = ws.def_file(
            "lua/autorun/client/cast_parent.lua",
            r#"
            ---@param pnl Panel
            local function run(pnl)
                local parent = pnl:GetParent()
                ---@cast parent DTileLayout
                parent:Layout()
            end
        "#,
        );
        assert!(severities(&mut ws, id).is_empty());
    }

    #[test]
    fn a_parent_result_with_a_child_capability_guard_is_clean() {
        let mut ws = workspace();
        let id = ws.def_file(
            "lua/autorun/client/guarded_parent.lua",
            r#"
            ---@param pnl Panel
            local function run(pnl)
                local parent = pnl:GetParent()
                if parent.Layout then parent:Layout() end
            end
        "#,
        );
        assert!(severities(&mut ws, id).is_empty());
    }

    #[test]
    fn a_parent_result_type_tested_to_the_child_is_clean() {
        let mut ws = workspace();
        let id = ws.def_file(
            "lua/autorun/client/type_tested_parent.lua",
            r#"
            ---@param pnl Panel
            local function run(pnl)
                local parent = pnl:GetParent()
                if type(parent) == "DTileLayout" then parent:Layout() end
            end
        "#,
        );
        assert!(severities(&mut ws, id).is_empty());
    }

    #[test]
    fn broad_non_panel_contracts_keep_warnings() {
        let mut ws = workspace();
        ws.def_file(
            "lua/annotations/non_panel_contract.lua",
            r#"
            ---@class Entity
            local Entity = {}
            ---@class LayoutEntity: Entity
            ---@field Arrange fun(self: LayoutEntity)
            local LayoutEntity = {}
            ---@return Entity
            function find_entity() end
        "#,
        );
        let id = ws.def_file(
            "lua/autorun/client/non_panel_contract.lua",
            r#"
            local function run()
                local entity = find_entity()
                entity:Arrange()
            end
        "#,
        );
        assert_eq!(
            severities(&mut ws, id),
            vec![Some(DiagnosticSeverity::WARNING)]
        );
    }

    #[test]
    fn panel_contract_edits_delete_and_reopen_match_cold_severity() {
        let broad = "---@return Panel\n        function Panel:GetParent() end";
        let concrete = "---@return DTileLayout\n        function Panel:GetParent() end";
        let source = r#"
            ---@param pnl Panel
            local function run(pnl) pnl:GetParent():Layout() end
        "#;
        let mut ws = workspace();
        let api_id = ws.def_file("lua/annotations/panel_receiver.lua", API);
        let uri = ws.analysis.get_uri(api_id).unwrap();
        let id = ws.def_file("lua/autorun/client/edited_parent.lua", source);
        assert_eq!(
            severities(&mut ws, id),
            vec![Some(DiagnosticSeverity::HINT)]
        );
        let edited_api = API.replace(broad, concrete);
        ws.analysis
            .update_file_by_uri(&uri, Some(edited_api.clone()));
        let mut cold = workspace();
        cold.def_file("lua/annotations/panel_receiver.lua", &edited_api);
        let cold_id = cold.def_file("lua/autorun/client/edited_parent.lua", source);
        assert_eq!(severities(&mut ws, id), severities(&mut cold, cold_id));
        assert!(severities(&mut ws, id).is_empty());
        ws.analysis
            .remove_file_by_uri(&uri)
            .0
            .expect("removed panel contracts");
        let mut cold = VirtualWorkspace::new_with_init_std_lib();
        cold.def_gmod_call_arg_builtins();
        let cold_id = cold.def_file("lua/autorun/client/edited_parent.lua", source);
        assert_eq!(severities(&mut ws, id), severities(&mut cold, cold_id));
        ws.analysis.update_file_by_uri(&uri, Some(API.to_string()));
        assert_eq!(
            severities(&mut ws, id),
            vec![Some(DiagnosticSeverity::HINT)]
        );
    }
}

#[cfg(test)]
mod base_runtime_type_classifier_test {
    use crate::{DiagnosticCode, FileId, VirtualWorkspace};
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    const UTIL_PATH: &str = "lua/includes/util.lua";
    const PANEL_PATH: &str = "lua/vgui/prop_vectorcolor.lua";
    const UNGUARDED: &str = "expected `Vector` but found `(Color|Vector)`. Add a guard to narrow the parent to `Vector`.";

    const UTIL: &str = r#"
local C_type = type
local getmetatable = getmetatable
function type( v )
    local v_type = C_type( v )
    if ( v_type ~= "userdata" ) then return v_type end
    local metatable = getmetatable( v )
    local metaName = metatable and metatable.MetaName
    return C_type( metaName ) == "string" and metaName or "UserData"
end
function isvector( v ) return type( v ) == "Vector" end
"#;

    const PANEL: &str = r#"
---@param self VectorColorPanel
local function paint(self)
    if isvector(self.VectorValue) then
        local converted = self.VectorValue:ToColor()
    end
    local unguarded = self.VectorValue:ToColor()
end
"#;

    fn workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "lua/includes/geometry.lua",
            r#"
---@class Vector
local VECTOR = {}
---@class Color
local COLOR = {}
---@class Panel
local PANEL = {}
---@return Color
function VECTOR:ToColor() end
---@class VectorColorPanel: Panel
---@field VectorValue Color|Vector
local HOLDER = {}
"#,
        );
        ws
    }

    fn unguarded_reads(ws: &mut VirtualWorkspace, file: FileId) -> Vec<(u32, String)> {
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::InferUnguardedChild);
        let code = Some(NumberOrString::String(
            DiagnosticCode::InferUnguardedChild.get_name().to_string(),
        ));
        let mut reads = ws
            .analysis
            .diagnose_file(file, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| diagnostic.code == code)
            .map(|diagnostic| (diagnostic.range.start.line, diagnostic.message))
            .collect::<Vec<_>>();
        reads.sort();
        reads
    }

    fn reads_at(lines: &[u32]) -> Vec<(u32, String)> {
        lines
            .iter()
            .map(|line| (*line, UNGUARDED.to_string()))
            .collect()
    }

    #[test]
    fn the_base_util_type_wrapper_proves_isvector_and_leaves_unguarded_reads() {
        let mut ws = workspace();
        ws.def_file(UTIL_PATH, UTIL);
        let file = ws.def_file(PANEL_PATH, PANEL);
        assert_eq!(unguarded_reads(&mut ws, file), reads_at(&[6]));
    }

    #[test]
    fn the_color_arm_of_a_failed_isvector_still_reports_its_vector_only_read() {
        let mut ws = workspace();
        ws.def_file(UTIL_PATH, UTIL);
        let file = ws.def_file(
            PANEL_PATH,
            r#"
---@param self VectorColorPanel
local function paint(self)
    if not isvector(self.VectorValue) then
        local converted = self.VectorValue:ToColor()
    end
end
"#,
        );
        let codes = [
            DiagnosticCode::InferUnguardedChild,
            DiagnosticCode::UndefinedField,
            DiagnosticCode::UndefinedMethod,
        ]
        .map(|code| Some(NumberOrString::String(code.get_name().to_string())));
        let diagnostics = ws
            .analysis
            .diagnose_file(file, CancellationToken::new())
            .unwrap_or_default();
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.range.start.line == 4 && codes.contains(&diagnostic.code)
            }),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn a_type_wrapper_outside_base_util_or_without_a_classifier_contract_proves_nothing() {
        let unannotated = UTIL
            .replace("function type( v )", "function classify( v )")
            .replace(
                "return type( v ) == \"Vector\"",
                "return classify( v ) == \"Vector\"",
            );
        for (path, util) in [
            ("lua/autorun/sh_type_override.lua", UTIL.to_string()),
            (UTIL_PATH, unannotated),
        ] {
            let mut ws = workspace();
            ws.def_file(path, &util);
            let file = ws.def_file(PANEL_PATH, PANEL);
            assert_eq!(
                unguarded_reads(&mut ws, file),
                reads_at(&[4, 6]),
                "{path}: {util}"
            );
        }
    }

    #[test]
    fn base_util_classifier_edits_delete_and_reopen_match_cold_analysis() {
        let renamed = UTIL
            .replace("function type( v )", "function classify( v )")
            .replace(
                "return type( v ) == \"Vector\"",
                "return classify( v ) == \"Vector\"",
            );
        let mut warm = workspace();
        let util = warm.def_file(UTIL_PATH, UTIL);
        let uri = warm.analysis.get_uri(util).unwrap();
        let file = warm.def_file(PANEL_PATH, PANEL);
        assert_eq!(unguarded_reads(&mut warm, file), reads_at(&[6]));
        for (text, expected) in [
            (Some(renamed.as_str()), &[4, 6][..]),
            (None, &[4, 6][..]),
            (Some(UTIL), &[6][..]),
        ] {
            if let Some(text) = text {
                warm.analysis
                    .update_file_by_uri(&uri, Some(text.to_string()));
            } else {
                warm.analysis
                    .remove_file_by_uri(&uri)
                    .0
                    .expect("removed util");
            }
            let mut cold = workspace();
            if let Some(text) = text {
                cold.def_file(UTIL_PATH, text);
            }
            let cold_file = cold.def_file(PANEL_PATH, PANEL);
            let actual = unguarded_reads(&mut warm, file);
            assert_eq!(actual, unguarded_reads(&mut cold, cold_file), "{text:?}");
            assert_eq!(actual, reads_at(expected), "{text:?}");
        }
    }
}

#[cfg(test)]
mod define_baseclass_local_test {
    use glua_parser::{LuaAstNode, LuaExpr, LuaNameExpr};

    use crate::{Emmyrc, LuaType, VirtualWorkspace};

    /// `BaseClass.X` that only a subclass defines does not retype the local.
    #[test]
    fn a_member_only_a_subclass_defines_keeps_baseclass_on_the_named_base() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "lua/entities/base_glide/sv_wheels.lua",
            "function ENT:CreateWheel(offset) return 1 end
",
        );
        let car = ws.def_file(
            "lua/entities/base_glide_car/init.lua",
            r#"
            DEFINE_BASECLASS("base_glide")
            function ENT:CreateWheel(offset)
                local wheel = BaseClass.CreateWheel(self, offset)
                return wheel
            end
            function ENT:TriggerInput(name)
                BaseClass.TriggerInput(self, name)
            end
            "#,
        );

        let model = ws
            .analysis
            .compilation
            .get_semantic_model(car)
            .expect("semantic model");
        let mut types = model
            .get_root()
            .descendants::<LuaNameExpr>()
            .filter(|name| name.get_name_text().as_deref() == Some("BaseClass"))
            .map(|name| {
                ws.humanize_type(
                    model
                        .infer_expr(LuaExpr::NameExpr(name))
                        .unwrap_or(LuaType::Unknown),
                )
            })
            .collect::<Vec<_>>();
        types.push(ws.humanize_type(ws.local_ty(car, "wheel")));

        assert_eq!(types, ["base_glide", "base_glide", "1"]);
    }
}

#[cfg(test)]
mod covered_union_arm_test {
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};

    fn unguarded_reads(receiver_type: &str) -> Vec<String> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "lua/includes/owner_api.lua",
            r#"
            ---@class Entity
            local Entity = {}
            ---@class NPC: Entity
            local NPC = {}
            ---@class Player: Entity
            local Player = {}
            ---@class Weapon: Entity
            local Weapon = {}
            ---@return number
            function NPC:GetShootPos() end
            ---@return number
            function Player:GetShootPos() end
            "#,
        );
        let file = ws.def_file(
            "lua/weapons/weapon_knife/shared.lua",
            &format!(
                r#"
                ---@param owner {receiver_type}
                local function aim(owner)
                    return owner:GetShootPos()
                end
                "#
            ),
        );
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::InferUnguardedChild);
        let code = Some(NumberOrString::String(
            DiagnosticCode::InferUnguardedChild.get_name().to_string(),
        ));
        ws.analysis
            .diagnose_file(file, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| diagnostic.code == code)
            .map(|diagnostic| diagnostic.message)
            .collect()
    }

    /// `Entity|NPC` can be any `Entity`, so a `Player` is as likely to hold the member.
    #[test]
    fn a_covered_arm_names_the_descendants_of_the_arm_covering_it() {
        let reads = unguarded_reads("Entity|NPC");

        assert_eq!(reads.len(), 1, "{reads:?}");
        assert!(
            reads[0].contains("`NPC`") && reads[0].contains("`Player`"),
            "{reads:?}"
        );
    }

    #[test]
    fn unrelated_class_arms_name_only_the_arm_with_the_member() {
        let reads = unguarded_reads("Weapon|NPC");

        assert_eq!(reads.len(), 1, "{reads:?}");
        assert!(!reads[0].contains("`Player`"), "{reads:?}");
    }
}
