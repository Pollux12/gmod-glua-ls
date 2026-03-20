#[cfg(test)]
mod tests {
    use crate::FormattingOptions;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn formatter_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("emmylua_codestyle_{name}_{nanos}_{counter}"))
    }

    fn normalize_path(path: &PathBuf) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    fn format_with_editorconfig_overrides(code: &str, style_lines: &[&str]) -> String {
        let _guard = formatter_test_lock()
            .lock()
            .expect("formatter test lock poisoned");

        let workspace_dir = unique_test_dir("annotation_spacing");
        fs::create_dir_all(&workspace_dir).expect("failed to create temp workspace");

        let editorconfig_path = workspace_dir.join(".editorconfig");
        let mut editorconfig = String::from("root = true\n\n[*.lua]\n");
        for line in style_lines {
            editorconfig.push_str(line);
            editorconfig.push('\n');
        }
        fs::write(&editorconfig_path, editorconfig).expect("failed to write editorconfig");

        let workspace = normalize_path(&workspace_dir);
        let editorconfig = normalize_path(&editorconfig_path);
        let file_path = normalize_path(&workspace_dir.join("test.lua"));

        crate::update_code_style(&workspace, &editorconfig);
        let result = crate::reformat_code(code, &file_path, FormattingOptions::default());
        crate::remove_code_style(&workspace);

        let _ = fs::remove_file(editorconfig_path);
        let _ = fs::remove_dir_all(workspace_dir);

        result
    }

    fn range_format_with_editorconfig_overrides(
        code: &str,
        style_lines: &[&str],
        start_line: i32,
        start_col: i32,
        end_line: i32,
        end_col: i32,
    ) -> crate::RangeFormatResult {
        let _guard = formatter_test_lock()
            .lock()
            .expect("formatter test lock poisoned");

        let workspace_dir = unique_test_dir("annotation_spacing_range");
        fs::create_dir_all(&workspace_dir).expect("failed to create temp workspace");

        let editorconfig_path = workspace_dir.join(".editorconfig");
        let mut editorconfig = String::from("root = true\n\n[*.lua]\n");
        for line in style_lines {
            editorconfig.push_str(line);
            editorconfig.push('\n');
        }
        fs::write(&editorconfig_path, editorconfig).expect("failed to write editorconfig");

        let workspace = normalize_path(&workspace_dir);
        let editorconfig = normalize_path(&editorconfig_path);
        let file_path = normalize_path(&workspace_dir.join("test.lua"));

        crate::update_code_style(&workspace, &editorconfig);
        let result = crate::range_format_code(
            code,
            &file_path,
            start_line,
            start_col,
            end_line,
            end_col,
            FormattingOptions::default(),
        )
        .expect("range formatter should produce output");
        crate::remove_code_style(&workspace);

        let _ = fs::remove_file(editorconfig_path);
        let _ = fs::remove_dir_all(workspace_dir);

        result
    }

    #[test]
    fn test_format() {
        let code = r#"
        local a = 1
        local b = 2
        print(a+b)
        "#;
        let result = crate::reformat_code(code, "test.lua", FormattingOptions::default());
        let expected = "local a = 1\nlocal b = 2\nprint(a + b)\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn test_range_format() {
        let code = r#"
        local a         = 1
        local b = 2
        print(a+b)
        "#;
        let result =
            crate::range_format_code(code, "test.lua", 1, 1, 1, 1, FormattingOptions::default())
                .unwrap();
        let expected = "local a = 1\n";
        assert_eq!(result.text, expected);
    }

    #[test]
    fn test_check_code_style() {
        let code = r#"
        print(a+b)
        "#;

        let result = crate::check_code_style("test.lua", code);
        println!("{:?}", result);
    }

    #[test]
    fn test_format_options_1() {
        let code = r#"
        local a = 1
        local b = 2
        function f()
            print(a+b)
        end
        "#;
        let options = FormattingOptions {
            indent_size: 2,
            use_tabs: false,
            insert_final_newline: false,
            non_standard_symbol: true,
        };
        let result = crate::reformat_code(code, "test.lua", options);
        let expected = "local a = 1\nlocal b = 2\nfunction f()\n  print(a + b)\nend";
        assert_eq!(result, expected);
    }

    #[test]
    fn test_format_options_2() {
        let code = r#"
        local a = 1
        local b = 2
        function f()
            print(a+b)
        end
        "#;
        let options = FormattingOptions {
            indent_size: 4,
            use_tabs: true,
            insert_final_newline: true,
            non_standard_symbol: false,
        };
        let result = crate::reformat_code(code, "test.lua", options);
        let expected = "local a = 1\nlocal b = 2\nfunction f()\n\tprint(a + b)\nend\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn test_format_options_3() {
        let code = r#"
        local a = 1
        local b = 2
        print(a+b)
        "#;
        let options = FormattingOptions {
            indent_size: 4,
            use_tabs: false,
            insert_final_newline: false,
            non_standard_symbol: true,
        };
        let result = crate::reformat_code(code, "test.lua", options);
        let expected = "local a = 1\nlocal b = 2\nprint(a + b)";
        assert_eq!(result, expected);
    }

    #[test]
    fn test_format_options_4() {
        let code = r#"
        local a = 1
        local b = 2
        a /=123
        /* afafa /*
        "#;
        let options = FormattingOptions {
            indent_size: 4,
            use_tabs: false,
            insert_final_newline: true,
            non_standard_symbol: true,
        };
        let result = crate::reformat_code(code, "test.lua", options);
        let expected = "local a = 1\nlocal b = 2\na /= 123\n/* afafa /*\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn test_format_returns_original_text_when_formatter_fails() {
        let code = "if then\nend\n";
        let result = crate::reformat_code(code, "test.lua", FormattingOptions::default());
        assert_eq!(result, code);
    }

    #[test]
    fn test_annotation_comment_spacing_is_not_collapsed() {
        let code = "--- @class GlideFuelRefundData\n--- @field units number\n\n--- @type table<Entity, table>\nlocal pumpEntities = Glide.FuelPumps\n\n--- @return number\nlocal function getCount()\n    return 1\nend\n";

        let result = format_with_editorconfig_overrides(
            code,
            &[
                "line_space_after_comment = fixed(1)",
                "line_space_after_local_or_assign_statement = fixed(1)",
            ],
        );

        assert!(
            result.contains("--- @field units number\n\n--- @type table<Entity, table>"),
            "expected formatter to preserve a blank line between annotation blocks, got:\n{result}"
        );

        assert!(
            result.contains("local pumpEntities = Glide.FuelPumps\n\n--- @return number"),
            "expected formatter to preserve a blank line before annotation comments, got:\n{result}"
        );
    }

    #[test]
    fn test_non_annotation_comment_spacing_still_follows_rules() {
        let code = "-- regular note\n\nlocal value = 1\n";

        let result = format_with_editorconfig_overrides(
            code,
            &[
                "line_space_after_comment = fixed(1)",
                "line_space_after_local_or_assign_statement = fixed(1)",
            ],
        );

        assert_eq!(result, "-- regular note\nlocal value = 1\n");
    }

    #[test]
    fn test_annotation_gap_preserved_for_class_then_type_sequence() {
        let code = "--- Refund data passed to Glide.FuelSessionRefund hook\n--- @class GlideFuelRefundData\n--- @field units number Number of fuel units not delivered\n--- @field cost number? The monetary value to refund (may be nil/0 if no cost was set)\n--- @field reason number Why the session ended (REFUEL_STOP_REASON constant)\n--- @field pumpType string|number The pump type used for the session\n\n--- @type table<Entity, table>\nGlide.FuelPumps = Glide.FuelPumps or {}\n---@type table<number, table>\nGlide.FuelPumpData = Glide.FuelPumpData or {}\n---@type table<Player, table>\nGlide.FuelSessions = Glide.FuelSessions or {}\n---@type table<Entity, table>\nGlide.FuelVehicleSessions = Glide.FuelVehicleSessions or {}\n---@type table<Entity, table>\nGlide.FuelNozzleSessions = Glide.FuelNozzleSessions or {}\n---@type table<Entity, table>\nGlide.FuelPumpSessions = Glide.FuelPumpSessions or {}\n---@type table<Player, table>\nGlide.FuelPlayerStates = Glide.FuelPlayerStates or {}\n";

        let result = format_with_editorconfig_overrides(
            code,
            &[
                "line_space_after_comment = fixed(1)",
                "line_space_after_local_or_assign_statement = fixed(1)",
            ],
        );

        assert!(
            result.contains(
                "--- @field pumpType string|number The pump type used for the session\n\n--- @type table<Entity, table>"
            ),
            "expected formatter to preserve class/type annotation gap, got:\n{result}"
        );
    }

    #[test]
    fn test_range_format_keeps_annotation_gap_for_class_then_type_sequence() {
        let code = "--- Refund data passed to Glide.FuelSessionRefund hook\n--- @class GlideFuelRefundData\n--- @field units number Number of fuel units not delivered\n--- @field cost number? The monetary value to refund (may be nil/0 if no cost was set)\n--- @field reason number Why the session ended (REFUEL_STOP_REASON constant)\n--- @field pumpType string|number The pump type used for the session\n\n--- @type table<Entity, table>\nGlide.FuelPumps = Glide.FuelPumps or {}\n---@type table<number, table>\nGlide.FuelPumpData = Glide.FuelPumpData or {}\n---@type table<Player, table>\nGlide.FuelSessions = Glide.FuelSessions or {}\n---@type table<Entity, table>\nGlide.FuelVehicleSessions = Glide.FuelVehicleSessions or {}\n---@type table<Entity, table>\nGlide.FuelNozzleSessions = Glide.FuelNozzleSessions or {}\n---@type table<Entity, table>\nGlide.FuelPumpSessions = Glide.FuelPumpSessions or {}\n---@type table<Player, table>\nGlide.FuelPlayerStates = Glide.FuelPlayerStates or {}\n";

        let result = range_format_with_editorconfig_overrides(
            code,
            &[
                "line_space_after_comment = fixed(1)",
                "line_space_after_local_or_assign_statement = fixed(1)",
            ],
            1,
            1,
            40,
            1,
        );

        assert!(
            result
                .text
                .contains("--- @field pumpType string|number The pump type used for the session\n\n--- @type table<Entity, table>"),
            "expected range formatter to preserve class/type annotation gap, got:\n{}",
            result.text
        );
    }

    #[test]
    fn test_range_format_preserves_leading_blank_line_in_selected_range() {
        let code = "--- @field pumpType string|number\n\n--- @type table<Entity, table>\nGlide.FuelPumps = Glide.FuelPumps or {}\n";

        let result = range_format_with_editorconfig_overrides(
            code,
            &[
                "line_space_after_comment = fixed(1)",
                "line_space_after_local_or_assign_statement = fixed(1)",
            ],
            1,
            0,
            3,
            0,
        );

        assert!(
            result.text.starts_with("\n--- @type table<Entity, table>"),
            "expected selected blank line to be preserved at range start, got:\n{}",
            result.text
        );
    }
}
