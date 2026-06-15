#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, VirtualWorkspace};
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
}
