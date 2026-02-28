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
