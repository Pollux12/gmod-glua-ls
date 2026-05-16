#[cfg(test)]
mod test {
    use glua_parser::{LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaIndexExpr};

    use crate::{
        DiagnosticCode, FileId, LuaDocDefaultValue, LuaMemberOwner, LuaSemanticDeclId, LuaType,
        LuaTypeDeclId, VirtualWorkspace,
    };

    fn index_expr_ty(ws: &VirtualWorkspace, file_id: FileId, expr_text: &str) -> LuaType {
        index_expr_ty_at_occurrence(ws, file_id, expr_text, 0)
    }

    fn index_expr_ty_at_occurrence(
        ws: &VirtualWorkspace,
        file_id: FileId,
        expr_text: &str,
        occurrence: usize,
    ) -> LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");

        let mut seen = Vec::new();
        let mut seen_occurrence = 0usize;
        for index_expr in semantic_model.get_root().descendants::<LuaIndexExpr>() {
            let text = index_expr.syntax().text().to_string();
            seen.push(text.clone());
            if text == expr_text
                && seen_occurrence == occurrence
                && let Some(info) =
                    semantic_model.get_semantic_info(index_expr.syntax().clone().into())
            {
                return info.typ;
            } else if text == expr_text {
                seen_occurrence += 1;
            }
        }

