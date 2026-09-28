//! Resolves each call's callee once per file, for the per-file passes that
//! read it: call-site parameters and net helpers.

use std::sync::Arc;

use glua_parser::{LuaAstNode, LuaCallExpr};

use crate::semantic::{get_prefix_expr_signature_id, infer_expr};
use crate::{
    CacheOptions, DbIndex, FileCallTargets, FileId, LuaAnalysisPhase, LuaCallTarget, LuaInferCache,
};

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

/// What each call of `file_id` targets.
pub(super) fn compute_file_call_targets(db: &DbIndex, file_id: FileId) -> FileCallTargets {
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_chunk_node())
    else {
        return FileCallTargets::default();
    };
    let mut cache = LuaInferCache::new(file_id, pass_view());
    root.descendants::<LuaCallExpr>()
        .map(|call_expr| {
            let target = resolve_call_target(db, &mut cache, &call_expr);
            (call_expr.get_range(), target)
        })
        .collect()
}

pub(super) fn apply_file_call_targets(db: &mut DbIndex, file_id: FileId, targets: FileCallTargets) {
    db.get_call_target_index_mut().set_file(file_id, targets);
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

/// What `file_id`'s calls target, for a pass ranked after the file's
/// `CallTargets` derivation and starting at the same stage, so that it ran
/// first.
pub(super) fn file_call_targets(db: &DbIndex, file_id: FileId) -> Arc<FileCallTargets> {
    let targets = db.get_call_target_index().file(file_id);
    debug_assert!(
        targets.is_some(),
        "{file_id:?}'s call targets resolve first"
    );
    targets.unwrap_or_default()
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
        let Some(stored) = db.get_call_target_index().file(file_id) else {
            continue;
        };
        for (range, fresh) in compute_file_call_targets(db, file_id) {
            if stored.get(&range) != Some(&fresh) {
                eprintln!(
                    "[cache] call target mismatch {file_id:?} {range:?} stored={:?} fresh={fresh:?}",
                    stored.get(&range),
                );
            }
        }
    }
}
