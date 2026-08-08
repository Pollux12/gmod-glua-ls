#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, LuaMemberOwner, LuaType, VirtualWorkspace};
    use googletest::prelude::*;
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    fn diagnostic_messages_for_file(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        diagnostic_code: DiagnosticCode,
    ) -> Vec<String> {
        ws.analysis.diagnostic.enable_only(diagnostic_code);
        let code = Some(NumberOrString::String(
            diagnostic_code.get_name().to_string(),
        ));

        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| diagnostic.code == code)
            .map(|diagnostic| diagnostic.message)
            .collect()
    }
    fn latest_member_type(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
        field_name: &str,
    ) -> LuaType {
        let db = ws.analysis.compilation.get_db();
        let member = db
            .get_member_index()
            .get_file_members(file_id)
            .into_iter()
            .filter(|member| member.get_key().to_path() == field_name)
            .max_by_key(|member| member.get_id().get_position())
            .expect("expected named assignment member");
        db.get_type_index()
            .get_type_cache(&member.get_id().into())
            .expect("expected assignment member type cache")
            .as_type()
            .clone()
    }

    fn nil_diagnostic_messages_for_file(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
    ) -> Vec<String> {
        let mut messages = diagnostic_messages_for_file(ws, file_id, DiagnosticCode::NeedCheckNil);
        messages.extend(diagnostic_messages_for_file(
            ws,
            file_id,
            DiagnosticCode::UncheckedNilAccess,
        ));
        messages
    }

    #[gtest]
    fn test_inject_field_suppressed_for_dynamic_field() {
        let mut ws = VirtualWorkspace::new();
        // gmod.enabled=true, gmod.inferDynamicFields=true by default
        assert!(ws.check_code_for(
            DiagnosticCode::InjectField,
            r#"
            ---@class DynTest.Player

            ---@type DynTest.Player
            local client
            client.customField = 1
            "#
        ));
    }

    #[gtest]
    fn test_inject_field_reported_when_disabled() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.infer_dynamic_fields = false;
        ws.update_emmyrc(emmyrc);

        assert!(!ws.check_code_for(
            DiagnosticCode::InjectField,
            r#"
            ---@class DynTestDisabled.Player

            ---@type DynTestDisabled.Player
            local client
            client.customField = 1
            "#
        ));
    }

    #[gtest]
    fn test_undefined_field_suppressed_same_file() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@class DynTest2.Entity

            ---@type DynTest2.Entity
            local ent
            ent.myData = "hello"

            ---@type DynTest2.Entity
            local ent2
            local x = ent2.myData
            "#
        ));
    }

    #[gtest]
    fn test_undefined_field_reported_when_disabled() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.infer_dynamic_fields = false;
        ws.update_emmyrc(emmyrc);

        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@class DynTestDisabled2.Entity

            ---@type DynTestDisabled2.Entity
            local ent
            ent.myData = "hello"

            ---@type DynTestDisabled2.Entity
            local ent2
            local x = ent2.myData
            "#
        ));
    }

    #[gtest]
    fn test_nil_assignment_still_tracked() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::InjectField,
            r#"
            ---@class DynTest3.Player

            ---@type DynTest3.Player
            local ply
            ply.nullableField = nil
            "#
        ));
    }

    #[gtest]
    fn test_cross_file_dynamic_field() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class DynTestCross.Player

            ---@type DynTestCross.Player
            local ply
            ply.crossFileField = 42
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@type DynTestCross.Player
            local ply2
            local x = ply2.crossFileField
            "#,
        ));
    }

    #[gtest]
    fn test_multiple_dynamic_fields() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::InjectField,
            r#"
            ---@class DynTest4.Vehicle

            ---@param client DynTest4.Vehicle
            local function setup(client)
                client.chairExitVeh = nil
                client.chairExitEnterPos = nil
            end
            "#
        ));
    }

    #[gtest]
    fn test_gmod_disabled_no_suppress() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);

        assert!(!ws.check_code_for(
            DiagnosticCode::InjectField,
            r#"
            ---@class DynTestGmodOff.Player

            ---@type DynTestGmodOff.Player
            local client
            client.customField = 1
            "#
        ));
    }

    #[gtest]
    fn test_declared_fields_still_work() {
        let mut ws = VirtualWorkspace::new();
        // Fields that ARE declared should still pass without dynamic field inference
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@class DynTest5.Entity
            ---@field health number

            ---@type DynTest5.Entity
            local ent
            local h = ent.health
            "#
        ));
    }

    #[gtest]
    fn test_string_key_dynamic_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::InjectField,
            r#"
            ---@class DynTest6.Data

            ---@type DynTest6.Data
            local data
            data["dynamicKey"] = true
            "#
        ));
    }

    #[gtest]
    fn test_dynamic_field_with_function_param() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::InjectField,
            r#"
            ---@class DynTest7.Player

            ---@param ply DynTest7.Player
            ---@param veh any
            function PLUGIN_CanPlayerEnterVehicle(ply, veh)
                ply.chairExitVeh = nil
                ply.chairExitEnterPos = nil
                ply.chairExitVeh = veh
            end
            "#
        ));
    }

    #[gtest]
    fn test_param_check_handles_recursive_dynamic_field_value() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Entity
            ---@field SetNWEntity fun(self: Entity, key: string, value: Entity)

            ---@class DynTest8.Chip: Entity

            ---@type DynTest8.Chip
            local self

            self:SetNWEntity("owner", self._Owner)
            "#
        ));
    }

    #[gtest]
    fn test_dynamic_field_value_type_stays_precise_for_param_check() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class DynTest9.Entity
            ---@class DynTest9.Other

            ---@type DynTest9.Entity
            local ent
            ent.preciseCount = 1

            ---@type DynTest9.Entity
            local ent2

            ---@param value DynTest9.Other
            local function takes_other(value) end

            takes_other(ent2.preciseCount)
            "#
        ));
    }

    #[gtest]
    fn test_dynamic_field_later_assignment_slot_uses_multi_return_type() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let target_path = "lua/autorun/dynamic_multi_return_slot.lua";
        let target_source = r#"
            ---@class DynMultiReturn.First
            ---@field FirstOnly fun(self: DynMultiReturn.First)
            ---@class DynMultiReturn.Second
            ---@field SecondOnly fun(self: DynMultiReturn.Second)
            ---@class DynMultiReturn.Owner

            ---@return DynMultiReturn.First
            ---@return DynMultiReturn.Second
            local function make_pair() end

            ---@param value DynMultiReturn.First
            local function takes_first(value) end

            ---@type DynMultiReturn.Owner
            local owner
            local ignored
            ignored, owner.value = make_pair()

            takes_first(owner.value)
            owner.value:SecondOnly()
            "#;
        let file_id = ws.def_file(target_path, target_source);

        let before_param =
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::ParamTypeMismatch);
        verify_that!(&before_param, not(is_empty()))?;
        let before_method =
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::UndefinedMethod);
        verify_that!(&before_method, is_empty())?;

        let uri = ws.virtual_url_generator.new_uri(target_path);
        ws.analysis
            .update_file_text_only(&uri, format!("{target_source}\n"));
        ws.analysis.reindex_files(vec![file_id]);

        let after_param =
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::ParamTypeMismatch);
        verify_that!(after_param, eq(&before_param))?;
        let after_method =
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::UndefinedMethod);
        verify_that!(after_method, eq(&before_method))
    }

    #[gtest]
    fn test_dynamic_field_inferred_nullable_multi_return_slots_keep_their_types() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/autorun/client/inherited_multi_return.lua",
            r#"
            local owner = {}

            ---@return number
            ---@return number
            local function get_size() end

            ---@type fun(self: table, enabled: boolean): string
            owner.GetFont = function(self, enabled)
                if not enabled then return end
                local name = "font"
                local width, height = get_size()
                return name, width, height
            end

            owner.CurrentFont, owner.FontWidth, owner.FontHeight = owner:GetFont(true)

            ---@param value number
            local function takes_number(value) end

            takes_number(owner.FontWidth)
            takes_number(owner.FontHeight)
            "#,
        );

        let font_width = latest_member_type(&ws, file_id, "FontWidth");
        let font_width_desc = ws.humanize_type(font_width);
        assert!(
            font_width_desc.contains("number"),
            "expected FontWidth to retain the second return slot, got {font_width_desc}"
        );
        verify_that!(
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::ParamTypeMismatch),
            is_empty()
        )?;
        verify_that!(
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::AssignTypeMismatch),
            is_empty()
        )
    }

    #[gtest]
    fn test_dynamic_field_direct_return_doc_keeps_undeclared_tail_unavailable() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/autorun/client/direct_doc_multi_return.lua",
            r#"
            ---@return string
            local function get_font()
                return "font", 8
            end

            local owner = {}
            owner.CurrentFont, owner.FontWidth = get_font()
            "#,
        );

        let font_width = latest_member_type(&ws, file_id, "FontWidth");
        let font_width_desc = ws.humanize_type(font_width);
        assert!(
            !font_width_desc.contains("number"),
            "direct return documentation must remain authoritative, got {font_width_desc}"
        );
    }

    #[gtest]
    fn test_inherited_return_doc_preserves_cross_file_inferred_tail_consumers() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "annotations/gmod.lua",
            r#"
            ---@meta
            ---@class Panel
            ---@return string
            function Panel:GetFont() end
            ---@class DFrame : Panel
            "#,
        );
        let producer_path = "lua/vgui/editor_frame.lua";
        let producer_source = r#"
            local PANEL = {}

            ---@return number
            ---@return number
            local function get_size() end

            function PANEL:GetFont(enabled)
                if not enabled then return end
                local name = "font"
                local width, height = get_size()
                return name, width, height
            end

            vgui.Register("EditorFrame", PANEL, "DFrame")
            "#;
        let producer_file = ws.def_file(producer_path, producer_source);
        let consumer_file = ws.def_file(
            "lua/autorun/client/editor.lua",
            r#"
            SF = { Editor = {} }
            SF.Editor.editor = vgui.Create("EditorFrame")

            local owner = {}
            owner.CurrentFont, owner.FontWidth, owner.FontHeight = SF.Editor.editor:GetFont(true)

            ---@param value number
            local function takes_number(value) end

            takes_number(owner.FontWidth)
            takes_number(owner.FontHeight)
            "#,
        );

        let font_width = latest_member_type(&ws, consumer_file, "FontWidth");
        let font_width_desc = ws.humanize_type(font_width);
        assert!(
            font_width_desc.contains("number"),
            "expected the cross-file second return slot to refresh, got {font_width_desc}"
        );
        verify_that!(
            diagnostic_messages_for_file(&mut ws, consumer_file, DiagnosticCode::ParamTypeMismatch),
            is_empty()
        )?;
        verify_that!(
            diagnostic_messages_for_file(
                &mut ws,
                consumer_file,
                DiagnosticCode::AssignTypeMismatch
            ),
            is_empty()
        )?;
        verify_that!(
            nil_diagnostic_messages_for_file(&mut ws, consumer_file),
            is_empty()
        )?;

        let producer_uri = ws.virtual_url_generator.new_uri(producer_path);
        let updated_producer_source =
            producer_source.replace("---@return number", "---@return boolean");
        ws.analysis
            .update_file_text_only(&producer_uri, updated_producer_source);
        ws.analysis.reindex_files(vec![producer_file]);

        let updated_font_width = latest_member_type(&ws, consumer_file, "FontWidth");
        let updated_font_width_desc = ws.humanize_type(updated_font_width);
        assert!(
            updated_font_width_desc.contains("boolean"),
            "expected producer reindex to refresh the second return slot, got {updated_font_width_desc}"
        );
        verify_that!(
            diagnostic_messages_for_file(&mut ws, consumer_file, DiagnosticCode::ParamTypeMismatch),
            len(eq(2))
        )?;

        ws.analysis
            .remove_file_by_uri(&producer_uri)
            .expect("producer must be removable");
        let removed_font_width = latest_member_type(&ws, consumer_file, "FontWidth");
        let removed_font_width_desc = ws.humanize_type(removed_font_width);
        assert!(
            removed_font_width_desc == "any",
            "expected producer deletion to invalidate the later return slot, got {removed_font_width_desc}"
        );

        ws.analysis
            .update_file_by_uri(&producer_uri, Some(producer_source.to_string()))
            .expect("producer must reopen");
        let reopened_font_width = latest_member_type(&ws, consumer_file, "FontWidth");
        let reopened_font_width_desc = ws.humanize_type(reopened_font_width);
        assert!(
            reopened_font_width_desc.contains("number"),
            "expected producer reopen to restore the second return slot, got {reopened_font_width_desc}"
        );
        verify_that!(
            diagnostic_messages_for_file(&mut ws, consumer_file, DiagnosticCode::ParamTypeMismatch),
            is_empty()
        )
    }

    #[gtest]
    fn test_dynamic_field_later_assignment_slot_keeps_one_to_one_rhs_type() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/autorun/dynamic_one_to_one_slot.lua",
            r#"
            ---@class DynOneToOne.First
            ---@field FirstOnly fun(self: DynOneToOne.First)
            ---@class DynOneToOne.Second
            ---@field SecondOnly fun(self: DynOneToOne.Second)
            ---@class DynOneToOne.Owner

            ---@param value DynOneToOne.First
            local function takes_first(value) end

            ---@type DynOneToOne.First
            local first
            ---@type DynOneToOne.Second
            local second
            ---@type DynOneToOne.Owner
            local owner
            local ignored
            ignored, owner.value = first, second

            takes_first(owner.value)
            owner.value:SecondOnly()
            "#,
        );

        verify_that!(
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::ParamTypeMismatch),
            not(is_empty())
        )?;
        verify_that!(
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            is_empty()
        )
    }

    #[gtest]
    fn test_dynamic_field_missing_multi_return_slot_is_nil() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/autorun/dynamic_missing_return_slot.lua",
            r#"
            ---@class DynMissingReturn.First
            ---@field FirstOnly fun(self: DynMissingReturn.First)
            ---@class DynMissingReturn.Owner

            ---@return DynMissingReturn.First
            local function make_one() end

            ---@param value DynMissingReturn.First
            local function takes_first(value) end

            ---@type DynMissingReturn.Owner
            local owner
            local ignored
            ignored, owner.value = make_one()

            takes_first(owner.value)
            owner.value:FirstOnly()
            "#,
        );

        verify_that!(
            nil_diagnostic_messages_for_file(&mut ws, file_id),
            not(is_empty())
        )
    }

    #[gtest]
    fn test_dynamic_field_defined_on_base_visible_to_subclass() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@class DynTest10.Base
            ---@class DynTest10.Child : DynTest10.Base

            ---@type DynTest10.Base
            local base
            base.sharedDynamic = 1

            ---@type DynTest10.Child
            local child
            local value = child.sharedDynamic
            "#
        ));
    }

    #[gtest]
    fn test_global_setmetatable_dynamic_fields_stay_scope_local() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@class DynTest11.Meta
            DYNTEST_META = {}
            DYNTEST_META.__index = DYNTEST_META

            function BuildDynTestA()
                DYNTEST_OBJ = {}
                setmetatable(DYNTEST_OBJ, DYNTEST_META)
                DYNTEST_OBJ.scopedField = true
            end

            function DYNTEST_META:ReadScoped()
                return self.scopedField
            end

            function BuildDynTestB()
                DYNTEST_OBJ = {}
                DYNTEST_OBJ.otherScopeField = true
            end

            function DYNTEST_META:ReadOtherScope()
                return self.otherScopeField
            end
            "#
        ));
    }

    #[gtest]
    fn test_later_sibling_dynamic_field_recovers_nullable_class_type() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/vgui/dynamic_sibling.lua",
            r#"
            ---@class DynSibling.DHTML
            ---@field OpenURL fun(self: DynSibling.DHTML, url: string)

            ---@class DynSibling.Owner

            ---@param value number
            local function takes_number(value) end

            ---@type DynSibling.Owner
            local owner

            local function use_browser()
                takes_number(owner.browser)
                owner.browser:OpenURL("https://example.com")
            end

            local function init_browser()
                ---@type DynSibling.DHTML
                local browser
                owner.browser = browser
            end
            "#,
        );

        verify_that!(
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::ParamTypeMismatch),
            not(is_empty())
        )?;
        verify_that!(
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            is_empty()
        )?;
        verify_that!(
            nil_diagnostic_messages_for_file(&mut ws, file_id),
            not(is_empty())
        )
    }

    #[gtest]
    fn test_earlier_sibling_dynamic_field_stays_nullable() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/vgui/dynamic_earlier_sibling.lua",
            r#"
            ---@class DynEarlierSibling.DHTML
            ---@field OpenURL fun(self: DynEarlierSibling.DHTML, url: string)

            ---@class DynEarlierSibling.Owner

            ---@param value number
            local function takes_number(value) end

            ---@type DynEarlierSibling.Owner
            local owner

            local function init_browser()
                ---@type DynEarlierSibling.DHTML
                local browser
                owner.browser = browser
            end

            local function use_browser()
                takes_number(owner.browser)
                owner.browser:OpenURL("https://example.com")
            end
            "#,
        );

        verify_that!(
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::ParamTypeMismatch),
            not(is_empty())
        )?;
        verify_that!(
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            is_empty()
        )?;
        verify_that!(
            nil_diagnostic_messages_for_file(&mut ws, file_id),
            not(is_empty())
        )
    }

    #[gtest]
    fn test_guarded_later_sibling_dynamic_field_is_safe() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/vgui/dynamic_sibling_guarded.lua",
            r#"
            ---@class DynSiblingGuarded.DHTML
            ---@field OpenURL fun(self: DynSiblingGuarded.DHTML, url: string)

            ---@class DynSiblingGuarded.Owner

            ---@param value number
            local function takes_number(value) end

            ---@type DynSiblingGuarded.Owner
            local owner

            local function use_browser()
                if owner.browser then
                    takes_number(owner.browser)
                    owner.browser:OpenURL("https://example.com")
                end
            end

            local function init_browser()
                ---@type DynSiblingGuarded.DHTML
                local browser
                owner.browser = browser
            end
            "#,
        );

        verify_that!(
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::ParamTypeMismatch),
            not(is_empty())
        )?;
        verify_that!(
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            is_empty()
        )?;
        verify_that!(
            nil_diagnostic_messages_for_file(&mut ws, file_id),
            is_empty()
        )
    }

    #[gtest]
    fn test_dynamic_field_order_stays_position_sensitive_within_scope() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/autorun/dynamic_ordering.lua",
            r#"
            ---@class DynOrdering.FunctionOwner
            ---@class DynOrdering.TopLevelOwner

            ---@param value number
            local function takes_number(value) end

            ---@param self DynOrdering.FunctionOwner
            local function same_function(self)
                takes_number(self.value)
                self.value = "assigned"
                takes_number(self.value)
            end

            ---@type DynOrdering.TopLevelOwner
            local owner
            takes_number(owner.value)
            owner.value = "assigned"
            takes_number(owner.value)
            "#,
        );

        verify_that!(
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::ParamTypeMismatch),
            len(eq(2))
        )
    }

    #[gtest]
    fn test_multiple_later_sibling_definitions_union_stably_after_reindex() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let target_path = "lua/vgui/dynamic_sibling_union.lua";
        let target_source = r#"
            ---@class DynSiblingUnion.Owner

            ---@param value string
            local function takes_string(value) end
            ---@param value number
            local function takes_number(value) end

            ---@type DynSiblingUnion.Owner
            local owner

            local function use_value()
                takes_string(owner.value)
                takes_number(owner.value)
            end

            local function init_string()
                owner.value = "text"
            end

            local function init_number()
                owner.value = 42
            end
            "#;
        let file_id = ws.def_file(target_path, target_source);

        let before =
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::ParamTypeMismatch);
        verify_that!(&before, len(eq(2)))?;

        let uri = ws.virtual_url_generator.new_uri(target_path);
        ws.analysis
            .update_file_text_only(&uri, format!("{target_source}\n"));
        ws.analysis.reindex_files(vec![file_id]);

        let after =
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::ParamTypeMismatch);
        verify_that!(after, eq(&before))
    }

    #[gtest]
    fn test_gmod_drive_registered_method_dispatch_has_no_undefined_fields() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        let base_file_id = ws.def_file(
            "lua/drive/drive_base.lua",
            r##"
            AddCSLuaFile()

            drive.Register( "drive_base",
            {
                Init = function( self, cmd ) end,
                SetupControls = function( self, cmd ) end,
                StartMove = function( self, mv, cmd ) end,
                Move = function( self, mv ) end,
                FinishMove = function( self, mv ) end,
                CalcView = function( self, view ) end,
            } )
            "##,
        );

        let file_id = ws.def_file(
            "lua/includes/modules/drive.lua",
            r##"
            local IsValid = IsValid
            local setmetatable = setmetatable
            local SERVER = SERVER
            local util = util
            local ErrorNoHalt = ErrorNoHalt
            local baseclass = baseclass
            local LocalPlayer = LocalPlayer

            module( "drive" )

            local Type = {}

            function Register( name, table, base )
                Type[ name ] = table

                if ( base ) then
                    Type[ base ] = Type[ base ] or baseclass.Get( base )
                    setmetatable( Type[ name ], { __index = Type[ base ] } )
                end

                if ( SERVER ) then
                    util.AddNetworkString( name )
                end

                baseclass.Set( name, Type[ name ] )
            end

            function PlayerStartDriving( ply, ent, mode )
                local method = Type[mode]
                if ( !method ) then ErrorNoHalt( "Unknown drive type " .. ( mode ) .. "!\n" ) return end

                local id = util.NetworkStringToID( mode )

                ply:SetDrivingEntity( ent, id )
            end

            function GetMethod(ply)
                if ( !ply:IsDrivingEntity() ) then return end

                local ent = ply:GetDrivingEntity()
                local modeid = ply:GetDrivingMode()

                if ( !IsValid( ent ) || modeid == 0 ) then return end

                local method = ply.m_CurrentDriverMethod
                if ( method && method.Entity == ent && method.ModeID == modeid ) then return method end

                local modename = util.NetworkIDToString( modeid )
                if ( !modename ) then return end

                local type = Type[ modename ]
                if ( !type ) then return end

                local method = {}
                method.Entity = ent
                method.Player = ply
                method.ModeID = modeid

                setmetatable( method, { __index = type } )

                ply.m_CurrentDriverMethod = method

                method:Init()
                return method
            end

            function CreateMove( cmd )
                local method = GetMethod( LocalPlayer() )
                if ( !method ) then return end

                method:SetupControls( cmd )
                return true
            end

            function CalcView( ply, view )
                local method = GetMethod( ply )
                if ( !method ) then return end

                method:CalcView( view )
                return true
            end

            function StartMove( ply, mv, cmd )
                local method = GetMethod( ply )
                if ( !method ) then return end

                method:StartMove( mv, cmd )
                return true
            end

            function Move( ply, mv )
                local method = GetMethod( ply )
                if ( !method ) then return end

                method:Move( mv )
                return true
            end

            function FinishMove( ply, mv )
                local method = GetMethod( ply )
                if ( !method ) then return end

                method:FinishMove( mv )

                if ( method.StopDriving ) then
                    PlayerStopDriving( ply )
                end

                return true
            end
            "##,
        );

        let base_diagnostics =
            diagnostic_messages_for_file(&mut ws, base_file_id, DiagnosticCode::UndefinedField);
        let diagnostics =
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::UndefinedField);

        verify_that!(base_diagnostics, is_empty())?;
        verify_that!(diagnostics, is_empty())
    }

    #[gtest]
    fn test_unresolved_metatable_index_suppresses_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            local t = {}
            local unresolved = GetDynamicIndex()
            setmetatable(t, { __index = unresolved })
            local value = t.anything
            "#
        ));
    }

    #[gtest]
    fn test_known_metatable_index_typo_still_reports_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@class DynTestKnownIndex
            ---@field knownField number
            local KnownClass = {}

            ---@type DynTestKnownIndex
            local known = KnownClass

            local t = {}
            setmetatable(t, { __index = known })
            local value = t.typoField
            "#
        ));
    }

    #[gtest]
    fn test_metatable_without_index_still_reports_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            local t = {}
            setmetatable(t, {})
            local value = t.foo
            "#
        ));
    }

    #[gtest]
    fn test_plain_table_still_reports_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            local t = {}
            local value = t.foo
            "#
        ));
    }

    #[gtest]
    fn test_same_file_global_call_site_overrides_gmod_param_name_hint() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_file_for(
            DiagnosticCode::ParamTypeMismatch,
            "lua/postprocess/bloom.lua",
            r#"
            ---@class Color

            ---@class ConVar
            ---@return number
            function ConVar:GetFloat() end
            ---@return boolean
            function ConVar:GetBool() end

            ---@return ConVar
            function CreateClientConVar(name, default, shouldsave, userinfo, helptext, min, max) end

            hook = hook or {}
            function hook.Add(eventName, identifier, func) end

            ---@class Material
            ---@param key string
            ---@param value number
            function Material:SetFloat(key, value) end

            ---@type Material
            local mat_Bloom
            local pp_bloom = CreateClientConVar("pp_bloom", "1", true, false)
            local pp_bloom_color = CreateClientConVar("pp_bloom_color", "1", true, false)

            function DrawBloom(darken, multiply, sizex, sizey, passes, color, colr, colg, colb)
                mat_Bloom:SetFloat("$colormul", color)
            end

            hook.Add("RenderScreenspaceEffects", "RenderBloom", function()
                if not pp_bloom:GetBool() then return end
                DrawBloom(0.65, 1, 9, 9, 1, pp_bloom_color:GetFloat(), 1, 1, 1)
            end)
            "#,
        ));
    }

    #[gtest]
    fn test_same_file_member_global_call_site_overrides_gmod_param_name_hint() {
        let mut ws = VirtualWorkspace::new();
        let target_file = ws.def_file(
            "lua/postprocess/workspace_bloom.lua",
            r#"
            ---@class Color

            Namespace = Namespace or {}

            ---@param value number
            local function takes_number(value) end

            function Namespace.AcceptColorName(color)
                takes_number(color)
            end

            Namespace.AcceptColorName(123)
            "#,
        );

        assert_that!(
            diagnostic_messages_for_file(&mut ws, target_file, DiagnosticCode::ParamTypeMismatch),
            is_empty()
        );
    }

    #[gtest]
    fn test_reindexing_same_file_refreshes_call_site_param_evidence() {
        let mut ws = VirtualWorkspace::new();
        let target_path = "lua/postprocess/workspace_bloom.lua";
        let target_source = r#"
            ---@class Color

            Namespace = Namespace or {}

            ---@param value number
            local function takes_number(value) end

            function Namespace.AcceptColorName(color)
                takes_number(color)
            end

            Namespace.AcceptColorName(123)
            "#;
        let target_file = ws.def_file(target_path, target_source);

        assert_that!(
            diagnostic_messages_for_file(&mut ws, target_file, DiagnosticCode::ParamTypeMismatch),
            is_empty()
        );

        let uri = ws.virtual_url_generator.new_uri(target_path);
        ws.analysis
            .update_file_text_only(&uri, format!("{target_source}\n"));
        ws.analysis.reindex_files(vec![target_file]);

        assert_that!(
            diagnostic_messages_for_file(&mut ws, target_file, DiagnosticCode::ParamTypeMismatch),
            is_empty(),
            "same-file reindex should refresh call-site evidence"
        );
    }

    #[gtest]
    fn test_direct_param_return_alias_preserves_call_argument_panel_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "annotations/gmod.lua",
            r#"
            ---@meta
            ---@class Entity
            ---@class Panel : Entity
            ---@class DPanel : Panel
            ---@class DTextEntry : Panel
            function DTextEntry:SetEditable(enabled) end
            "#,
        );
        let target_file = ws.def_file(
            "lua/starfall/editor/direct_return.lua",
            r#"
            local Editor = { Components = {} }

            function Editor:AddComponent(panel)
                self.Components[#self.Components + 1] = panel
                return panel
            end

            Editor.Generic = Editor:AddComponent(vgui.Create("DPanel"))
            Editor.Credit = Editor:AddComponent(vgui.Create("DTextEntry"))
            Editor.Credit:SetEditable(false)
            "#,
        );

        assert_that!(
            diagnostic_messages_for_file(&mut ws, target_file, DiagnosticCode::UndefinedMethod),
            is_empty()
        );
    }

    #[gtest]
    fn test_class_name_param_factory_return_specializes_vgui_create() {
        // Real pattern: AddTab(icon, tip, panelClass) does
        //   tab.panel = vgui.Create(panelClass); return tab.panel
        // Call site `frame:AddTab(..., "DPanel")` must yield DPanel so
        // DPanel-only methods like SetPaintBackground are defined.
        let mut ws = VirtualWorkspace::new();
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "annotations/gmod.lua",
            r#"
            ---@meta
            ---@class Entity
            ---@class Panel : Entity
            function Panel:Dock() end
            ---@class DPanel : Panel
            function DPanel:SetPaintBackground(paint) end
            ---@class DFrame : Panel
            ---@generic T: Panel
            ---@param classname `T`
            ---@param parent? Panel
            ---@return (instance) T?
            function vgui.Create(classname, parent) end
            ---@generic T: Panel
            ---@param classname string
            ---@param panelTable T
            ---@param baseName? string
            ---@return T
            function vgui.Register(classname, panelTable, baseName) end
            "#,
        );
        let target_file = ws.def_file(
            "lua/includes/modules/styled_theme_tabbed_frame.lua",
            r#"
            local TABBED_FRAME = {}

            function TABBED_FRAME:AddTab(icon, tooltip, panelClass)
                panelClass = panelClass or "DScrollPanel"
                local tab = {}
                tab.panel = vgui.Create(panelClass, self)
                return tab.panel
            end

            vgui.Register("Styled_TabbedFrame", TABBED_FRAME, "DFrame")

            local frame = vgui.Create("Styled_TabbedFrame")
            local panelExtension = frame:AddTab("icon.png", "extensions", "DPanel")
            panelExtension:SetPaintBackground(false)
            panelExtension:Dock()
            "#,
        );

        assert_that!(
            diagnostic_messages_for_file(&mut ws, target_file, DiagnosticCode::UndefinedMethod),
            is_empty()
        );
    }

    #[gtest]
    fn test_reassigned_param_is_not_treated_as_direct_return_alias() {
        let mut ws = VirtualWorkspace::new();
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "annotations/gmod.lua",
            r#"
            ---@meta
            ---@class Entity
            ---@class Panel : Entity
            ---@class DPanel : Panel
            ---@class DTextEntry : Panel
            function DTextEntry:SetEditable(enabled) end
            "#,
        );
        let target_file = ws.def_file(
            "lua/starfall/editor/reassigned_return.lua",
            r#"
            local Editor = {}

            function Editor:Replace(panel)
                panel = vgui.Create("DPanel")
                return panel
            end

            local replaced = Editor:Replace(vgui.Create("DTextEntry"))
            replaced:SetEditable(false)
            "#,
        );

        assert_that!(
            diagnostic_messages_for_file(&mut ws, target_file, DiagnosticCode::UndefinedMethod),
            not(is_empty())
        );
    }

    #[gtest]
    fn test_deferred_vgui_callback_member_owner_and_rhs_survive_dynamic_analysis() {
        let mut ws = VirtualWorkspace::new();
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "annotations/gmod.lua",
            r#"
            ---@meta

            ---@class Entity
            ---@class NULL : Entity
            ---@type NULL
            NULL = nil

            ---@class Panel : Entity
            function Panel:Dock() end
            function Panel:SetVisible(visible) end

            ---@class DPanel : Panel
            ---@class DFrame : DPanel
            ---@class DTree : DPanel
            local DTree = {}
            ---@class DTree_Node : DPanel
            ---@class ContentContainer : DPanel
            ---@class ContentSidebar : DPanel
            ---@field Tree DTree

            ---@param node DTree_Node
            function DTree:OnNodeSelected(node) end

            ---@return DTree_Node
            function DTree:AddNode() end

            ---@return TypeGuard<any>
            ---@return_cast object -NULL
            ---@valid_guard
            function IsValid(object) end
            "#,
        );
        let target_path = "lua/starfall/editor/dynamic_selected.lua";
        let target_source = r#"
            local PANEL = {}
            vgui.Register("StarfallFrame", PANEL, "DFrame")

            PANEL = {}
            vgui.Register("StarfallPanel", PANEL, "DPanel")

            local frame = vgui.Create("StarfallFrame")

            function frame:Initialize()
                frame.ContentNavBar = vgui.Create("ContentSidebar", frame)
                frame.ContentNavBar.Tree.OnNodeSelected = function(self, node)
                    if not IsValid(node.propPanel) then return end
                    if IsValid(frame.PropPanel.selected) then
                        frame.PropPanel.selected:SetVisible(false)
                        frame.PropPanel.selected = nil
                    end

                    frame.PropPanel.selected = node.propPanel
                    frame.PropPanel.selected:Dock()
                    frame.PropPanel.selected:SetVisible(true)
                end

                frame.PropPanel = vgui.Create("StarfallPanel", frame)

                local node = frame.ContentNavBar.Tree:AddNode()
                node.propPanel = vgui.Create("ContentContainer", frame.PropPanel)
            end
            "#;
        let target_file = ws.def_file(target_path, target_source);

        assert_that!(
            diagnostic_messages_for_file(&mut ws, target_file, DiagnosticCode::UndefinedField),
            is_empty()
        );
        assert_that!(
            diagnostic_messages_for_file(&mut ws, target_file, DiagnosticCode::UndefinedMethod),
            is_empty()
        );

        {
            let db = ws.analysis.compilation.get_db();
            let selected_members = db
                .get_member_index()
                .get_file_members(target_file)
                .into_iter()
                .filter(|member| member.get_key().to_path() == "selected")
                .collect::<Vec<_>>();
            assert_eq!(selected_members.len(), 2);
            assert!(selected_members.iter().all(|member| {
                db.get_member_index()
                    .get_current_owner(&member.get_id())
                    .is_some()
            }));
        }
        assert_eq!(
            ws.humanize_type(latest_member_type(&ws, target_file, "selected")),
            "ContentContainer",
            "the selected write cache must retain its resolved RHS type"
        );
        let uri = ws.virtual_url_generator.new_uri(target_path);
        ws.analysis
            .update_file_text_only(&uri, format!("{target_source}\n"));
        ws.analysis.reindex_files(vec![target_file]);
        assert_that!(
            diagnostic_messages_for_file(&mut ws, target_file, DiagnosticCode::UndefinedField),
            is_empty()
        );
        assert_eq!(
            ws.humanize_type(latest_member_type(&ws, target_file, "selected")),
            "ContentContainer",
            "same-file reindex must rebuild the resolved selected write cache"
        );
    }

    #[gtest]
    fn test_deferred_uninformative_member_rhs_stays_any_without_owner_pollution() {
        let mut ws = VirtualWorkspace::new();
        let target_path = "lua/vgui/unresolved_member_rhs.lua";
        let target_source = r#"
            ---@type any
            local source
            local holder = {}
            holder.value = source.missing
        "#;
        let file_id = ws.def_file(target_path, target_source);

        assert_eq!(
            ws.humanize_type(latest_member_type(&ws, file_id, "value")),
            "any"
        );
        {
            let db = ws.analysis.compilation.get_db();
            let value_members = db
                .get_member_index()
                .get_file_members(file_id)
                .into_iter()
                .filter(|member| member.get_key().to_path() == "value")
                .collect::<Vec<_>>();
            assert_eq!(value_members.len(), 1);
            assert!(matches!(
                db.get_member_index()
                    .get_current_owner(&value_members[0].get_id()),
                Some(LuaMemberOwner::Element(_))
            ));
        }

        let uri = ws.virtual_url_generator.new_uri(target_path);
        ws.analysis
            .update_file_text_only(&uri, format!("{target_source}\n"));
        ws.analysis.reindex_files(vec![file_id]);
        assert_eq!(
            ws.humanize_type(latest_member_type(&ws, file_id, "value")),
            "any"
        );
    }

    #[gtest]
    fn test_deferred_dynamic_member_fix_keeps_typed_entity_missing_method_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/entity_missing_method.lua",
            r#"
            ---@class Entity
            ---@type Entity
            local entity
            entity:DefinitelyMissing()
            "#,
        );

        assert_eq!(
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            vec!["Undefined method `DefinitelyMissing`. "]
        );
    }

    #[gtest]
    fn test_deferred_dynamic_member_fix_keeps_straight_line_nil_check() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/straight_line_nil.lua",
            r#"
            ---@class Panel
            function Panel:Dock() end

            ---@type Panel?
            local panel = nil
            panel:Dock()
            "#,
        );

        assert_that!(
            nil_diagnostic_messages_for_file(&mut ws, file_id),
            not(is_empty())
        );
    }

    /// StarfallEx builds `instance.data = {}` and then fills it from
    /// library files whose receiver arrives as an unannotated closure param
    /// (`SF.Modules[..][..].init(self)` after a `CompileFile` round trip),
    /// so every `instance.data.<x> = ...` write is attributed to nothing.
    #[gtest]
    fn test_shapeless_table_stays_quiet_only_for_unattributably_written_fields() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "lua/holder.lua",
            r#"
            Holder = {}
            Holder.data = {}
            Holder.shaped = {}
            Holder.shaped.known = 1
            "#,
        );
        ws.def_file(
            "lua/writer.lua",
            r#"
            return function(instance)
                instance.data.filledElsewhere = {}
            end
            "#,
        );
        let reader = ws.def_file(
            "lua/reader.lua",
            r#"
            print(Holder.data.filledElsewhere)
            print(Holder.data.neverWrittenAnywhere)
            print(Holder.shaped.neverWritten)
            "#,
        );

        assert_eq!(
            diagnostic_messages_for_file(&mut ws, reader, DiagnosticCode::UndefinedField),
            vec![
                // No unattributed writer for this name.
                "Undefined field `neverWrittenAnywhere`. ",
                // `shaped` has a known member, so it has a shape.
                "Undefined field `neverWritten`. ",
            ]
        );
    }

    /// The plain typo case must keep reporting: nothing writes `meow` anywhere.
    #[gtest]
    fn test_empty_local_table_still_reports_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/same_file.lua",
            r#"
            local test = {}
            print(test.meow)
            "#,
        );

        assert_eq!(
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::UndefinedField),
            vec!["Undefined field `meow`. "]
        );
    }

    /// A table populated only through computed keys has no statically knowable
    /// field names, so reads off it are never actionable typos.
    #[gtest]
    fn test_pure_computed_key_registry_suppresses_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/registry.lua",
            r#"
            ---@class WildReg.Registry

            ---@type WildReg.Registry
            local registry

            ---@param name string
            local function add(name, value)
                registry[name] = value
            end

            add("a", 1)
            print(registry.Anything)
            "#,
        );

        assert_eq!(
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::UndefinedField),
            Vec::<String>::new()
        );
    }

    /// One named write gives the table a shape again, so bogus names still report.
    #[gtest]
    fn test_mixed_computed_key_table_still_reports_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/mixed_registry.lua",
            r#"
            ---@class WildRegMixed.Registry
            ---@field known number

            ---@type WildRegMixed.Registry
            local registry

            ---@param name string
            local function add(name, value)
                registry[name] = value
            end

            add("a", 1)
            print(registry.bogus)
            "#,
        );

        assert_eq!(
            diagnostic_messages_for_file(&mut ws, file_id, DiagnosticCode::UndefinedField),
            vec!["Undefined field `bogus`. "]
        );
    }
}
