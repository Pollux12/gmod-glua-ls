#[cfg(test)]
mod test {
    use glua_parser::{LuaAstNode, LuaAstToken, LuaFuncStat, LuaNameExpr, LuaVarExpr};
    use googletest::prelude::*;

    use crate::{DiagnosticCode, LuaType, VirtualWorkspace};

    #[test]
    fn unknown_local_stabilizes_from_anchored_usage_for_every_raw_query() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "annotations/context.lua",
            r#"
            ---@meta
            ---@class Vector
            ---@class HullTrace
            ---@field start Vector
            util = {}
            ---@param trace HullTrace
            function util.TraceHull(trace) end
            ---@return unknown
            function unknown_source() end
            "#,
        );
        let file_id = ws.def_file(
            "lua/autorun/context.lua",
            r#"
            local value = unknown_source()
            local before = value
            util.TraceHull({ start = value })
            local after = value
            "#,
        );
        let model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let root = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("syntax tree")
            .get_chunk_node();
        let types = root
            .descendants::<LuaNameExpr>()
            .filter(|name| name.get_name_text().as_deref() == Some("value"))
            .map(|name| {
                model
                    .infer_expr(LuaVarExpr::NameExpr(name).into())
                    .expect("value type")
            })
            .collect::<Vec<_>>();

        assert_eq!(
            types,
            vec![LuaType::Ref(crate::LuaTypeDeclId::global("Vector")); 3]
        );
    }

    /// An unknown-from-field initializer followed by a call assignment must land
    /// the call's type whether the call resolves eagerly or defers to the
    /// unresolve pass — the two write paths apply different acceptance rules, so
    /// the assignment is routed through the deferred one either way.
    #[test]
    fn decl_assignment_from_call_narrows_unknown_initializer() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "annotations/narrowing-call.lua",
            r#"
            ---@meta
            ---@return string
            function narrowing_localise() end
            "#,
        );
        let file_id = ws.def_file(
            "lua/autorun/narrowing-call.lua",
            r#"
            local function register(item)
                local name = item.name
                if name then
                    name = narrowing_localise()
                end
            end
            "#,
        );

        let db = ws.analysis.compilation.get_db();
        let root = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("syntax tree")
            .get_chunk_node();
        let name_decl = root
            .descendants::<LuaNameExpr>()
            .find(|name| name.get_name_text().as_deref() == Some("name"))
            .expect("decl name expr");
        let decl_id = db
            .get_reference_index()
            .get_local_reference(&file_id)
            .and_then(|refs| refs.get_decl_id(&name_decl.get_range()))
            .expect("local decl");

        assert_eq!(
            db.get_type_index()
                .get_type_cache(&crate::LuaTypeOwner::Decl(decl_id))
                .map(|cache| cache.as_type().clone()),
            Some(LuaType::String)
        );
    }

    #[test]
    fn stabilized_local_respects_assignment_regions() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "annotations/context-region.lua",
            r#"
            ---@meta
            ---@class RegionVector
            ---@class RegionTrace
            ---@field start RegionVector
            region_util = {}
            ---@param trace RegionTrace
            function region_util.Trace(trace) end
            ---@return unknown
            function region_unknown() end
            "#,
        );
        let file_id = ws.def_file(
            "lua/autorun/context-region.lua",
            r#"
            local value = region_unknown()
            region_util.Trace({ start = value })
            value = "changed"
            local after = value
            "#,
        );
        let model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let root = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("syntax tree")
            .get_chunk_node();
        let mut uses = root
            .descendants::<LuaNameExpr>()
            .filter(|name| name.get_name_text().as_deref() == Some("value"));
        let before = uses.next().expect("contextual use");
        let _assignment = uses.next().expect("assignment target");
        let after = uses.next().expect("post-assignment use");

        assert_eq!(
            model
                .infer_expr(LuaVarExpr::NameExpr(before).into())
                .expect("before type"),
            LuaType::Ref(crate::LuaTypeDeclId::global("RegionVector"))
        );
        assert_eq!(
            model
                .infer_expr(LuaVarExpr::NameExpr(after).into())
                .expect("after type"),
            LuaType::StringConst(internment::ArcIntern::from(smol_str::SmolStr::new(
                "changed",
            )))
        );
    }

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

        assert_that!(
            info.display_typ().clone(),
            matches_pattern!(LuaType::Signature(_))
        );

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

        assert_that!(
            call_info.display_typ().clone(),
            matches_pattern!(LuaType::Signature(_))
        );

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
