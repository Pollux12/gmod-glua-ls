#[cfg(test)]
mod test {
    use std::sync::Arc;

    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn test_749() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::Unused,
            r#"
            --- @alias Timer {start: fun(timer, delay: integer, cb: function), stop: fun()}

            --- @return Timer
            local new_timer = function() end


            local timer --- @type Timer?

            local function foo()
                timer = timer or new_timer()

                timer:start(100, function()
                    timer:stop()
                    timer = nil

                    -- code
                end)
            end

            foo()
        "#
        ));
    }

    #[test]
    fn test_unused_self_is_separate_code() {
        let mut ws = VirtualWorkspace::new();
        let code = r#"
            local PLUGIN = {}

            function PLUGIN:PlayerLeaveVehicle(client, veh)
                return client, veh
            end
        "#;

        assert!(!ws.check_code_for(DiagnosticCode::UnusedSelf, code));
        assert!(ws.check_code_for(DiagnosticCode::Unused, code));
    }

    #[test]
    fn test_gmod_ignores_unused_params() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::Unused,
            r#"
                local function consume(v)
                    return v
                end

                local function foo(vehicle)
                    return consume(1)
                end

                foo(1)
            "#,
        ));
    }

    #[test]
    fn test_scripted_class_seeded_var_no_unused_diagnostic() {
        // A gamemode file with no GM method definitions must NOT trigger
        // "GM is never used" — the GM variable is injected by the LS, not written by the user.
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        // File with path triggering gamemode scripted-class scope, but no GM usage.
        assert!(ws.check_file_for(
            DiagnosticCode::Unused,
            "gamemodes/test/gamemode/cl_test1.lua",
            r#"
                local spawnIconFile = file.Open("test.png", "rb", "GAME")
                if spawnIconFile then end
            "#,
        ));
    }

    #[test]
    fn test_scripted_class_seeded_var_no_redefined_local_diagnostic() {
        // A gamemode file that declares `local GM = {}` must NOT trigger
        // "redefined local" against the LS-injected GM seed decl.
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        assert!(ws.check_file_for(
            DiagnosticCode::RedefinedLocal,
            "gamemodes/test/gamemode/init.lua",
            r#"
                local GM = {}
                function GM:PlayerSpawn(ply) end
            "#,
        ));
    }

    #[test]
    fn test_non_gmod_reports_unused_params() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = false;
        ws.analysis.update_config(Arc::new(emmyrc));
        assert!(!ws.check_code_for(
            DiagnosticCode::Unused,
            r#"
                local function consume(v)
                    return v
                end

                local function foo(vehicle)
                    return consume(1)
                end

                foo(1)
            "#,
        ));
    }
}
