//! Resolves each call's callee in a derivation of its own, for the passes
//! that read it: call-site parameters and net helpers.

use glua_parser::{LuaAstNode, LuaCallExpr, LuaSyntaxId};

use crate::semantic::{get_prefix_expr_signature_id, infer_expr};
use crate::{CacheOptions, DbIndex, FileId, LuaAnalysisPhase, LuaCallTarget, LuaInferCache};

/// The view the per-file passes infer callees under: after the statements,
/// with dynamic fields, and without the call-site parameter facts one of them
/// produces.
pub(crate) fn pass_view() -> CacheOptions {
    CacheOptions {
        analysis_phase: LuaAnalysisPhase::Force,
        dynamic_fields_visible: true,
        call_site_params_visible: false,
    }
}

/// The call `syntax_id` names in `file_id`, if the file still has it.
pub(super) fn call_at(
    db: &DbIndex,
    file_id: FileId,
    syntax_id: LuaSyntaxId,
) -> Option<LuaCallExpr> {
    let root = db.get_vfs().get_syntax_tree(&file_id)?.get_chunk_node();
    LuaCallExpr::cast(syntax_id.to_node_from_root(root.syntax())?)
}

/// Resolves the call `syntax_id` names with `cache`, which infers under the
/// view [`pass_view`] names, and stores what it targets.
pub(super) fn derive_call_target(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    syntax_id: LuaSyntaxId,
) {
    let file_id = cache.get_file_id();
    let Some(call_expr) = call_at(db, file_id, syntax_id) else {
        return;
    };
    let target = resolve_call_target(db, cache, &call_expr);
    db.get_call_target_index_mut()
        .set_target(file_id, call_expr.get_range(), target);
}

/// What `call_expr` targets, inferred with `cache`.
pub(crate) fn resolve_call_target(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> LuaCallTarget {
    let declared = get_prefix_expr_signature_id(db, cache, call_expr);
    let prefix_type = call_expr
        .get_prefix_expr()
        .and_then(|prefix| infer_expr(db, cache, prefix).ok());
    LuaCallTarget::new(declared, prefix_type.as_ref())
}

/// What `call_expr`, one of `file_id`'s calls, targets, for a pass ranked after
/// the `CallTarget` derivations and starting at the same stage, so that the
/// call's ran first.
pub(super) fn call_target_of<'a>(
    db: &'a DbIndex,
    file_id: FileId,
    call_expr: &LuaCallExpr,
) -> Option<&'a LuaCallTarget> {
    let target = db
        .get_call_target_index()
        .target(file_id, call_expr.get_range());
    debug_assert!(
        target.is_some(),
        "{file_id:?}'s call target at {:?} resolves first",
        call_expr.get_range()
    );
    target
}

/// Under `GLUALS_CHECK_CACHE`, once a re-analysis settled: reports every
/// stored call target that resolving its call again finds different.
pub(super) fn check_settled_call_targets(db: &DbIndex) {
    if !crate::db_index::derivation::check_cache() {
        return;
    }
    let mut file_ids = db.get_vfs().get_all_local_file_ids();
    file_ids.sort_unstable();
    for file_id in file_ids {
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_chunk_node())
        else {
            continue;
        };
        let mut cache = LuaInferCache::new(file_id, pass_view());
        for call_expr in root.descendants::<LuaCallExpr>() {
            let range = call_expr.get_range();
            let Some(stored) = db.get_call_target_index().target(file_id, range) else {
                continue;
            };
            let fresh = resolve_call_target(db, &mut cache, &call_expr);
            if *stored != fresh {
                eprintln!(
                    "[cache] call target mismatch {file_id:?} {range:?} stored={stored:?} fresh={fresh:?}",
                );
            }
        }
    }
}
