#[cfg(test)]
mod test {
    use glua_parser::{LuaAstNode, LuaClosureExpr, LuaNameExpr};
    use tokio_util::sync::CancellationToken;

    use crate::{DiagnosticCode, LuaSignatureId, LuaType, VirtualWorkspace};

    fn local_name_type(
        ws: &VirtualWorkspace,
        file_id: crate::FileId,
        name: &str,
    ) -> crate::LuaType {
        let model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let name_expr = model
            .get_root()
            .descendants::<LuaNameExpr>()
            .find(|expr| expr.get_name_text().as_deref() == Some(name))
            .expect("local name");
        model
            .get_semantic_info(name_expr.syntax().clone().into())
            .expect("semantic info")
            .display_typ()
            .clone()
    }

    #[test]
    fn test_flow() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        --- @return string[] stdout
        --- @return string? stderr
        local function foo() end

        --- @param _a string[]
        local function bar(_a) end

        local a = {}

        a = foo()

        b = a
        "#,
        );
        let ty = ws.expr_ty("b");
        let expected = ws.ty("string[]");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_issue_265() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
        local function bar()
            return ''
        end

        --- @return integer
        function foo()
            return bar() --[[@as integer]]
        end

        "#,
        ));
    }

    #[test]
    fn test_issue_464() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
                ---@class D31
                ---@field func? fun(a:number, b:string):number

                ---@type D31
                local f = {
                    func = function(a, b)
                        return "a"
                    end,
                }
        "#,
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
                ---@class D31
                ---@field func? fun(a:number, b:string):number

                ---@type D31
                local f = {
                    func = function(a, b)
                        return a
                    end,
                }
        "#,
        ));
    }

    #[test]
    fn unresolved_multi_return_is_stable_after_consumer_edit() {
        let mut ws = VirtualWorkspace::new();
        let consumer = r#"
            local ok, instance = API.Compile()
            observed = instance
        "#;

        let file_ids = ws.def_files(vec![
            (
                "lua/autorun/api.lua",
                r#"
                    API = {}

                    function API.Compile()
                        return API.Compile()
                    end
                "#,
            ),
            ("lua/autorun/consumer.lua", consumer),
        ]);
        let (consumer_file_id, consumer_uri) = file_ids
            .into_iter()
            .find_map(|file_id| {
                let db = ws.analysis.compilation.get_db();
                db.get_vfs()
                    .get_file_path(&file_id)
                    .is_some_and(|path| path.ends_with("lua/autorun/consumer.lua"))
                    .then(|| db.get_vfs().get_uri(&file_id).map(|uri| (file_id, uri)))
                    .flatten()
            })
            .expect("consumer URI");

        let initial = local_name_type(&ws, consumer_file_id, "instance");
        assert_eq!(initial, ws.ty("unknown"));
        ws.analysis
            .update_file_by_uri(&consumer_uri, Some(format!("{consumer}\n")))
            .expect("edited consumer");
        let incremental = local_name_type(&ws, consumer_file_id, "instance");

        assert_eq!(initial, incremental);
    }

    #[test]
    fn deferred_multi_return_correlation_is_stable_after_consumer_edit() {
        let mut ws = VirtualWorkspace::new();
        let consumer = r#"
            local function run()
                local ok, instance = API.Compile()
                if not ok then return end
                observed = instance.value
            end
        "#;
        let file_ids = ws.def_files(vec![
            (
                "lua/autorun/api.lua",
                r#"
                    API = {}

                    function API.Compile()
                        if maybe then return false, API.Failure() end
                        return true, { value = "ok" }
                    end

                    function API.Failure()
                        return nil
                    end
                "#,
            ),
            ("lua/autorun/consumer.lua", consumer),
        ]);
        let (consumer_file_id, consumer_uri) = file_ids
            .into_iter()
            .find_map(|file_id| {
                let db = ws.analysis.compilation.get_db();
                if !db
                    .get_vfs()
                    .get_file_path(&file_id)
                    .is_some_and(|path| path.ends_with("lua/autorun/consumer.lua"))
                {
                    return None;
                }
                Some((file_id, db.get_vfs().get_uri(&file_id)?))
            })
            .expect("consumer file");
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);

        let before = ws
            .analysis
            .diagnose_file(consumer_file_id, CancellationToken::new())
            .unwrap_or_default();
        ws.analysis
            .update_file_by_uri(&consumer_uri, Some(format!("{consumer}\n")))
            .expect("edited consumer");
        let after = ws
            .analysis
            .diagnose_file(consumer_file_id, CancellationToken::new())
            .unwrap_or_default();

        assert!(before.is_empty(), "unexpected diagnostics: {before:?}");
        assert_eq!(before, after);
    }

    /// An `any`/`unknown` return on the expected callback type says nothing
    /// about what this callback returns. Taking it cleared the body-derived
    /// return and stamped `DocResolve` over the result, and every later repair
    /// pass refuses to correct a documented return.
    #[test]
    fn uninformative_callback_return_keeps_body_inference() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@param cb fun(): any
            local function register(cb) end

            register(function()
                _side_effect = 1
            end)
            "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let closure = semantic_model
            .get_root()
            .descendants::<LuaClosureExpr>()
            .last()
            .expect("callback closure");
        let signature = semantic_model
            .get_db()
            .get_signature_index()
            .get(&LuaSignatureId::from_closure(file_id, &closure))
            .expect("callback signature");

        assert_eq!(signature.get_return_type(), LuaType::Nil);
    }
}
