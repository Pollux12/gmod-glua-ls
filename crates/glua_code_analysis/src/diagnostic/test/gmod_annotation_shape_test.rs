#[cfg(test)]
mod test {
    use googletest::prelude::*;

    use crate::{DiagnosticCode, VirtualWorkspace};

    #[gtest]
    fn string_comma_accepts_number_or_string_and_rejects_table() {
        let mut ws = VirtualWorkspace::new();

        let code = r#"
            string = {}

            ---@param value number|string
            ---@param separator? string
            ---@return string
            function string.Comma(value, separator) end

            string.Comma(12345)
            string.Comma("12345")
        "#;

        expect_that!(
            ws.check_code_for(DiagnosticCode::ParamTypeMismatch, code),
            eq(true)
        );

        expect_that!(
            ws.check_code_for(
                DiagnosticCode::ParamTypeMismatch,
                r#"
                string = {}

                ---@param value number|string
                ---@param separator? string
                ---@return string
                function string.Comma(value, separator) end

                string.Comma({})
            "#,
            ),
            eq(false)
        );
    }

    #[gtest]
    fn util_get_sun_info_optional_return_narrows_after_nil_guard() {
        let mut ws = VirtualWorkspace::new();

        let code = r#"
            util = {}

            ---@class Vector

            ---@class SunInfo
            ---@field direction Vector
            ---@field obstruction number

            ---@return SunInfo?
            function util.GetSunInfo() end

            local sun = util.GetSunInfo()
            if not sun then return end

            local direction = sun.direction
            local obstruction = sun.obstruction
        "#;

        expect_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(true)
        );
        expect_that!(
            ws.check_code_for(DiagnosticCode::UndefinedField, code),
            eq(true)
        );

        expect_that!(
            ws.check_code_for(
                DiagnosticCode::UndefinedField,
                r#"
                util = {}

                ---@class Vector

                ---@class SunInfo
                ---@field direction Vector
                ---@field obstruction number

                ---@return SunInfo?
                function util.GetSunInfo() end

                local sun = util.GetSunInfo()
                if not sun then return end

                local typo = sun.obstructon
            "#,
            ),
            eq(false)
        );
    }
}
