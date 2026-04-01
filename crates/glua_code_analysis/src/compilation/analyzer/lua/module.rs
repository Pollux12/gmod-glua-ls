use glua_parser::{LuaAstNode, LuaChunk, LuaExpr};

use crate::{
    DbIndex, FileId, InferFailReason, LuaDeclId, LuaSemanticDeclId, LuaSignatureId,
    compilation::analyzer::unresolve::UnResolveModule, db_index::LuaType,
};

use super::{LuaAnalyzer, LuaReturnPoint, func_body::analyze_func_body_returns};

pub fn analyze_chunk_return(analyzer: &mut LuaAnalyzer, chunk: LuaChunk) -> Option<()> {
    let block = chunk.get_block()?;
    let return_exprs = analyze_func_body_returns(block);
    for point in return_exprs {
        if let LuaReturnPoint::Expr(expr) = point {
            let expr_type = match analyzer.infer_expr(&expr) {
                Ok(expr_type) => expr_type,
                Err(InferFailReason::None) => LuaType::Unknown,
                Err(reason) => {
                    let unresolve = UnResolveModule {
                        file_id: analyzer.file_id,
                        expr,
                    };
                    analyzer.context.add_unresolve(unresolve.into(), reason);
                    return None;
                }
            };

            let semantic_id = compute_module_semantic_id(&analyzer.db, analyzer.file_id, &expr);

            let module_info = analyzer
                .db
                .get_module_index_mut()
                .get_module_mut(analyzer.file_id)?;
            match expr_type {
                LuaType::Variadic(multi) => {
                    let ty = multi.get_type(0)?;
                    module_info.export_type = Some(ty.clone());
                }
                _ => {
                    module_info.export_type = Some(expr_type);
                }
            }
            module_info.semantic_id = semantic_id;
            break;
        }
    }

    Some(())
}

pub fn compute_module_semantic_id(
    db: &DbIndex,
    file_id: FileId,
    expr: &LuaExpr,
) -> Option<LuaSemanticDeclId> {
    match expr {
        LuaExpr::TableExpr(table_expr) => Some(LuaSemanticDeclId::LuaDecl(LuaDeclId::new(
            file_id,
            table_expr.get_position(),
        ))),
        LuaExpr::ClosureExpr(closure_expr) => {
            let sig_id = LuaSignatureId::from_closure(file_id, closure_expr);
            Some(LuaSemanticDeclId::Signature(sig_id))
        }
        LuaExpr::NameExpr(name_expr) => {
            let name = name_expr.get_name_text()?;
            let tree = db.get_decl_index().get_decl_tree(&file_id)?;
            let decl = tree.find_local_decl(&name, name_expr.get_position())?;

            Some(LuaSemanticDeclId::LuaDecl(decl.get_id()))
        }
        _ => None,
    }
}
