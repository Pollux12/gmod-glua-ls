#[cfg(test)]
mod test {
    use crate::VirtualWorkspace;

    #[test]
    fn a_vararg_before_the_last_value_gives_one_value_and_keeps_the_rest() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def(
            r#"
        ---@param ... integer
        local function h(...)
            return ..., true
        end
        "#,
        );
        let model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let return_stat =
            glua_parser::LuaAstNode::descendants::<glua_parser::LuaReturnStat>(model.get_root())
                .next()
                .expect("return statement");
        let exprs = return_stat.get_expr_list().collect::<Vec<_>>();

        let values = |var_count| {
            model
                .infer_expr_list_types(&exprs, var_count)
                .into_iter()
                .map(|(typ, _)| typ)
                .collect::<Vec<_>>()
        };
        let expected = vec![crate::LuaType::Integer, crate::LuaType::BooleanConst(true)];
        assert_eq!(values(None), expected);
        assert_eq!(values(Some(3)), expected);
    }

    #[test]
    fn a_return_after_a_vararg_is_checked_against_its_slot() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(!ws.check_code_for(
            crate::DiagnosticCode::ReturnTypeMismatch,
            r#"
        ---@param ... integer
        ---@return integer, string
        local function h(...)
            return ..., true
        end
        "#,
        ));
    }

    #[test]
    fn test_pcall_return() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        a, b = pcall(string.rep, "a", 1000000000)
        "#,
        );

        let ty = ws.expr_ty("b");
        let expected = ws.ty("string");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_unpack_return() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        a, b, c = table.unpack({1, 2, 3})
        "#,
        );

        let a_ty = ws.expr_ty("a");
        let b_ty = ws.expr_ty("b");
        let c_ty = ws.expr_ty("c");
        let a_expected = ws.expr_ty("1");
        let b_expected = ws.expr_ty("2");
        let c_expected = ws.expr_ty("3");
        assert_eq!(a_ty, a_expected);
        assert_eq!(b_ty, b_expected);
        assert_eq!(c_ty, c_expected);
    }

    #[test]
    fn test_assert_return() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        ---@return string?
        ---@return string?
        function cwd() end

        a, b = assert(cwd())
        "#,
        );

        let a_ty = ws.expr_ty("a");
        let b_ty = ws.expr_ty("b");
        let a_expected = ws.ty("string");
        let b_expected = ws.ty("string?");
        assert_eq!(a_ty, a_expected);
        assert_eq!(b_ty, b_expected);
    }

    #[test]
    fn test_issue_237() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            crate::DiagnosticCode::UnbalancedAssignments,
            r#"
        local fmt = ""
        local scol, ecol, match, key, time_fmt = fmt:find('(<([^:>]+):?([^>]*)>)')
        "#,
        ));
    }

    #[test]
    fn test_issue_244() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
        ---@return string
        local function foo()
            return "ok"
        end
        ok, err = pcall(foo)
        "#,
        );

        let err_ty = ws.expr_ty("err");
        let expected = ws.ty("string");
        assert_eq!(err_ty, expected);
    }

    #[test]
    fn test_issue_342() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
        function bar()
            local a, b = foo()
            e = b
        end

        --- @return string, integer
        function foo() end
        "#,
        );

        let e_ty = ws.expr_ty("e");
        let expected = ws.ty("integer");
        assert_eq!(e_ty, expected);
    }
}
