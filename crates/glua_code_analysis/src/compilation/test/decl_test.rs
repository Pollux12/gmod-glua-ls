#[cfg(test)]
mod test {
    use glua_parser::{LuaAstNode, LuaAstToken, LuaFuncStat, LuaVarExpr};
    use googletest::prelude::*;

    use crate::{DiagnosticCode, LuaType, VirtualWorkspace};

    #[test]
    fn test_1() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
        ---@return any ...
        ---@return integer offset
        local function unpack() end
        a, b, c, d = unpack()
        "#,
        );

        assert_eq!(ws.expr_ty("a"), ws.ty("any"));
        assert_eq!(ws.expr_ty("b"), ws.ty("integer"));
        assert_eq!(ws.expr_ty("c"), ws.ty("nil"));
        assert_eq!(ws.expr_ty("d"), ws.ty("nil"));
    }

    #[test]
    fn test_2() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
        ---@return integer offset
        ---@return any ...
        local function unpack() end
        a, b, c, d = unpack()
        "#,
        );

        assert_eq!(ws.expr_ty("a"), ws.ty("integer"));
        assert_eq!(ws.expr_ty("b"), ws.ty("any"));
        assert_eq!(ws.expr_ty("c"), ws.ty("any"));
        assert_eq!(ws.expr_ty("d"), ws.ty("any"));
    }

    #[test]
    fn test_3() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@return any ...
                ---@return integer offset
                local function unpack() end

                ---@param a nil|integer|'l'|'L'
                local function test(a) end
                local len = unpack()
                test(len)
        "#,
        ));
    }

    #[gtest]
    fn forward_declared_function_name_uses_function_type_and_references() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
                local create_initial_simplex4

                function create_initial_simplex4(points, thread_yield)
                    return { points, thread_yield }
                end

                local faces = create_initial_simplex4({}, nil)
            "#,
        );

        let func_stat = ws.get_node::<LuaFuncStat>(file_id);
        let LuaVarExpr::NameExpr(func_name) =
            func_stat.get_func_name().expect("expected function name")
        else {
            panic!("expected plain function name");
        };
        let token = func_name.get_name_token().expect("expected name token");
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let info = semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .expect("expected semantic info for function name");

        assert_that!(info.typ, matches_pattern!(LuaType::Signature(_)));

        let call_name = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("expected syntax tree")
            .get_chunk_node()
            .descendants::<glua_parser::LuaCallExpr>()
            .find_map(|call_expr| match call_expr.get_prefix_expr()? {
                glua_parser::LuaExpr::NameExpr(name_expr)
                    if name_expr.get_name_text().as_deref() == Some("create_initial_simplex4") =>
                {
                    Some(name_expr)
                }
                _ => None,
            })
            .expect("expected call to forward-declared function");
        let call_token = call_name
            .get_name_token()
            .expect("expected call name token");
        let call_info = semantic_model
            .get_semantic_info(call_token.syntax().clone().into())
            .expect("expected semantic info for call name");

        assert_that!(call_info.typ, matches_pattern!(LuaType::Signature(_)));

        let decl_id = ws
            .analysis
            .compilation
            .get_db()
            .get_reference_index()
            .get_local_reference(&file_id)
            .and_then(|refs| refs.get_decl_id(&func_name.get_range()))
            .expect("expected function name to resolve to forward local");
        let references = ws
            .analysis
            .compilation
            .get_db()
            .get_reference_index()
            .get_decl_references(&file_id, &decl_id)
            .expect("expected references for forward local");

        assert_that!(references.cells.len(), ge(2));
    }
}
