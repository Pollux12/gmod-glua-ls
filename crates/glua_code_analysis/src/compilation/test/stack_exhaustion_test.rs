#[cfg(test)]
mod test {
    // Regression tests: deeply nested input must degrade gracefully (parse
    // errors + Unknown fallback), never abort the process with a stack
    // overflow. Analysis threads range from 2 MB tokio/test workers to the
    // 256 MB analysis pool, and source-controlled nesting depth can exceed
    // any of them.
    //
    // The parse-tree types are `!Send`, so a parsed tree cannot cross threads:
    // each thread below parses its own copy of the same source. The large-
    // stack parse proves the shape is valid (zero errors given room); the
    // small-stack runs prove graceful degradation. The `+`-chain and `[]`
    // shapes parse iteratively (constant parser stack, so zero parse errors
    // on any thread) while inference recurses per level, which isolates the
    // `infer_expr` / `infer_doc_type` reserve guards from the parser guards:
    // with no parse errors to blame, `Unknown` can only come from inference
    // bailing out.
    //
    // Deep trees drop on the same small-stack threads via iterative rowan
    // green-node freeing, so no test leaks to avoid teardown.
    use glua_parser::{
        LuaAstNode, LuaAstToken, LuaDocTagParam, LuaExpr, LuaLocalName, LuaParser, LuaUnaryExpr,
        ParserConfig,
    };

    use crate::{
        DbIndex, DocTypeInferContext, FileId, LuaInferCache, LuaType, VirtualWorkspace,
        infer_doc_type, infer_expr,
    };

    const SMALL_STACK: usize = 2 * 1024 * 1024;
    const BIG_STACK: usize = 64 * 1024 * 1024;

    fn deep_calls_source(depth: usize) -> String {
        let mut body = String::from("local function f(x) return x end\nlocal v = ");
        for _ in 0..depth {
            body.push_str("f(");
        }
        body.push('1');
        for _ in 0..depth {
            body.push(')');
        }
        body.push('\n');
        body
    }

    // A `-` chain parses one frame per level (constant parser stack for these
    // depths) while inference recurses per level, and unlike `+` the negation
    // of an `Unknown` stays `Unknown` instead of folding away — so `Unknown`
    // here can only come from the inference guard.
    fn minus_chain_source(depth: usize) -> String {
        let mut body = String::from("local v = ");
        for _ in 0..depth {
            body.push_str("- ");
        }
        body.push_str("1\n");
        body
    }

    fn array_type_source(depth: usize) -> String {
        let mut body = String::from("---@param x T");
        for _ in 0..depth {
            body.push_str("[]");
        }
        body.push_str("\nlocal dummy = 1\n");
        body
    }

    /// The classic crash shape parses cleanly given room: 3000-deep calls
    /// must produce zero parse errors on a large-stack thread.
    #[test]
    fn deeply_nested_calls_parse_valid_on_large_stack() {
        let body = deep_calls_source(3000);
        std::thread::Builder::new()
            .stack_size(BIG_STACK)
            .spawn(move || {
                let tree = LuaParser::parse(&body, ParserConfig::default());
                assert!(
                    tree.get_errors().is_empty(),
                    "3000-deep calls must parse cleanly with room, got {:?}",
                    tree.get_errors()
                        .iter()
                        .map(|e| &e.message)
                        .collect::<Vec<_>>()
                );
            })
            .expect("worker thread should spawn")
            .join()
            .expect("large-stack parse must not overflow its stack");
    }

    /// The same shape degrades gracefully end to end on a production-sized
    /// (2 MB) worker: the query resolves the real local `v` (not an unrelated
    /// global). The parser trips its reserve partway, yet inference still
    /// resolves the value instead of aborting the process.
    #[test]
    fn deeply_nested_calls_infer_gracefully_on_small_stack() {
        let body = deep_calls_source(3000);
        std::thread::Builder::new()
            .stack_size(SMALL_STACK)
            .spawn(move || {
                let mut ws = VirtualWorkspace::new();
                let file_id = ws.def(&body);
                let tree = ws
                    .analysis
                    .compilation
                    .get_db()
                    .get_vfs()
                    .get_syntax_tree(&file_id)
                    .expect("Tree must exist");
                let local_v = tree
                    .get_chunk_node()
                    .descendants::<LuaLocalName>()
                    .find(|name| {
                        name.get_name_token()
                            .is_some_and(|token| token.get_name_text() == "v")
                    })
                    .expect("local v must exist");
                let semantic_model = ws
                    .analysis
                    .compilation
                    .get_semantic_model(file_id)
                    .expect("Model must exist");
                let token = local_v.get_name_token().expect("Name token must exist");
                let info = semantic_model
                    .get_semantic_info(token.syntax().clone().into())
                    .expect("Semantic info must exist");
                let ty = info.display_typ().clone();
                assert!(
                    matches!(ty, LuaType::IntegerConst(1)),
                    "3000-deep nesting must still resolve gracefully, got {ty:?}"
                );
            })
            .expect("worker thread should spawn")
            .join()
            .expect("production-sized worker must not overflow its stack");
    }

