use glua_parser::{LuaAstNode, LuaCallExpr};

use crate::{DiagnosticCode, LuaType, SemanticModel, check_export_visibility};

use super::{Checker, DiagnosticContext};

pub struct RequireModuleVisibilityChecker;

impl Checker for RequireModuleVisibilityChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::RequireModuleNotVisible];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let Some(buckets) = context.file_buckets() else {
            return;
        };
        for call_expr in buckets.call_exprs.iter().cloned() {
            if call_expr.is_require() {
                check_require_call_expr(context, semantic_model, call_expr);
            }
        }
    }
}

fn check_require_call_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    call_expr: LuaCallExpr,
) -> Option<()> {
    let args_list = call_expr.get_args_list()?;
    let arg_expr = args_list.get_args().next()?;

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
