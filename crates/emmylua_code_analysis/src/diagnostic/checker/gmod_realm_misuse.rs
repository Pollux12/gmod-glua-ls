use emmylua_parser::{LuaAstNode, LuaCallExpr, PathTrait};

use crate::{DiagnosticCode, GmodRealm, SemanticModel};

use super::{Checker, DiagnosticContext};

pub struct GmodRealmMisuseChecker;

impl Checker for GmodRealmMisuseChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::GmodRealmMisuse];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let file_id = semantic_model.get_file_id();
        let db = semantic_model.get_db();
        let Some(realm_metadata) = db
            .get_gmod_infer_index()
            .get_realm_file_metadata(&file_id)
        else {
            return;
        };

        let file_realm = realm_metadata.inferred_realm;
        let has_branch_ranges = !realm_metadata.branch_realm_ranges.is_empty();

        for call_expr in semantic_model.get_root().descendants::<LuaCallExpr>() {
            // Determine effective realm at this call site
            let effective_realm = if has_branch_ranges {
                let offset = call_expr.get_range().start();
                db.get_gmod_infer_index()
                    .get_realm_at_offset(&file_id, offset)
            } else {
                file_realm
            };

            if is_add_cslua_file_call(&call_expr) && effective_realm == GmodRealm::Client {
                context.add_diagnostic(
                    DiagnosticCode::GmodRealmMisuse,
                    call_expr.get_range(),
                    t!("AddCSLuaFile is server-only and may be invalid in inferred client realm.")
                        .to_string(),
                    None,
                );
            }
        }
    }
}

fn is_add_cslua_file_call(call_expr: &LuaCallExpr) -> bool {
    let Some(call_path) = call_expr.get_access_path() else {
        return false;
    };

    call_path == "AddCSLuaFile"
        || call_path.ends_with(".AddCSLuaFile")
        || call_path.ends_with(":AddCSLuaFile")
}