    // No single depth isolates the inference guard in both profiles:
    // release needs >= ~3500 for `infer_expr` to trip (3000 folds to a
    // const) while debug's fatter parser frames already error at ~3050,
    // which would blame the parser instead of inference. Depths encode
    // profile-specific frame sizes, so each profile uses its own depth
    // inside the clean-parse + infer-bail window.
    #[cfg(debug_assertions)]
    const MINUS_CHAIN_DEPTH: usize = 3000;
    #[cfg(not(debug_assertions))]
    const MINUS_CHAIN_DEPTH: usize = 5000;

    /// `infer_expr`'s own reserve guard, isolated: a `-` chain parses with
    /// one frame per level (zero parse errors even on 2 MB) while inference
    /// recurses per level, so `Unknown` here can only come from the inference
    /// guard.
    #[test]
    fn infer_expr_bails_via_stack_reserve_not_parse_errors() {
        let body = minus_chain_source(MINUS_CHAIN_DEPTH);
        let control_body = body.clone();
        std::thread::Builder::new()
            .stack_size(SMALL_STACK)
            .spawn(move || {
                let tree = LuaParser::parse(&body, ParserConfig::default());
                assert!(
                    tree.get_errors().is_empty(),
                    "minus chain must parse cleanly, got {:?}",
                    tree.get_errors()
                        .iter()
                        .map(|e| &e.message)
                        .collect::<Vec<_>>()
                );
                let outermost = tree
                    .get_chunk_node()
                    .descendants::<LuaUnaryExpr>()
                    .next()
                    .expect("chain must exist");
                let db = DbIndex::new();
                let mut cache = LuaInferCache::new(FileId::new(0), Default::default());
                let result = infer_expr(&db, &mut cache, LuaExpr::from(outermost));
                assert!(
                    matches!(result, Ok(LuaType::Unknown)),
                    "deep inference must bail to Unknown via the reserve guard, got {result:?}"
                );

                let shallow = LuaParser::parse("local v = - - 5\n", ParserConfig::default());
                assert!(shallow.get_errors().is_empty());
                let shallow_expr = shallow
                    .get_chunk_node()
                    .descendants::<LuaUnaryExpr>()
                    .next()
                    .expect("shallow chain must exist");
                let mut cache = LuaInferCache::new(FileId::new(0), Default::default());
                let shallow_result = infer_expr(&db, &mut cache, LuaExpr::from(shallow_expr));
                assert!(
                    matches!(&shallow_result, Ok(ty) if !ty.is_unknown()),
                    "shallow chain must infer normally, got {shallow_result:?}"
                );
            })
            .expect("worker thread should spawn")
            .join()
            .expect("production-sized worker must not overflow its stack");

        // Control: the same deep chain infers to a real type with room, so
        // the small-stack `Unknown` comes from stack pressure, not from the
        // shape itself.
        std::thread::Builder::new()
            .stack_size(BIG_STACK)
            .spawn(move || {
                let tree = LuaParser::parse(&control_body, ParserConfig::default());
                assert!(tree.get_errors().is_empty());
                let outermost = tree
                    .get_chunk_node()
                    .descendants::<LuaUnaryExpr>()
                    .next()
                    .expect("chain must exist");
                let db = DbIndex::new();
                let mut cache = LuaInferCache::new(FileId::new(0), Default::default());
                let result = infer_expr(&db, &mut cache, LuaExpr::from(outermost));
                assert!(
                    matches!(&result, Ok(ty) if !ty.is_unknown()),
                    "deep chain must infer normally with room, got {result:?}"
                );
            })
            .expect("worker thread should spawn")
            .join()
            .expect("large-stack inference must not overflow its stack");
    }

