#[cfg(test)]
mod tests {
    use crate::handlers::{
        auto_insert_end::build_auto_insert_end_response,
        test_lib::ProviderVirtualWorkspace,
    };
    use googletest::prelude::*;

    fn check_auto_insert_end(block_str: &str, should_insert: bool, close_keyword: &str) -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let (content, position) = ProviderVirtualWorkspace::handle_file_content(block_str)?;
        let file_id = ws.def(&content);
        let result = build_auto_insert_end_response(&ws.analysis, file_id, position)
            .ok_or("failed to compute auto insert end response")
            .or_fail()?;

        verify_that!(result.should_insert, eq(should_insert))?;
        if should_insert {
            verify_that!(result.close_keyword.as_str(), eq(close_keyword))?;
            verify_that!(result.reason.is_none(), eq(true))?;
        }
        Ok(())
    }

    #[gtest]
    fn inserts_end_for_if() -> Result<()> {
        check_auto_insert_end(
            r#"
                if foo then<??>
            "#,
            true,
            "end",
        )
    }

    #[gtest]
    fn does_not_insert_when_end_already_exists() -> Result<()> {
        check_auto_insert_end(
            r#"
                if foo then<??>
                end
            "#,
            false,
            "",
        )
    }

    #[gtest]
    fn inserts_until_for_repeat() -> Result<()> {
        check_auto_insert_end(
            r#"
                repeat<??>
            "#,
            true,
            "until",
        )
    }

    #[gtest]
    fn inserts_end_for_while() -> Result<()> {
        check_auto_insert_end(
            r#"
                while foo do<??>
            "#,
            true,
            "end",
        )
    }

    #[gtest]
    fn inserts_end_for_for() -> Result<()> {
        check_auto_insert_end(
            r#"
                for i = 1, 10 do<??>
            "#,
            true,
            "end",
        )
    }

    #[gtest]
    fn inserts_end_for_do_block() -> Result<()> {
        check_auto_insert_end(
            r#"
                do<??>
            "#,
            true,
            "end",
        )
    }

    #[gtest]
    fn inserts_end_for_function() -> Result<()> {
        check_auto_insert_end(
            r#"
                local function hello()<??>
            "#,
            true,
            "end",
        )
    }

    #[gtest]
    fn rejects_incomplete_header() -> Result<()> {
        check_auto_insert_end(
            r#"
                if foo<??>
            "#,
            false,
            "",
        )
    }

    #[gtest]
    fn rejects_text_after_cursor() -> Result<()> {
        check_auto_insert_end(
            r#"
                if foo then x<??>
            "#,
            false,
            "",
        )
    }

    #[gtest]
    fn inserts_end_for_else_clause() -> Result<()> {
        check_auto_insert_end(
            r#"
                if foo then
                else<??>
            "#,
            true,
            "end",
        )
    }

    #[gtest]
    fn inserts_end_for_elseif_clause() -> Result<()> {
        check_auto_insert_end(
            r#"
                if foo then
                elseif bar then<??>
            "#,
            true,
            "end",
        )
    }
}
