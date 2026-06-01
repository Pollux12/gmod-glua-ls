#[cfg(test)]
mod tests {
    use crate::handlers::{
        code_actions::code_action,
        test_lib::{ProviderVirtualWorkspace, VirtualCodeAction, check},
    };
    use glua_code_analysis::{DiagnosticCode, Emmyrc};
    use googletest::prelude::*;
    use lsp_types::CodeActionOrCommand;
    use tokio_util::sync::CancellationToken;

    const GMOD_NULL_QUICK_FIX_TITLE: &str = "Use IsValid(...) for GMod NULL check";

    #[gtest]
    fn test_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Cast1
            ---@field get fun(self: self, a: number): Cast1?
        "#,
        );

        check!(ws.check_code_action(
            r#"
                ---@type Cast1
                local A

                local _a = A:get(1):get(2):get(3)
            "#,
            vec![
                VirtualCodeAction {
                    title: "use cast to remove nil".to_string()
                },
                VirtualCodeAction {
                    title: "Disable current line diagnostic (need-check-nil)".to_string()
                },
                VirtualCodeAction {
                    title: "Disable all diagnostics in current file (need-check-nil)".to_string()
                },
                VirtualCodeAction {
                    title:
                        "Disable all diagnostics in current project (need-check-nil)".to_string()
                },
                VirtualCodeAction {
                    title: "use cast to remove nil".to_string()
                },
                VirtualCodeAction {
                    title: "Disable current line diagnostic (need-check-nil)".to_string()
                },
                VirtualCodeAction {
                    title: "Disable all diagnostics in current file (need-check-nil)".to_string()
                },
                VirtualCodeAction {
                    title:
                        "Disable all diagnostics in current project (need-check-nil)".to_string()
                }
            ]
        ));

        Ok(())
    }

    #[gtest]
    fn test_add_doc_tag() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc
            .diagnostics
            .enables
            .push(DiagnosticCode::UnknownDocTag);
        ws.analysis.update_config(emmyrc.into());
        check!(ws.check_code_action(
            r#"
                ---@class Cast1
                ---@foo bar
            "#,
            vec![
                VirtualCodeAction {
                    title: "Add @foo to the list of known tags".to_string()
                },
                VirtualCodeAction {
                    title: "Disable current line diagnostic (unknown-doc-tag)".to_string()
                },
                VirtualCodeAction {
                    title: "Disable all diagnostics in current file (unknown-doc-tag)".to_string()
                },
                VirtualCodeAction {
                    title:
                        "Disable all diagnostics in current project (unknown-doc-tag)".to_string()
                },
            ]
        ));

        Ok(())
    }

    #[gtest]
    fn test_gmod_null_check_code_action_replaces_not_nil_comparison() -> Result<()> {
        let edit = gmod_null_quick_fix_result(
            r#"
                local ent = GetEntityOrNULL()
                if ent ~= nil then
                    ent:GetPos()
                end
            "#,
        )?;

        verify_that!(edit.new_text, eq("IsValid(ent)"))?;
        verify_that!(
            edit.applied_text,
            contains_substring("if IsValid(ent) then")
        )
    }

    #[gtest]
    fn test_gmod_null_check_code_action_replaces_eq_nil_comparison() -> Result<()> {
        let edit = gmod_null_quick_fix_result(
            r#"
                local ent = GetEntityOrNULL()
                if ent == nil then
                    return
                end
            "#,
        )?;

        verify_that!(edit.new_text, eq("not IsValid(ent)"))?;
        verify_that!(
            edit.applied_text,
            contains_substring("if not IsValid(ent) then")
        )
    }

    #[gtest]
    fn test_gmod_null_check_code_action_handles_parenthesized_nil_comparison() -> Result<()> {
        let edit = gmod_null_quick_fix_result(
            r#"
                local ent = GetEntityOrNULL()
                if ent ~= (nil) then
                    ent:GetPos()
                end
            "#,
        )?;

        verify_that!(edit.new_text, eq("IsValid(ent)"))?;
        verify_that!(
            edit.applied_text,
            contains_substring("if IsValid(ent) then")
        )
    }

    #[gtest]
    fn test_gmod_null_check_code_action_wraps_truthy_check() -> Result<()> {
        let edit = gmod_null_quick_fix_result(
            r#"
                local ent = GetEntityOrNULL()
                if ent then
                    ent:GetPos()
                end
            "#,
        )?;

        verify_that!(edit.new_text, eq("IsValid(ent)"))?;
        verify_that!(
            edit.applied_text,
            contains_substring("if IsValid(ent) then")
        )
    }

    struct QuickFixResult {
        new_text: String,
        applied_text: String,
    }

    fn gmod_null_quick_fix_result(code: &str) -> Result<QuickFixResult> {
        let mut ws = gmod_null_workspace();
        let file_id = ws.def(code);
        let diagnostics = check!(
            ws.analysis
                .diagnose_file(file_id, CancellationToken::new())
                .ok_or("failed to diagnose file")
        );
        let actions = check!(
            code_action(&ws.analysis, file_id, diagnostics).ok_or("failed to generate code action")
        );

        let edit = check!(
            actions
                .iter()
                .find_map(gmod_null_quick_fix_text_edit)
                .ok_or("missing GMod NULL quick fix")
        );
        let document = check!(
            ws.analysis
                .compilation
                .get_db()
                .get_vfs()
                .get_document(&file_id)
                .ok_or("missing test document")
        );
        let edit_range = check!(
            document
                .to_rowan_range(edit.range)
                .ok_or("failed to convert edit range")
        );
        let source = document.get_text();
        let applied_text = format!(
            "{}{}{}",
            &source[..u32::from(edit_range.start()) as usize],
            edit.new_text,
            &source[u32::from(edit_range.end()) as usize..]
        );

        Ok(QuickFixResult {
            new_text: edit.new_text,
            applied_text,
        })
    }

    fn gmod_null_quick_fix_text_edit(action: &CodeActionOrCommand) -> Option<lsp_types::TextEdit> {
        match action {
            CodeActionOrCommand::CodeAction(action)
                if action.title == GMOD_NULL_QUICK_FIX_TITLE =>
            {
                action
                    .edit
                    .as_ref()?
                    .changes
                    .as_ref()?
                    .values()
                    .next()?
                    .first()
                    .cloned()
            }
            _ => None,
        }
    }

    fn gmod_null_workspace() -> ProviderVirtualWorkspace {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.analysis.update_config(emmyrc.into());
        ws.def(
            r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): any

            ---@class NULL : Entity
            ---@alias EntityOrNULL Entity|NULL

            ---@return EntityOrNULL
            function GetEntityOrNULL() end
            "#,
        );
        ws
    }
}
