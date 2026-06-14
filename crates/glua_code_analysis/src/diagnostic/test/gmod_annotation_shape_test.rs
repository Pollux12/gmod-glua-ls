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

    #[gtest]
    fn dtree_node_get_root_returns_dtree_for_right_click_hook() {
        let mut ws = VirtualWorkspace::new();

        const DTREE_STUBS: &str = r#"
            ---@class Panel

            ---@class DTree: Panel
            DTree = {}

            ---@param node DTree_Node
            ---@return boolean
            function DTree:DoRightClick(node) end

            ---@class DTree_Node: Panel
            DTree_Node = {}

            ---@return DTree
            function DTree_Node:GetRoot() end

            ---@type DTree_Node
            local node
        "#;

        let valid = format!("{DTREE_STUBS}\nnode:GetRoot():DoRightClick(node)");

        expect_that!(
            ws.check_code_for(DiagnosticCode::RedundantParameter, &valid),
            eq(true)
        );
        expect_that!(
            ws.check_code_for(DiagnosticCode::UndefinedField, &valid),
            eq(true)
        );

        let extra_arg = format!("{DTREE_STUBS}\nnode:GetRoot():DoRightClick(node, true)");
        expect_that!(
            ws.check_code_for(DiagnosticCode::RedundantParameter, &extra_arg),
            eq(false)
        );

        let typo = format!("{DTREE_STUBS}\nnode:GetRoot():DoRightCick(node)");
        expect_that!(
            ws.check_code_for(DiagnosticCode::UndefinedField, &typo),
            eq(false)
        );
    }

    #[gtest]
    fn load_presets_string_keyed_table_allows_guarded_nested_assignment() {
        let mut ws = VirtualWorkspace::new();

        let code = r#"
            ---@class GmodPresets: table<string, table>

            ---@return GmodPresets
            function LoadPresets() end

            local P = LoadPresets()
            P["a"] = P["a"] or {}
            P["a"]["b"] = 1
        "#;

        expect_that!(
            ws.check_code_for(DiagnosticCode::NeedCheckNil, code),
            eq(true)
        );
        expect_that!(
            ws.check_code_for(DiagnosticCode::UncheckedNilAccess, code),
            eq(true)
        );
    }

    #[gtest]
    fn unguarded_table_index_field_access_still_warns() {
        let mut ws = VirtualWorkspace::new();

        let code = r#"
            ---@type table
            local T = {}
            return T.someKey.Joinable
        "#;

        expect_that!(
            ws.check_code_for(DiagnosticCode::UncheckedNilAccess, code),
            eq(false)
        );
    }
}
