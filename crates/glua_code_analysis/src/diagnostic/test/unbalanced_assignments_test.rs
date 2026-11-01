#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn test() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local x, y, z = print()
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local x, y, z
            x, y, z = 1
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local x, y, z = 1
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local x, y, z
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
                local x, y, z
                x, y, z = 1
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
                X, Y, Z = 1
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            T = {}
            T.x, T.y, T.z = 1
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            T = {}
            T['x'], T['y'], T['z'] = 1
        "#
        ));
    }

    #[test]
    fn test_issue_232() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local a, b, c = string.match("hello world", "(%w+) (%w+)")
            "#
        ));
    }

    #[test]
    fn test_2() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            ---@return any
            local function test()
            end

            local a, b, c = test()
            "#
        ));
    }

    #[test]
    fn test_3() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            ---@class D18
            local M
            function M:send()
            end
            local suc, err = M:send()
            "#
        ));
    }

    #[test]
    fn test_pcall() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
                ---@type any
                local f
                local ok, result, err = pcall(f)
            "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
                ---@type any
                local f
                local ok, result, err = xpcall(f, debug.traceback)
            "#
        ));
    }

    #[test]
    fn test_nil_literal_resets_remaining_targets() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local x, y, z
            x, y, z = nil
            "#
        ));
        assert!(ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local a, b, c = nil
            "#
        ));
    }

    #[test]
    fn test_padded_local_written_before_every_read() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local x, y, item = 1, 1
            local function use()
                item = {}
                return item
            end
            return x, y, use
            "#
        ));
    }

    #[test]
    fn test_padded_local_read_before_write() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local x, y, item = 1, 1
            print(item)
            item = {}
            return x, y
            "#
        ));
    }

    #[test]
    fn test_padded_local_read_by_closure_before_write() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local x, y, item = 1, 1
            local function use()
                return item
            end
            item = {}
            return x, y, use
            "#
        ));
    }

    #[test]
    fn test_padded_local_never_written() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local x, y, item = 1, 1
            return x, y, item
            "#
        ));
    }

    #[test]
    fn test_padded_local_read_after_goto_skips_the_write() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local function use(skip_write)
                local head, item = 1
                if skip_write then goto observe end
                item = {}
                ::observe::
                return head, item
            end
            return use
            "#
        ));
    }

    #[test]
    fn test_padded_local_captured_read_after_goto_skips_the_write() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local head, item = 1
            local function use(skip_write)
                if skip_write then goto observe end
                item = {}
                ::observe::
                return item
            end
            return head, use
            "#
        ));
    }

    #[test]
    fn test_padded_local_read_in_loop_condition_before_write() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local head, item = 1
            while item == nil do
                item = {}
            end
            return head, item
            "#
        ));
    }

    #[test]
    fn test_padded_local_written_only_on_repeat_break_paths_is_still_observed() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local function use(first, second)
                local head, item = 1
                repeat
                    if first then
                        item = {value = 1}
                        break
                    elseif second then
                        item = {value = 2}
                        break
                    end
                until true
                return head, item.value
            end
            return use
            "#
        ));
    }

    #[test]
    fn test_padded_local_skipped_by_a_repeat_continue_is_still_observed() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local function use(skip_write)
                local head, item = 1
                repeat
                    if skip_write then
                        continue
                    end
                    item = {value = 1}
                until true
                return head, item.value
            end
            return use
            "#
        ));
    }

    #[test]
    fn test_padded_local_skipped_by_a_repeat_continue_after_an_inner_repeat_is_still_observed() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local function use(skip_write)
                local head, item = 1
                repeat
                    repeat break until true
                    if skip_write then
                        continue
                    end
                    item = {value = 1}
                until true
                return head, item.value
            end
            return use
            "#
        ));
    }

    #[test]
    fn test_padded_local_written_on_every_break_of_an_endless_repeat_is_not_observed() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local function use(first, second)
                local head, item = 1
                repeat
                    if first then
                        item = {value = 1}
                        break
                    elseif second then
                        item = {value = 2}
                        break
                    end
                until false
                return head, item.value
            end
            return use
            "#
        ));
    }

    #[test]
    fn test_padded_local_written_before_the_only_break_of_an_endless_repeat_is_not_observed() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local function use()
                local head, item = 1
                repeat
                    item = {value = 1}
                    break
                until false
                return head, item.value
            end
            return use
            "#
        ));
    }

    #[test]
    fn test_padded_local_written_before_every_repeat_exit_is_not_observed() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UnbalancedAssignments,
            r#"
            local function use(first)
                local head, item = 1
                repeat
                    item = {value = 1}
                    if first then
                        break
                    end
                until true
                return head, item.value
            end
            return use
            "#
        ));
    }
}
