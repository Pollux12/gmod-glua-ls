#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, VirtualWorkspace};
    use glua_parser::{LuaAstNode, LuaExpr, LuaNameExpr};

    fn infer_last_name_expr_type(
        ws: &mut VirtualWorkspace,
        code: &str,
        name: &str,
    ) -> crate::LuaType {
        let file_id = ws.def(code);
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let target = semantic_model
            .get_root()
            .descendants::<LuaNameExpr>()
            .filter(|expr| expr.get_name_text().as_deref() == Some(name))
            .collect::<Vec<_>>()
            .pop()
            .expect("Target name expr must exist");

        semantic_model
            .infer_expr(LuaExpr::NameExpr(target))
            .unwrap_or(crate::LuaType::Unknown)
    }

    #[test]
    fn test_custom_binary() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@class AA
        ---@operator pow(number): AA

        ---@type AA
        a = {}
        "#,
        );

        let ty = ws.expr_ty(
            r#"
        a ^ 1
        "#,
        );
        let expected = ws.ty("AA");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_issue_559() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Origin
            ---@operator add(Origin):Origin

            ---@alias AliasType Origin

            ---@type AliasType
            local x1
            ---@type AliasType
            local x2

            A = x1 + x2
        "#,
        );

        let ty = ws.expr_ty("A");
        let expected = ws.ty("Origin");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_issue_867() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local a --- @type { foo? : { bar: { baz: number } } }

            local b = a.foo.bar -- a.foo may be nil (correct)

            c = b.baz -- b may be nil (incorrect)
        "#,
        );

        let ty = ws.expr_ty("c");
        let expected = ws.ty("number");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_isvalid_local_cached_still_narrows() {
        let mut ws = VirtualWorkspace::new();
        let library_root = ws.virtual_url_generator.new_path("__test_library_isvalid");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("isvalid.lua")).unwrap();
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
            ---@class Entity
            ---@field health integer

            ---@param obj any
            ---@return boolean
            function _G.IsValid(obj) end
            "#
                .to_string(),
            ),
        );

        // Cached aliases of the global helper should still narrow.
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field health integer

            ---@return Entity?
            local function get_ent() end

            local IsValid = IsValid

            local ent = get_ent()
            if IsValid(ent) then
                local _health = ent.health
            end
            "#
        ));
    }

    #[test]
    fn test_isvalid_false_branch_not_narrowed_to_nil() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Entity

            ---@return Entity?
            local function get_ent() end

            function IsValid(obj)
                return obj ~= nil
            end

            ---@param value nil
            local function expects_nil(value) end

            local ent = get_ent()
            if not IsValid(ent) then
                expects_nil(ent)
            end
            "#
        ));
    }

    #[test]
    fn test_infer_expr_list_types_tolerates_infer_failures() {
        let mut ws = VirtualWorkspace::new();
        let code = r#"
            local t ---@type { a: number }

            ---@type string, string
            local y, x

            x, y = t.b, 1
        "#;

        assert!(!ws.check_code_for(DiagnosticCode::UndefinedField, code));
        assert!(!ws.check_code_for(DiagnosticCode::AssignTypeMismatch, code));
    }

    #[test]
    fn test_flow_assign_preserves_doc_type_on_infer_error() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            local t ---@type { a: number }
            local x ---@type string
            x = t.b
            R = x
        "#,
        );

        assert_eq!(ws.expr_ty("R"), ws.ty("nil"));
    }

    #[test]
    fn test_isstring_guard_narrows_undefined_global_expr_to_string() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let ty = infer_last_name_expr_type(
            &mut ws,
            r#"
            if isstring(testVar2) then ---@diagnostic disable-line: undefined-global
                print(testVar2) ---@diagnostic disable-line: undefined-global
            end
        "#,
            "testVar2",
        );

        assert_eq!(ty, ws.ty("string"));
    }

    #[test]
    fn test_istable_guard_preserves_annotated_specific_table_type() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@class MyData
            ---@field value integer
        "#,
        );

        let ty = infer_last_name_expr_type(
            &mut ws,
            r#"
            ---@type MyData?
            local data

            if istable(data) then
                print(data)
            end
        "#,
            "data",
        );

        assert_eq!(ty, ws.ty("MyData"));
    }

    #[test]
    fn test_isstring_guard_preserves_annotated_string_subtype() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@class UserId: string
        "#,
        );

        let ty = infer_last_name_expr_type(
            &mut ws,
            r#"
            ---@type UserId?
            local value

            if isstring(value) then
                print(value)
            end
        "#,
            "value",
        );

        assert_eq!(ty, ws.ty("UserId"));
    }

    #[test]
    fn test_istable_guard_does_not_broaden_incompatible_known_type() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let ty = infer_last_name_expr_type(
            &mut ws,
            r#"
            ---@type string
            local value = "x"

            if istable(value) then
                print(value)
            end
        "#,
            "value",
        );

        assert_eq!(ty, ws.ty("string"));
    }
}