        panic!("expected semantic info for `{expr_text}` at occurrence {occurrence}, saw {seen:?}");
    }

    fn type_decl_id_from_type(typ: &LuaType) -> Option<LuaTypeDeclId> {
        match typ {
            LuaType::Def(type_decl_id) | LuaType::Ref(type_decl_id) => Some(type_decl_id.clone()),
            LuaType::Instance(instance) => type_decl_id_from_type(instance.get_base()),
            LuaType::TypeGuard(inner) => type_decl_id_from_type(inner),
            _ => None,
        }
    }

    #[test]
    fn test_inline_default_metadata_storage() {
        let mut ws = VirtualWorkspace::new();

        let file_id = ws.def(
            r#"
        ---@class Example
        ---@field ContentsLeft=0 CONTENTS
        ---@field ContentsRight CONTENTS=0
        ---@field EntityDefault Entity="NULL"
        local Example = {}

        ---@param retries_left=3 number
        ---@param retries_right number=3
        ---@return boolean=false
        function Example:Run(retries_left, retries_right)
        end
        "#,
        );

        let class_type = ws.ty("Example");
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let db = semantic_model.get_db();

        let closure = semantic_model
            .get_root()
            .descendants::<LuaClosureExpr>()
            .next()
            .expect("expected closure");
        let signature_id = crate::LuaSignatureId::from_closure(file_id, &closure);
        let signature = db
            .get_signature_index()
            .get(&signature_id)
            .expect("expected function signature");

        let left_idx = signature
            .find_param_idx("retries_left")
            .expect("missing retries_left param");
        let right_idx = signature
            .find_param_idx("retries_right")
            .expect("missing retries_right param");
        let left_default = signature
            .param_docs
            .get(&left_idx)
            .and_then(|doc| doc.default_value.clone());
        let right_default = signature
            .param_docs
            .get(&right_idx)
            .and_then(|doc| doc.default_value.clone());
        assert_eq!(
            left_default,
            Some(LuaDocDefaultValue::Number("3".to_string()))
        );
        assert_eq!(
            right_default,
            Some(LuaDocDefaultValue::Number("3".to_string()))
        );

        let return_default = signature
            .return_docs
            .first()
            .and_then(|doc| doc.default_value.clone());
        assert_eq!(return_default, Some(LuaDocDefaultValue::Boolean(false)));

        let class_decl_id =
            type_decl_id_from_type(&class_type).expect("expected class type declaration id");
        let members = db
            .get_member_index()
            .get_members(&LuaMemberOwner::Type(class_decl_id))
            .expect("expected class members");

        for key_name in ["ContentsLeft", "ContentsRight"] {
            let member = members
                .iter()
                .filter_map(|item| db.get_member_index().get_member(&item.get_id()))
                .find(|member| member.get_key().get_name() == Some(key_name))
                .expect("expected field member");
            let property = db
                .get_property_index()
                .get_property(&LuaSemanticDeclId::Member(member.get_id()))
                .expect("expected field property");
            assert_eq!(
                property.default_value(),
                Some(&LuaDocDefaultValue::Number("0".to_string()))
            );
        }

        let entity_member = members
            .iter()
            .filter_map(|item| db.get_member_index().get_member(&item.get_id()))
            .find(|member| member.get_key().get_name() == Some("EntityDefault"))
            .expect("expected EntityDefault member");
        let entity_property = db
            .get_property_index()
            .get_property(&LuaSemanticDeclId::Member(entity_member.get_id()))
            .expect("expected EntityDefault property");
        assert_eq!(
            entity_property.default_value(),
            Some(&LuaDocDefaultValue::String("NULL".to_string()))
        );
    }

    #[test]
    fn test_inline_default_metadata_storage_for_local_function() {
        let mut ws = VirtualWorkspace::new();

        let file_id = ws.def(
            r#"
        ---@param retries number=3
        ---@return boolean
        local function run(retries)
        end
        "#,
        );
        let expected_number = ws.ty("number");
        let expected_boolean = ws.ty("boolean");

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let db = semantic_model.get_db();

        let closure = semantic_model
            .get_root()
            .descendants::<LuaClosureExpr>()
            .next()
            .expect("expected closure");
        let signature_id = crate::LuaSignatureId::from_closure(file_id, &closure);
        let signature = db
            .get_signature_index()
            .get(&signature_id)
            .expect("expected function signature");

        let retries_idx = signature
            .find_param_idx("retries")
            .expect("missing retries param");
        let retries_default = signature
            .param_docs
            .get(&retries_idx)
            .and_then(|doc| doc.default_value.clone());
        let retries_type = signature
            .param_docs
            .get(&retries_idx)
            .map(|doc| doc.type_ref.clone());
        assert_eq!(
            retries_default,
            Some(LuaDocDefaultValue::Number("3".to_string()))
        );
        assert_eq!(retries_type, Some(expected_number));
        assert_eq!(
            signature
                .return_docs
                .first()
                .map(|doc| doc.type_ref.clone()),
            Some(expected_boolean)
        );
    }

    #[test]
    fn test_local_function_call_infers_defaulted_param_and_return_metadata() {
        let mut ws = VirtualWorkspace::new();

        let file_id = ws.def(
            r#"
        ---@param retries number=3
        ---@return boolean
        local function run(retries)
        end

        local result = run()
        "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let call_expr = semantic_model
            .get_root()
            .descendants::<LuaCallExpr>()
            .next()
            .expect("expected call");
        let func = semantic_model
            .infer_call_expr_func(call_expr, None)
            .expect("expected callable");

        assert!(func.is_param_optional(0));
        assert_eq!(func.get_ret(), &ws.ty("boolean"));
    }

    #[test]
    fn test_issue_223() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
        --- @return integer
        function foo()
            local a
            return a --[[@as integer]]
        end
        "#,
        );
    }

    // workaround for table
    #[test]
    fn test_issue_234() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        GG = {} --- @type table

        GG.f = {}

        function GG.fun() end

        function GG.f.fun() end
        "#,
        );

        let ty = ws.expr_ty("GG.fun");
        assert_eq!(
            format!("{:?}", ty),
            "Signature(LuaSignatureId { file_id: FileId { id: 20 }, position: 76 })"
        );
    }

    #[test]
    fn test_issue_493() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        local async = {}
        --- @async
        --- @generic T, R
        --- @param argc integer
        --- @param func fun(...:T..., cb: fun(...:R...))
        --- @param ... T...
        --- @return R...
        function async.await(argc, func, ...)
            error('not implemented')
        end

        --- @param func async fun()
        function async.run(func)
            error('not implemented')
        end

        --- @alias FsStat {path: string, type:string, size:integer}

        --- @param path string
        --- @param callback fun(stat: FsStat)
        local function fs_stat(path, callback)
            error('not implemented')
        end

        async.run(function ()
            stat = async.await(2, fs_stat, 'a.lua')
        end)

        "#,
        );

        let ty = ws.expr_ty("stat");
        let expected = ws.ty("FsStat");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_issue_497() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        --- @generic T, R
        --- @param argc integer
        --- @param func fun(...:T..., cb: fun(...:R...))
        --- @return async fun(...:T...):R...
        local function wrap(argc, func) end

        --- @param a string
        --- @param b string
        --- @param callback fun(out: string)
        local function system(a, b, callback) end

        local wrapped = wrap(3, system)
        -- type is 'async fun(a: string, b: string): unknown'

        d = wrapped("a", "b")
        "#,
        );

        let ty = ws.expr_ty("d");
        let expected = ws.ty("string");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_generic_type_inference() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::TypeNotFound,
            r#"
            ---@class AnonymousObserver<T>: Observer<T>
        "#,
        ));
    }

    #[test]
    fn test_generic_type_extends() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@generic T
            ---@[constructor("__init")]
            ---@param name `T`
            ---@return T
            function meta(name)
            end
        "#,
        );
        ws.def(
            r#"
            ---@class State
            ---@field a string

            ---@class StateMachine<T: State>
            ---@field aaa T
            ---@field new fun(self: self): self
            StateMachine = meta("StateMachine")

            ---@return self
            function StateMachine:abc()
            end


            ---@return self
            function StateMachine:__init()
            end
            "#,
        );
        {
            ws.def(
                r#"
            A = StateMachine:new()
            "#,
            );
            let ty = ws.expr_ty("A");
            let expected = ws.ty("StateMachine<State>");
            assert_eq!(ty, expected);
        }
        {
            ws.def(
                r#"
            B = StateMachine:abc()
            "#,
            );
            let ty = ws.expr_ty("B");
            let expected = ws.ty("StateMachine<State>");
            assert_eq!(ty, expected);
        }
        {
            ws.def(
                r#"
            C = StateMachine:abc()
            "#,
            );
            let ty = ws.expr_ty("C");
            let expected = ws.ty("StateMachine<State>");
            assert_eq!(ty, expected);
        }
    }

    #[test]
    fn test_merge_right_mapped_type() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@generic T: table, U: table
        ---@param a T
        ---@param b U
        ---@return Merge<T, U>
        local function extend(a, b) end

        ---@type { x: number, y: number }
        local a = { x = 1, y = 2 }

        ---@type { y: string, z: boolean }
        local b = { y = "hello", z = true }

        c = extend(a, b)
        "#,
        );

        let ty = ws.expr_ty("c.y");
        let expected = ws.ty("string");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_extend_with_policy_overwrite() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@overload fun<T: table, U: table>(a: T, b: U, policy: "never"): T & U
        ---@overload fun<T: table, U: table>(a: T, b: U, policy: "right"): Merge<T, U>
        ---@overload fun<T: table, U: table>(a: T, b: U, policy: "left"): Merge<U, T>
        ---@generic T: table, U: table
        ---@param a T
        ---@param b U
        ---@param policy "never" | "left" | "right"
        ---@return (T & U) | Merge<T, U> | Merge<U, T>
        local function extend(a, b, policy) end

        ---@type { x: number, y: number }
        local a = { x = 1, y = 2 }

        ---@type { y: string, z: boolean }
        local b = { y = "hello", z = true }

        c_never = extend(a, b, "never")
        c_right = extend(a, b, "right")
        c_left = extend(a, b, "left")
        "#,
        );

        let never_ty = ws.expr_ty("c_never.y");
        let never_expected = ws.ty("never");
        assert_eq!(never_ty, never_expected);

        let right_ty = ws.expr_ty("c_right.y");
        let right_expected = ws.ty("string");
        assert_eq!(right_ty, right_expected);

        let left_ty = ws.expr_ty("c_left.y");
        let left_expected = ws.ty("number");
        assert_eq!(left_ty, left_expected);
    }

    #[test]
    fn test_intersection_conflict_yields_never() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@return { y: number } & { y: string }
        local function foo() end

        c = foo()
        "#,
        );

        let ty = ws.expr_ty("c.y");
        let expected = ws.ty("never");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_type_return_usage() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::AnnotationUsageError,
            r#"
            ---@type string
            return ""
        "#,
        ));
    }

    #[test]
    fn test_outparam_updates_literal_output_field_and_referenced_local() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "trace.lua",
            r#"
                ---@class TraceResult
                ---@field Hit boolean
                local TraceResult = {}

                util = {}

                ---@outparam traceConfig.output TraceResult
                ---@param traceConfig table
                ---@return TraceResult
                function util.TraceLine(traceConfig) end
                "#,
        );
        let file_id = ws.def_file(
            "test.lua",
            r#"
                local ray = {}
                local traceData = {
                    output = ray,
                }

                util.TraceLine(traceData)

                local hitFromConfig = traceData.output.Hit
                local hitFromRay = ray.Hit
                "#,
        );
        let trace_result_ty = ws.ty("TraceResult");
        assert_eq!(
            index_expr_ty(&ws, file_id, "traceData.output"),
            trace_result_ty
        );
        assert_eq!(index_expr_ty(&ws, file_id, "ray.Hit"), ws.ty("boolean"));
    }

    #[test]
    fn test_outparam_updates_assigned_output_field() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "trace.lua",
            r#"
                ---@class TraceResult
                ---@field Hit boolean
                local TraceResult = {}

                util = {}

                ---@outparam traceConfig.output TraceResult
                ---@param traceConfig table
                ---@return TraceResult
                function util.TraceLine(traceConfig) end
                "#,
        );
        let file_id = ws.def_file(
            "test.lua",
            r#"
                local ray = {}
                local traceData = {}
                traceData.output = ray

                util.TraceLine(traceData)

                local hitFromConfig = traceData.output.Hit
                local hitFromRay = ray.Hit
                "#,
        );
        let trace_result_ty = ws.ty("TraceResult");

        assert_eq!(
            index_expr_ty_at_occurrence(&ws, file_id, "traceData.output", 1),
            trace_result_ty
        );
        assert_eq!(index_expr_ty(&ws, file_id, "ray.Hit"), ws.ty("boolean"));
    }

    #[test]
    fn test_outparam_updates_nested_output_field() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "trace.lua",
            r#"
                ---@class TraceResult
                ---@field Hit boolean
                local TraceResult = {}

                util = {}

                ---@outparam traceConfig.inner.output TraceResult
                ---@param traceConfig table
                function util.TraceNested(traceConfig) end
                "#,
        );
        let file_id = ws.def_file(
            "test.lua",
            r#"
                local ray = {}
                local traceData = {
                    inner = {
                        output = ray,
                    },
                }

                util.TraceNested(traceData)

                local hitFromConfig = traceData.inner.output.Hit
                local hitFromRay = ray.Hit
                "#,
        );
        let trace_result_ty = ws.ty("TraceResult");

        assert_eq!(
            index_expr_ty(&ws, file_id, "traceData.inner.output"),
            trace_result_ty
        );
        assert_eq!(index_expr_ty(&ws, file_id, "ray.Hit"), ws.ty("boolean"));
    }

    #[test]
    fn test_outparam_does_not_invent_missing_field() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "trace.lua",
            r#"
                ---@class TraceResult
                ---@field Hit boolean
                local TraceResult = {}

                util = {}

                ---@outparam traceConfig.output TraceResult
                ---@param traceConfig table
                function util.TraceLine(traceConfig) end
                "#,
        );
        let file_id = ws.def_file(
            "test.lua",
            r#"
                local traceData = {}

                util.TraceLine(traceData)

                local maybeOutput = traceData.output
                "#,
        );

        assert_eq!(
            index_expr_ty(&ws, file_id, "traceData.output"),
            ws.ty("nil")
        );
    }

    #[test]
    fn test_outparam_only_applies_after_call() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "trace.lua",
            r#"
                ---@class TraceResult
                ---@field Hit boolean
                local TraceResult = {}

                util = {}

                ---@outparam traceConfig.output TraceResult
                ---@param traceConfig table
                function util.TraceLine(traceConfig) end
                "#,
        );
        let file_id = ws.def_file(
            "test.lua",
            r#"
                local ray = {}
                local traceData = {
                    output = ray,
                }

                local before = ray.Hit
                util.TraceLine(traceData)
                local after = ray.Hit
                "#,
        );

        assert_eq!(
            index_expr_ty_at_occurrence(&ws, file_id, "ray.Hit", 0),
            ws.ty("nil")
        );
        assert_eq!(
            index_expr_ty_at_occurrence(&ws, file_id, "ray.Hit", 1),
            ws.ty("boolean")
        );
    }

    #[test]
    fn test_outparam_merges_across_optional_branch() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "trace.lua",
            r#"
                ---@class TraceResult
                ---@field Hit boolean
                local TraceResult = {}

                util = {}

                ---@outparam traceConfig.output TraceResult
                ---@param traceConfig table
                function util.TraceLine(traceConfig) end
                "#,
        );
        let file_id = ws.def_file(
            "test.lua",
            r#"
                local ray = {}
                local traceData = {
                    output = ray,
                }

                local shouldTrace = ...
                if shouldTrace then
                    util.TraceLine(traceData)
                end

                local after = ray.Hit
                "#,
        );

        assert_eq!(index_expr_ty(&ws, file_id, "ray.Hit"), ws.ty("boolean|nil"));
    }

    #[test]
    fn test_outparam_does_not_globally_bind_for_embedded_condition_call() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "trace.lua",
            r#"
                ---@class TraceResult
                ---@field Hit boolean
                local TraceResult = {}

                util = {}

                ---@outparam traceConfig.output TraceResult
                ---@param traceConfig table
                function util.TraceLine(traceConfig) end
                "#,
        );
        let file_id = ws.def_file(
            "test.lua",
            r#"
                local ray = {}
                local traceData = {
                    output = ray,
                }

                if false and util.TraceLine(traceData) then
                end

                local after = ray.Hit
                "#,
        );

        assert_eq!(index_expr_ty(&ws, file_id, "ray.Hit"), LuaType::Nil);
    }

    #[test]
    fn test_defaulted_field_reads_as_non_nil_after_omitted_table_assignment() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "test.lua",
            r#"
                ---@class TraceLike
                ---@field Hit boolean=false
                ---@field HitPos number
                local TraceLike = {}

                ---@type TraceLike
                local trace = {
                    HitPos = 1,
                }

                local hit = trace.Hit
                "#,
        );

        assert_eq!(index_expr_ty(&ws, file_id, "trace.Hit"), ws.ty("boolean"));
    }

    #[test]
    fn test_outparam_applies_after_call_used_in_local_initializer() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "trace.lua",
            r#"
                ---@class TraceResult
                ---@field Hit boolean
                local TraceResult = {}

                util = {}

                ---@outparam traceConfig.output TraceResult
                ---@param traceConfig table
                ---@return boolean
                function util.TraceLine(traceConfig) end
                "#,
        );
        let file_id = ws.def_file(
            "test.lua",
            r#"
                local ray = {}
                local traceData = {
                    output = ray,
                }

                local ok = util.TraceLine(traceData)
                local after = ray.Hit
                "#,
        );

        assert_eq!(index_expr_ty(&ws, file_id, "ray.Hit"), ws.ty("boolean"));
    }

    #[test]
    fn test_outparam_updates_original_container_alias_after_call() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "trace.lua",
            r#"
                ---@class TraceResult
                ---@field Hit boolean
                local TraceResult = {}

                util = {}

                ---@outparam traceConfig.output TraceResult
                ---@param traceConfig table
                function util.TraceLine(traceConfig) end
                "#,
        );
        let file_id = ws.def_file(
            "test.lua",
            r#"
                local ray = {}
                local traceData = {
                    output = ray,
                }

                local cfg = traceData
                util.TraceLine(cfg)
                local after = traceData.output.Hit
                "#,
        );

        assert_eq!(
            index_expr_ty(&ws, file_id, "traceData.output.Hit"),
            ws.ty("boolean")
        );
    }

    #[test]
    fn test_outparam_does_not_back_propagate_through_reassigned_alias() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "trace.lua",
            r#"
                ---@class TraceResult
                ---@field Hit boolean
                local TraceResult = {}

                util = {}

                ---@outparam traceConfig.output TraceResult
                ---@param traceConfig table
                function util.TraceLine(traceConfig) end
                "#,
        );
        let file_id = ws.def_file(
            "test.lua",
            r#"
                local ray = {}
                local traceData = {
                    output = ray,
                }
                local other = {}

                local cfg = traceData
                cfg = other
                util.TraceLine(cfg)
                local after = traceData.output.Hit
                "#,
        );

        assert_eq!(
            index_expr_ty(&ws, file_id, "traceData.output.Hit"),
            LuaType::Nil
        );
    }

    #[test]
    fn test_outparam_path_match_does_not_affect_similar_sibling_fields() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "trace.lua",
            r#"
                ---@class TraceResult
                ---@field Hit boolean
                local TraceResult = {}

                util = {}

                ---@outparam traceConfig.output TraceResult
                ---@param traceConfig table
                function util.TraceLine(traceConfig) end
                "#,
        );
        let file_id = ws.def_file(
            "test.lua",
            r#"
                local ray = {}
                local other = {}
                local traceData = {
                    output = ray,
                    output2 = other,
                }

                util.TraceLine(traceData)

                local hit = ray.Hit
                local untouched = other.Hit
                "#,
        );

        assert_eq!(index_expr_ty(&ws, file_id, "ray.Hit"), ws.ty("boolean"));
        assert_eq!(index_expr_ty(&ws, file_id, "other.Hit"), LuaType::Nil);
    }

    #[test]
    fn test_outparam_unions_effects_from_multiple_callable_candidates() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "trace.lua",
            r#"
                ---@class TraceResultBool
                ---@field Hit boolean
                local TraceResultBool = {}

                ---@class TraceResultString
                ---@field Hit string
                local TraceResultString = {}

                util = {}

                ---@outparam traceConfig.output TraceResultBool
                ---@param traceConfig table
                function util.TraceBool(traceConfig) end

                ---@outparam traceConfig.output TraceResultString
                ---@param traceConfig table
                function util.TraceString(traceConfig) end
                "#,
        );
        let file_id = ws.def_file(
            "test.lua",
            r#"
                local ray = {}
                local traceData = {
                    output = ray,
                }

                ---@param flag boolean
                local function run(flag)
                    (flag and util.TraceBool or util.TraceString)(traceData)
                    local hit = traceData.output.Hit
                end
                "#,
        );

        assert_eq!(
            index_expr_ty(&ws, file_id, "traceData.output.Hit"),
            ws.ty("boolean|string")
        );
    }

    #[test]
    fn test_outparam_duplicate_replaces() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "lib.lua",
            r#"
                ---@class ResultInt
                ---@field Value integer

                ---@class ResultStr
                ---@field Value string

                util = {}

                ---@outparam data.output ResultInt
                ---@outparam data.output ResultStr
                ---@param data table
                function util.Process(data) end
            "#,
        );
        let file_id = ws.def_file(
            "test.lua",
            r#"
                local ray = {}
                local data = {
                    output = ray,
                }

                util.Process(data)
                local val = data.output.Value
            "#,
        );
        // Second @outparam should win (last-writer-wins), so Value is string not integer
        assert_eq!(
            index_expr_ty(&ws, file_id, "data.output.Value"),
            ws.ty("string")
        );
    }
}
