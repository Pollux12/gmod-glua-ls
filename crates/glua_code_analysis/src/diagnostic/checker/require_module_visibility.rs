use glua_parser::{LuaAstNode, LuaExpr};

use crate::{DiagnosticCode, LuaType, SemanticModel, check_export_visibility};

use super::{Checker, DiagnosticContext};

pub struct RequireModuleVisibilityChecker;

impl Checker for RequireModuleVisibilityChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::RequireModuleNotVisible];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let Some(buckets) = context.file_buckets() else {
            return;
        };
        let path_args = buckets
            .call_exprs
            .iter()
            .filter_map(|call_expr| {
                crate::require_path_arg(
                    semantic_model.get_db(),
                    semantic_model.get_file_id(),
                    call_expr,
                )
            })
            .collect::<Vec<_>>();
        for path_arg in path_args {
            check_require_path(context, semantic_model, path_arg);
        }
    }
}

fn check_require_path(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    arg_expr: LuaExpr,
) -> Option<()> {
    // Get the module path
    let ty = semantic_model
        .infer_expr(arg_expr.clone())
        .unwrap_or(LuaType::Any);
    let module_path = match ty {
        LuaType::StringConst(s) => s.as_ref().to_string(),
        _ => return Some(()),
    };

    // Look up the module info
    let module_info = semantic_model
        .get_db()
        .get_module_index()
        .find_module_for_file(&module_path, semantic_model.get_file_id())?;

    // Check visibility
    if !check_export_visibility(semantic_model, module_info).unwrap_or(false) {
        context.add_diagnostic(
            DiagnosticCode::RequireModuleNotVisible,
            arg_expr.get_range(),
            format!(
                "Module '{module}' is not visible. It has @export restrictions.",
                module = module_info.full_module_name
            )
            .to_string(),
            None,
        );
    }

    Some(())
}
