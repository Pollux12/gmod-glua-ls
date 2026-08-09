use rustc_hash::FxHashMap;

use glua_parser::LuaAstNode;

use crate::{
    DbIndex, InFiled, InferFailReason, LuaDocReturnInfo, LuaType, LuaTypeCache, ReturnTypeKind,
    SignatureReturnStatus,
    compilation::analyzer::{
        common::{TypeCacheWriteMode, write_type_cache},
        infer_cache_manager::InferCacheManager,
    },
    infer_expr, infer_param,
};

use super::UnResolve;

pub fn check_reach_reason(
    db: &DbIndex,
    infer_manager: &mut InferCacheManager,
    reason: &InferFailReason,
) -> Option<bool> {
    match reason {
        InferFailReason::None
        | InferFailReason::FieldNotFound
        | InferFailReason::UnResolveOperatorCall
        // The member map these items read is fully indexed before this pipeline
        // runs, so the first wave is already the wave it fills.
        | InferFailReason::UnResolveIterTemplate
        | InferFailReason::RecursiveInfer => Some(true),
        InferFailReason::UnResolveDeclType(decl_id) => {
            let decl = db.get_decl_index().get_decl(decl_id)?;
            let typ = db.get_type_index().get_type_cache(&(*decl_id).into());
            if typ.is_none() && decl.is_param() {
                return Some(infer_param(db, decl).is_ok());
            }

            Some(typ.is_some())
        }
        InferFailReason::UnResolveTypeDecl(type_id) => {
            Some(db.get_type_index().get_type_decl(type_id).is_some())
        }
        InferFailReason::UnResolveMemberType(member_id) => {
            // `resolve_member_type` raises this reason naming the one
            // member whose type cache was missing, so that exact cache is
            // what "reached" has to mean. Asking the member *item* instead
            // — reached through `get_current_owner` — answers a different
            // question: a member parked on its global path resolves through
            // the parked item while inference, which reaches it through the
            // prefix's type, still sees no cache.
            Some(
                db.get_type_index()
                    .get_type_cache(&(*member_id).into())
                    .is_some(),
            )
        }
        InferFailReason::UnResolveExpr(expr) => {
            let cache = infer_manager.get_infer_cache(expr.file_id);
            let result = infer_expr(db, cache, expr.value.clone());
            Some(result.is_ok())
        }
        InferFailReason::UnResolveSignatureReturn(signature_id) => {
            let signature = db.get_signature_index().get(signature_id)?;
            Some(signature.is_resolve_return())
        }
        InferFailReason::UnResolveModuleExport(file_id) => {
            let module = db.get_module_index().get_module(*file_id)?;
            Some(module.export_type.is_some())
        }
        InferFailReason::UnSealedDynamicFields => Some(db.get_dynamic_field_index().is_sealed()),
    }
}

pub fn resolve_all_reason(
    db: &mut DbIndex,
    reason_unresolves: &mut FxHashMap<InferFailReason, Vec<UnResolve>>,
    loop_count: usize,
) {
    let mut reasons: Vec<InferFailReason> = reason_unresolves.keys().cloned().collect();
    reasons.sort_unstable_by(super::infer_fail_reason_stable_cmp);
    for reason in &reasons {
        resolve_as_unknown(db, reason, loop_count);
    }
}

pub fn resolve_as_unknown(
    db: &mut DbIndex,
    reason: &InferFailReason,
    loop_count: usize,
) -> Option<()> {
    super::census::record(
        "resolve_as_unknown.called",
        super::infer_fail_reason_label(reason),
    );
    match reason {
        InferFailReason::None
        | InferFailReason::FieldNotFound
        | InferFailReason::UnResolveTypeDecl(_)
        | InferFailReason::UnResolveOperatorCall
        // Names no owner to floor: the reason is the index's build state, and it
        // always clears before the last unresolve rounds.
        | InferFailReason::UnSealedDynamicFields
        // The template-ref placeholder these items would floor is already in the
        // cache and is what the flow fallback re-derives from, so the graceful
        // floor is leaving it alone.
        | InferFailReason::UnResolveIterTemplate
        | InferFailReason::RecursiveInfer => {
            return Some(());
        }
        InferFailReason::UnResolveDeclType(decl_id) => {
            super::census::record("resolve_as_unknown.floor", "decl_type");
            write_type_cache(
                db,
                (*decl_id).into(),
                LuaTypeCache::InferType(LuaType::Unknown),
                TypeCacheWriteMode::InsertOnly,
            );
        }
        InferFailReason::UnResolveMemberType(member_id) => {
            // 第一次循环不处理, 或许需要判断`unresolves`是否全为取值再跳过?
            if loop_count == 0 {
                return Some(());
            }
            // Fill the cache `check_reach_reason` reads, which is the one
            // member named by the reason. Going through the member *item*
            // asked a different question — a member parked on its global
            // path resolves through the parked item, so this arm wrote
            // nothing and the blocked member kept no type forever.
            super::census::record("resolve_as_unknown.floor", "member_type");
            write_type_cache(
                db,
                (*member_id).into(),
                LuaTypeCache::InferType(LuaType::Unknown),
                TypeCacheWriteMode::InsertOnly,
            );
        }
        InferFailReason::UnResolveExpr(expr) => {
            super::census::record("resolve_as_unknown.floor", "expr");
            let key = InFiled::new(expr.file_id, expr.value.get_syntax_id());
            write_type_cache(
                db,
                key.into(),
                LuaTypeCache::InferType(LuaType::Unknown),
                TypeCacheWriteMode::InsertOnly,
            );
        }
        InferFailReason::UnResolveSignatureReturn(signature_id) => {
            let signature = db.get_signature_index_mut().get_mut(signature_id)?;
            if !signature.is_resolve_return() {
                super::census::record("resolve_as_unknown.floor", "signature_return");
                signature.return_docs = vec![LuaDocReturnInfo {
                    name: None,
                    type_ref: LuaType::Unknown,
                    default_value: None,
                    description: None,
                    attributes: None,
                    return_kind: ReturnTypeKind::default(),
                }];
                signature.resolve_return = SignatureReturnStatus::InferResolve;
            }
        }
        InferFailReason::UnResolveModuleExport(file_id) => {
            let module = db.get_module_index_mut().get_module_mut(*file_id)?;
            if module.export_type.is_none() {
                super::census::record("resolve_as_unknown.floor", "module_export");
                module.export_type = Some(LuaType::Unknown);
            }
        }
    }

    Some(())
}