    /// `infer_doc_type`'s own reserve guard, isolated: suffixed `[]` types
    /// parse in a loop (zero parse errors even on 2 MB) while doc inference
    /// recurses per level, so `Unknown` here can only come from the doc
    /// inference guard.
    ///
    /// Depth 40000 sits above the ~28.5k first-trip point of the pre-fix tree
    /// build (28000 parsed cleanly, 29000 aborted a 2 MB release worker
    /// inside the vendored rowan `NodeCache` growth rehash, whose subtree
    /// hash recursed once per level past every parser/inference reserve
    /// guard): with the stored-hash rehash key the same shape parses in ~25
    /// ms release / ~0.1 s debug, so the `Unknown` below still isolates the
    /// `infer_doc_type` guard rather than the parser.
    #[test]
    fn infer_doc_type_bails_via_stack_reserve_not_parse_errors() {
        let body = array_type_source(40000);
        let control_body = body.clone();
        std::thread::Builder::new()
            .stack_size(SMALL_STACK)
            .spawn(move || {
                let tree = LuaParser::parse(&body, ParserConfig::default());
                assert!(
                    tree.get_errors().is_empty(),
                    "array nesting must parse cleanly, got {:?}",
                    tree.get_errors()
                        .iter()
                        .map(|e| &e.message)
                        .collect::<Vec<_>>()
                );
                let doc_type = tree
                    .get_chunk_node()
                    .descendants::<LuaDocTagParam>()
                    .next()
                    .expect("param tag must exist")
                    .get_type()
                    .expect("param type must exist");
                let db = DbIndex::new();
                let ctx = DocTypeInferContext::new(&db, FileId::new(1));
                let result = infer_doc_type(ctx, &doc_type);
                assert!(
                    result.is_unknown(),
                    "deep doc nesting must bail to Unknown, got {result:?}"
                );

                let shallow = LuaParser::parse(
                    "---@param x T[]\nlocal dummy = 1\n",
                    ParserConfig::default(),
                );
                assert!(shallow.get_errors().is_empty());
                let shallow_type = shallow
                    .get_chunk_node()
                    .descendants::<LuaDocTagParam>()
                    .next()
                    .expect("param tag must exist")
                    .get_type()
                    .expect("param type must exist");
                let shallow_result = infer_doc_type(ctx, &shallow_type);
                assert!(
                    !shallow_result.is_unknown(),
                    "shallow doc nesting must infer normally, got {shallow_result:?}"
                );
            })
            .expect("worker thread should spawn")
            .join()
            .expect("production-sized worker must not overflow its stack");

        // Control: the same deep nesting resolves to a real type with room.
        std::thread::Builder::new()
            .stack_size(BIG_STACK)
            .spawn(move || {
                let tree = LuaParser::parse(&control_body, ParserConfig::default());
                assert!(tree.get_errors().is_empty());
                let doc_type = tree
                    .get_chunk_node()
                    .descendants::<LuaDocTagParam>()
                    .next()
                    .expect("param tag must exist")
                    .get_type()
                    .expect("param type must exist");
                let db = DbIndex::new();
                let ctx = DocTypeInferContext::new(&db, FileId::new(1));
                let result = infer_doc_type(ctx, &doc_type);
                assert!(
                    !result.is_unknown(),
                    "deep doc nesting must infer normally with room, got {result:?}"
                );
            })
            .expect("worker thread should spawn")
            .join()
            .expect("large-stack inference must not overflow its stack");
    }

    /// Production fidelity: analysis also runs on default-stack tokio workers
    /// (~2 MB), not just test threads. The original crash shape (500
    /// sequential guarded writes) runs here on an explicit 2 MB thread: any
    /// stack overflow fails this test via the join instead of aborting the
    /// suite runner.
    #[test]
    fn guarded_slots_index_gracefully_on_production_sized_stacks() {
        let mut body = String::from("ns = ns or {}\n");
        for _ in 0..500 {
            body.push_str("ns.slot = ns.slot or {}\n");
        }
        body.push_str("return ns\n");

        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let mut ws = crate::VirtualWorkspace::new_with_init_std_lib();
                let file_id = ws.def(&body);
                ws.analysis
                    .compilation
                    .get_semantic_model(file_id)
                    .expect("semantic model");
            })
            .expect("worker thread should spawn")
            .join()
            .expect("production-sized worker must not overflow its stack");
    }
}
