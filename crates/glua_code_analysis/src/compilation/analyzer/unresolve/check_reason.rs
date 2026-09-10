use rustc_hash::FxHashMap;

use glua_parser::{LuaAstNode, LuaExpr};

use crate::{
    DbIndex, InferFailReason, LuaDocReturnInfo, LuaType, LuaTypeCache, ReturnTypeKind,
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
            let expr = resolve_unresolve_expr(db, expr)?;
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

fn resolve_unresolve_expr(
    db: &DbIndex,
    expr: &crate::InFiled<glua_parser::LuaSyntaxId>,
) -> Option<crate::InFiled<LuaExpr>> {
    let root = db
        .get_vfs()
        .get_syntax_tree(&expr.file_id)
        .map(|tree| tree.get_red_root())?;
    Some(crate::InFiled::new(
        expr.file_id,
        LuaExpr::cast(expr.value.to_node_from_root(&root)?)?,
    ))
}

/// Memoised `UnResolveExpr` reach results for one `try_resolve` call.
///
/// The check runs a full `infer_expr`, which on a cold cache is the expensive
/// half of the reach gate. Re-running it once per wave for every surviving
/// expression group is pure re-derivation while nothing the expression could
/// read has moved, so entries are validated against the type/return/param/
/// module/special-call/contribution/dynamic-field write epochs.
#[derive(Default)]
pub struct ExprReachMemo {
    entries: FxHashMap<InferFailReason, (bool, u64, u64, u64, u64, u64, u64, u64)>,
}

impl ExprReachMemo {
    pub fn check(
        &mut self,
        db: &DbIndex,
        infer_manager: &mut InferCacheManager,
        reason: &InferFailReason,
    ) -> bool {
        let epochs = (
            db.get_type_index().type_writes(),
            db.get_signature_index().return_writes(),
            db.get_signature_index().param_writes(),
            db.get_module_index().module_writes(),
            db.get_flow_index().special_call_writes(),
            db.get_call_site_param_index().contrib_writes(),
            db.get_dynamic_field_index().dynamic_writes(),
        );
        if let Some((
            reached,
            type_epoch,
            return_epoch,
            param_epoch,
            module_epoch,
            special_epoch,
            contrib_epoch,
            dynamic_epoch,
        )) = self.entries.get(reason)
            && (
                *type_epoch,
                *return_epoch,
                *param_epoch,
                *module_epoch,
                *special_epoch,
                *contrib_epoch,
                *dynamic_epoch,
            ) == epochs
        {
            return *reached;
        }
        let reached = check_reach_reason(db, infer_manager, reason).unwrap_or(false);
        self.entries.insert(
            reason.clone(),
            (
                reached, epochs.0, epochs.1, epochs.2, epochs.3, epochs.4, epochs.5, epochs.6,
            ),
        );
        reached
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
    // A3-remnant design check (language-server-performance §3, declined): the
    // `None` group is deliberately NOT pre-classified at enqueue into "undefined
    // global" vs "genuinely deferred". A `None` floor is a no-op by construction
    // (there is no owner to write), while every specific reason floors to
    // `Unknown` (decl/member/return) or publishes a placeholder. Re-labelling a
    // genuinely deferred item as an undefined global would therefore swap "leave
    // no cache and let later passes derive it" for "commit `Unknown`/`nil` as
    // the final answer", which the `moved == 0` totals cannot catch and which
    // downstream reads observe (undefined-global RHS is `nil` only on the
    // assignment path that opts in; elsewhere the same read must stay unresolved
    // so narrowing keeps the other branches). Proving the same final floor would
    // require showing the classification never fires on an item whose inputs
    // settle later in the same batch, which the enqueue-time DB cannot show. The
    // wave keeps carrying `None` items until they resolve or the force phase
    // drops them.
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
            write_type_cache(
                db,
                (*member_id).into(),
                LuaTypeCache::InferType(LuaType::Unknown),
                TypeCacheWriteMode::InsertOnly,
            );
        }
        // An expression's slot in the type index is the `@as` cast slot that
        // `infer_expr` consults before inferring anything. Flooring it would
        // make a placeholder read as a cast of the expression to `unknown`,
        // permanently and however its operands settle. The blocked items stay
        // blocked instead; an expression that never infers leaves them typed
        // as nothing rather than as a fabricated answer.
        InferFailReason::UnResolveExpr(_) => {
            return Some(());
        }
        InferFailReason::UnResolveSignatureReturn(signature_id) => {
            // Same deferral as the member-type arm above, for the same
            // reason. A return this round could not infer is usually one
            // whose own `UnResolveReturn` item is still queued; flooring it
            // publishes `unknown` as a *resolved* return, which every
            // caller waiting on this reason then commits as a final fact.
            if loop_count == 0 {
                return Some(());
            }
            let signature = db.get_signature_index_mut().get_mut(signature_id)?;
            if !signature.is_resolve_return() {
                signature.return_docs = vec![LuaDocReturnInfo {
                    name: None,
                    type_ref: LuaType::Unknown,
                    default_value: None,
                    description: None,
                    attributes: None,
                    return_kind: ReturnTypeKind::default(),
                }];
                signature.resolve_return = SignatureReturnStatus::InferResolve;
                db.get_signature_index_mut()
                    .note_return_write(*signature_id);
            }
        }
        InferFailReason::UnResolveModuleExport(file_id) => {
            let module = db.get_module_index_mut().get_module_mut(*file_id)?;
            if module.export_type.is_none() {
                module.export_type = Some(LuaType::Unknown);
                db.get_module_index_mut().note_module_write(*file_id);
            }
        }
    }

    Some(())
}

#[cfg(test)]
mod test {
    use crate::{
        DbIndex, FileId, InferFailReason, LuaSignatureId, SignatureReturnStatus,
        compilation::analyzer::unresolve::check_reason::resolve_as_unknown,
    };

    /// The signature-return floor may not fire on the first round: the return's
    /// own deferred item has not had a retry yet, and a floored return is
    /// published as `InferResolve`, which callers commit as a final type.
    #[test]
    fn signature_return_floor_is_deferred_past_the_first_round() {
        let mut db = DbIndex::new();
        let signature_id = LuaSignatureId::new(FileId { id: 1 }, 0.into());
        db.get_signature_index_mut().get_or_create(signature_id);
        let reason = InferFailReason::UnResolveSignatureReturn(signature_id);

        resolve_as_unknown(&mut db, &reason, 0);
        assert_eq!(
            db.get_signature_index()
                .get(&signature_id)
                .unwrap()
                .resolve_return,
            SignatureReturnStatus::UnResolve
        );

        resolve_as_unknown(&mut db, &reason, 1);
        assert_eq!(
            db.get_signature_index()
                .get(&signature_id)
                .unwrap()
                .resolve_return,
            SignatureReturnStatus::InferResolve
        );
    }
}
