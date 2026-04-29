#[cfg(test)]
mod test {
    use std::{ops::Deref, sync::Arc};

    use glua_parser::{LuaAstNode, LuaAstToken, LuaLocalName};

    use crate::{DiagnosticCode, LuaType, VirtualWorkspace};

    #[test]
    fn test_array_index() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.analysis.get_emmyrc().deref().clone();
        emmyrc.strict.array_index = false;
        ws.analysis.update_config(Arc::new(emmyrc));
        ws.def(
            r#"
            ---@class Test.Add
            ---@field a string

            ---@type int
            index = 1
            ---@type Test.Add[]
            items = {}
        "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
                local a = items[index]
                local b = a.a
        "#,
        ));
    }

    #[test]
    fn test_create_array() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@generic T
            ---@param ... T
            ---@return T[]
            local function new_array(...)
            end

            t = new_array(1, 2, 3, 4, 5)
        "#,
        );

        let t = ws.expr_ty("t");
        let t_expected = ws.ty("integer[]");
        assert_eq!(t, t_expected)
    }

    #[test]
    fn test_array_for_flow() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
        --- @param _x string
        local function foo(_x) end

        local list = {} --- @type string[]

        for i = #list, 1, -1 do
            foo(list[i])
        end
        "#,
        ));
    }

    fn local_name_type(ws: &mut VirtualWorkspace, file_id: crate::FileId, name: &str) -> LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");

        let local_name = semantic_model
            .get_root()
            .descendants::<LuaLocalName>()
            .find(|local_name| {
                local_name
                    .get_name_token()
                    .is_some_and(|token| token.get_name_text() == name)
            })
            .expect("expected local name");
        let token = local_name
            .get_name_token()
            .expect("expected local name token");

        semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .map(|info| info.typ)
            .expect("expected semantic info for local name")
    }

    #[test]
    fn test_array_index_with_whitespace() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.analysis.get_emmyrc().deref().clone();
        emmyrc.strict.array_index = false;
        ws.analysis.update_config(Arc::new(emmyrc));

        let file_id = ws.def(
            r#"
            ---@class Test.SkyPaint
            ---@field a string

            ---@return Test.SkyPaint[]
            local function get_sky_paints() end

            local compact = get_sky_paints()[1]
            local spaced = get_sky_paints()[ 1 ]
            local leading = get_sky_paints()[ 1]
            local trailing = get_sky_paints()[1 ]
        "#,
        );

        let compact_ty = local_name_type(&mut ws, file_id, "compact");
        let spaced_ty = local_name_type(&mut ws, file_id, "spaced");
        let leading_ty = local_name_type(&mut ws, file_id, "leading");
        let trailing_ty = local_name_type(&mut ws, file_id, "trailing");

        let expected = ws.ty("Test.SkyPaint");
        assert_eq!(compact_ty, expected);
        assert_eq!(spaced_ty, expected);
        assert_eq!(leading_ty, expected);
        assert_eq!(trailing_ty, expected);
    }
}
