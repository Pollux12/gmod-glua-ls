#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, VirtualWorkspace};
    use googletest::prelude::*;

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
}
