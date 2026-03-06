#[cfg(test)]
mod tests {
    use crate::FormattingOptions;

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
}
