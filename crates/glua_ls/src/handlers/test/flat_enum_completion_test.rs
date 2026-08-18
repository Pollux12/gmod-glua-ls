#[cfg(test)]
mod tests {
    use googletest::prelude::*;
    use lsp_types::{CompletionItemKind, CompletionTriggerKind};

    use crate::handlers::test_lib::{
        ProviderVirtualWorkspace, VirtualCompletionItem, VirtualInlayHint, check,
    };

    const FLAT_ENUM: &str = r#"
        EF_BONEMERGE = 1
        EF_NODRAW = 32

        ---@enum EF
        ---| EF_BONEMERGE # Performs bone merge on client side
        ---| EF_NODRAW # Don't draw the entity

        ---@param effect EF
        local function add_effects(effect) end
    "#;

    fn expected_members() -> Vec<VirtualCompletionItem> {
        vec![
            VirtualCompletionItem {
                label: "EF_BONEMERGE".to_string(),
                kind: CompletionItemKind::ENUM_MEMBER,
                label_detail: Some(" = 1".to_string()),
            },
            VirtualCompletionItem {
                label: "EF_NODRAW".to_string(),
                kind: CompletionItemKind::ENUM_MEMBER,
                label_detail: Some(" = 32".to_string()),
            },
        ]
    }

    #[gtest]
    fn flat_enum_completes_names_at_the_call_paren() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();

        check!(ws.check_completion_with_kind(
            &format!("{FLAT_ENUM}\nadd_effects(<??>)"),
            expected_members(),
            CompletionTriggerKind::TRIGGER_CHARACTER,
        ));
        Ok(())
    }

    #[gtest]
    fn flat_enum_completes_names_when_invoked() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();

        check!(ws.check_completion(
            &format!("{FLAT_ENUM}\nadd_effects(<??>)"),
            expected_members(),
        ));
        Ok(())
    }

    #[gtest]
    fn flat_enum_hints_the_member_name_for_a_raw_number() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.hint.enum_param_hint = true;
        ws.update_emmyrc(emmyrc);

        check!(ws.check_inlay_hint(
            &format!("{FLAT_ENUM}\nadd_effects(32)"),
            vec![
                VirtualInlayHint {
                    label: ": EF".to_string(),
                    line: 9,
                    pos: 41,
                    ref_file: Some("virtual_0.lua".to_string()),
                },
                VirtualInlayHint {
                    label: "effect:".to_string(),
                    line: 11,
                    pos: 12,
                    ref_file: Some("".to_string()),
                },
                VirtualInlayHint {
                    label: "EF_NODRAW".to_string(),
                    line: 11,
                    pos: 14,
                    ref_file: None,
                },
            ],
        ));
        Ok(())
    }

    /// Mirrors the exact shape the annotation generator emits for a Garry's Mod
    /// enum, so a change to that shape is caught here rather than in the editor.
    #[gtest]
    fn generated_annotation_shape_completes_member_names() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();

        check!(ws.check_completion_with_kind(
            r#"
                ---@realm shared
                ---@source https://wiki.facepunch.com/gmod/Enums/EF
                --- Performs bone merge on client side.
                ---@readonly
                EF_BONEMERGE = 1
                --- Prevents the entity from drawing and networking.
                ---@readonly
                EF_NODRAW = 32

                ---@realm shared
                ---@source https://wiki.facepunch.com/gmod/Enums/EF
                ---@enum EF : number
                ---| EF_BONEMERGE # Performs bone merge on client side.
                ---| EF_NODRAW # Prevents the entity from drawing and networking.

                ---@param effect EF
                local function add_effects(effect) end

                add_effects(<??>)
            "#,
            expected_members(),
            CompletionTriggerKind::TRIGGER_CHARACTER,
        ));
        Ok(())
    }

    #[gtest]
    fn table_enum_still_completes_qualified_members() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();

        check!(ws.check_completion(
            r#"
                ---@enum TEXFILTER
                local TEXFILTER = {
                    NONE = 0,
                    POINT = 1,
                }

                ---@param filter TEXFILTER
                local function set_filter(filter) end

                set_filter(<??>)
            "#,
            vec![
                VirtualCompletionItem {
                    label: "TEXFILTER.NONE".to_string(),
                    kind: CompletionItemKind::ENUM_MEMBER,
                    label_detail: Some(" = 0".to_string()),
                },
                VirtualCompletionItem {
                    label: "TEXFILTER.POINT".to_string(),
                    kind: CompletionItemKind::ENUM_MEMBER,
                    label_detail: Some(" = 1".to_string()),
                },
            ],
        ));
        Ok(())
    }
}
