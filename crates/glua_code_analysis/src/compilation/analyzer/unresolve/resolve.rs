use std::{
    collections::{BTreeMap, HashSet},
    ops::Deref,
    sync::Arc,
};

use glua_parser::{
    BinaryOperator, LuaAstNode, LuaAstToken, LuaCallExpr, LuaExpr, LuaIndexExpr, LuaTableExpr,
    LuaTableField, PathTrait,
};
use rowan::TextSize;

use crate::semantic::{semantic_decl_closure_signature_id, semantic_decl_value_expr};
use crate::{
    DbIndex, FileId, GlobalId, InferFailReason, LuaDeclId, LuaDeclOrMemberId, LuaDeclTypeKind,
    LuaDocReturnInfo, LuaMember, LuaMemberId, LuaMemberInfo, LuaMemberKey, LuaOperator,
    LuaOperatorMetaMethod, LuaOperatorOwner, LuaSemanticDeclId, LuaType, LuaTypeCache, LuaTypeDecl,
    LuaTypeDeclId, LuaTypeFlag, LuaTypeOwner, OperatorFunction, RenderLevel, ReturnTypeKind,
    SemanticDeclLevel, SignatureReturnStatus, TypeOps, VariadicType,
    compilation::analyzer::{
        common::{
            DeclWrite, TypeCacheWriteMode, add_member, bind_decl_write, bind_resolved_type,
            holds_unbound_iter_template,
        },
        lua::{
            IterVarInference, alias_target_global_path, compute_module_semantic_id,
            derive_inferred_return_with_reads, has_multiple_distinct_index_expr_member_owners,
            is_guarded_table_assignment_index_expr, mark_resolved_member_assignment,
            resolve_index_expr_member_owner_for_file,
        },
        unresolve::UnResolveSpecialCall,
    },
    db_index::{
        LuaFunctionType, LuaMemberFeature, LuaMemberOwner, LuaOutParamRoot, LuaSignature,
        LuaSignatureId, SpecialCallFact, derivation::StageRule,
    },
    find_members_with_key, humanize_type,
    semantic::{
        InferGuard, LuaInferCache, SelfRefId, SemanticDeclGuard, VarRefId, VarRefRootId,
        get_var_expr_var_ref_id, infer_call_expr_func, infer_expr, resolve_dynamic_field_member,
        try_infer_expr_semantic_decl,
    },
};

use super::{
    ResolveResult, UnResolveDecl, UnResolveMember, UnResolveModule, UnResolveReturn,
    resolve_closure::inferred_return_tail_matching_documented_first, writes::ResolveWrites,
};

pub fn try_resolve_decl(
    db: &DbIndex,
    writes: &mut ResolveWrites,
    cache: &mut LuaInferCache,
    decl: &mut UnResolveDecl,
) -> ResolveResult {
    let expr = decl.expr.clone();
    if should_defer_guarded_index_alias_resolution(db, cache, &expr) {
        return Err(InferFailReason::FieldNotFound);
    }

    let expr_type = infer_expr(db, cache, expr.clone())?;
    let decl_id = decl.decl_id;
    let expr_type = match &expr_type {
        LuaType::Variadic(multi) => multi
            .get_type(decl.ret_idx)
            .cloned()
            .unwrap_or(LuaType::Unknown),
        _ => expr_type,
    };

    if holds_unbound_iter_template(db, decl.file_id, &expr, &expr_type) {
        return Err(InferFailReason::UnResolveIterTemplate);
    }

    // Displacing an uninformative cache is the initializer's privilege: it is
    // the decl's own value, so it may narrow from any right-hand side whose
    // answer can still improve. An assignment may only narrow from a call or
    // index read — the boundary the file walk enforces in
    // `should_retry_narrowing_decl_assignment` before it queues one. An
    // assignment that landed here only because its right-hand side could not be
    // inferred yet arrives without that check, and whether the walk's inference
    // failed is a property of the batch: once the callee's return resolves, the
    // same assignment infers cleanly and the walk refuses the narrowing.
    //
    // Either way the write goes through the positional claim, so a write that
    // resolved late can still take the slot back from one that ran ahead of it.
    let may_improve = crate::compilation::analyzer::initializer_may_improve_after_resolve(&expr);
    let is_initializer = decl_expr_is_initializer(db, decl_id, &expr);
    let write = DeclWrite {
        position: expr.get_position(),
        may_improve_after_resolve: may_improve,
        reads_out_of_decl: crate::compilation::analyzer::lua::expr_reads_out_of_decl(
            db,
            decl.file_id,
            decl_id,
            &expr,
        ),
        may_narrow_uninformative: if is_initializer {
            may_improve
        } else {
            crate::compilation::analyzer::initializer_reads_through_call_or_index(&expr)
        },
        resolved_initializer: is_initializer && may_improve,
        fills_own_default: crate::compilation::analyzer::lua::expr_fills_own_default(
            db,
            decl.file_id,
            decl_id,
            &expr,
        ),
    };
    writes.push(move |db, _| {
        bind_decl_write(db, decl_id, LuaTypeCache::InferType(expr_type), write);
    });
    Ok(())
}

/// `try_resolve_member` ends in `add_member`, which can only re-home a
/// member that already exists. The Lua pass creates the member itself in
/// `apply_index_expr_member_owner`, but the branch that queued this
/// deferral never got that far — its prefix was not inferable while its own
/// file was analysed — so `t[k] = v` would otherwise leave no member at
/// all.
fn create_deferred_index_expr_member(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    prefix_type: &LuaType,
    owner: LuaMemberOwner,
    member_id: LuaMemberId,
    alias_path: Option<GlobalId>,
    set_owner_only: bool,
) -> Option<()> {
    let index_expr = LuaIndexExpr::cast(
        db.get_vfs()
            .syntax_node_around(&member_id.file_id, *member_id.get_syntax_id())?,
    )?;
    let index_key = index_expr.get_index_key()?;
    let member_key = LuaMemberKey::from_index_key_or_unknown(db, cache, &index_key).ok()?;
    // An unknown key cannot pick between candidate tables, so pinning the member
    // to one of them would be a guess. Same skip as the Lua pass.
    if matches!(member_key, LuaMemberKey::ExprType(ref typ) if typ.is_unknown())
        && has_multiple_distinct_index_expr_member_owners(prefix_type)
    {
        return Some(());
    }

    let feature = if db.get_module_index().is_meta_file(&member_id.file_id) {
        LuaMemberFeature::MetaDefine
    } else {
        LuaMemberFeature::FileDefine
    };
    let guarded = is_guarded_table_assignment_index_expr(&index_expr);
    let member = LuaMember::new(member_id, member_key, feature, None);
    {
        let member_index = db.get_member_index_mut();
        member_index.add_member_with_owner_only(owner.clone(), member, set_owner_only);
        // The same alias provenance the immediate path records: a write
        // through an alias of a global path must follow the path's
        // `---@class` flips even though its first home came from a prefix
        // type read mid-fixpoint.
        if let Some(alias_path) = alias_path {
            member_index.home_alias_member_with_provenance(
                owner,
                member_id.file_id,
                member_id,
                alias_path,
                set_owner_only,
            );
        }
        // The owner above came from a prefix type read mid-fixpoint, so this
        // placement is provisional: the post-settle re-home is the authority on
        // where the member ends up, and it needs to know it may detach this one.
        member_index.mark_deferred_index_expr_member(member_id);
        // `add_member` records the enclosing function scope for `FileDefine`
        // index-expr members only; for the rest it stores `None`. Same follow-up
        // the Lua pass does in `apply_index_expr_member_owner_with_guarded`.
        if !matches!(feature, LuaMemberFeature::FileDefine) {
            let function_scope = member_index
                .enclosing_function_scope_range(member_id.file_id, member_id.get_position());
            member_index.set_member_function_scope_range(member_id, function_scope);
        }
    }
    // Marks must run after the member exists: `mark_non_overwriting_assignment_member`
    // readmits co-writers through the member's current owner, which is a no-op
    // before insertion, and `mark_resolved_member_assignment` requires the member
    // to exist. The explicit guarded mark preserves `MetaDefine` bootstrap
    // semantics where `mark_resolved_member_assignment` early-returns; the shared
    // call covers the conditional/loop-body policy.
    if guarded {
        db.get_member_index_mut()
            .mark_non_overwriting_assignment_member(member_id);
    }
    mark_resolved_member_assignment(db, member_id);
    Some(())
}

/// Whether `expr` is the declaration's own initializer rather than a later
/// assignment to it.
fn decl_expr_is_initializer(db: &DbIndex, decl_id: LuaDeclId, expr: &LuaExpr) -> bool {
    db.get_decl_index()
        .get_decl(&decl_id)
        .and_then(|decl| decl.get_initializer())
        .is_some_and(|initializer| {
            initializer.get_expr_syntax_id() == glua_parser::LuaSyntaxId::from_node(expr.syntax())
        })
}

fn should_defer_guarded_index_alias_resolution(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: &LuaExpr,
) -> bool {
    let Some(left) = guarded_index_or_empty_table_left(expr) else {
        return false;
    };

    match infer_expr(db, cache, left) {
        // A `nil`/`unknown` left defers only while the index may still produce
        // the member. Under the force phase a failed read floors to `nil`,
        // which is the settled answer rather than a placeholder — `nil or {}`
        // yields the fresh table, so the `or` must run instead of waiting on a
        // member that cannot become visible at this position (the common case
        // is `local x = T.f or {}` where `T.f` is only written later in the
        // same file). Deferring here retires the declaration with no type and
        // lets usage-context inference fabricate one.
        Ok(ty) => (ty.is_nil() || ty.is_unknown()) && !crate::semantic::floors_unsettled(db, cache),
        Err(reason) => reason.is_need_resolve(),
    }
}

fn guarded_index_or_empty_table_left(expr: &LuaExpr) -> Option<LuaExpr> {
    let LuaExpr::BinaryExpr(binary_expr) = expr else {
        return None;
    };
    if binary_expr.get_op_token().map(|op| op.get_op()) != Some(BinaryOperator::OpOr) {
        return None;
    }
    let (left, right) = binary_expr.get_exprs()?;
    if !matches!(left, LuaExpr::IndexExpr(_)) {
        return None;
    }
    if !matches!(right, LuaExpr::TableExpr(table_expr) if table_expr.is_empty()) {
        return None;
    }

    Some(left)
}

pub fn try_resolve_member(
    db: &DbIndex,
    writes: &mut ResolveWrites,
    cache: &mut LuaInferCache,
    unresolve_member: &mut UnResolveMember,
) -> ResolveResult {
    // Registers the member's derivation read before any deferred write.
    db.get_member_index()
        .get_member_owner(&unresolve_member.member_id);
    if let Some(prefix_expr) = &unresolve_member.prefix {
        let prefix_type = infer_expr(db, cache, prefix_expr.clone())?;
        let member_id = unresolve_member.member_id;
        // Ownership is decided by
        // `resolve_index_expr_member_owner_for_file`, the same function the
        // Lua pass uses when the prefix resolves immediately. This used to
        // be a separate `match` that only understood `TableConst`, `Def`
        // and `Instance`, so a prefix that inferred to a `MergedTable` (`SF
        // = SF or {}` written once per realm — the GLua norm), a `Union` or
        // a `Ref` fell through to the annotation-class fallback and the
        // member was left parked on its global path.
        let resolved_owner = resolve_index_expr_member_owner_for_file(
            db,
            &prefix_type,
            Some(unresolve_member.file_id),
        );
        let member_owner = match resolved_owner {
            Some((LuaMemberOwner::Type(def_id), set_owner_only)) => {
                let type_decl = db
                    .get_type_index()
                    .get_type_decl(&def_id)
                    .ok_or(InferFailReason::None)?;
                // if is exact type, no need to extend field
                if type_decl.is_exact() {
                    return Ok(());
                }
                Some((LuaMemberOwner::Type(def_id), set_owner_only))
            }
            Some(owner) => Some(owner),
            None => {
                if matches!(prefix_expr, LuaExpr::IndexExpr(_)) {
                    let dynamic_type = infer_dynamic_index_expr_shape(db, cache, prefix_expr)?;
                    let Some(owner) = resolve_index_expr_member_owner_for_file(
                        db,
                        &dynamic_type,
                        Some(unresolve_member.file_id),
                    ) else {
                        return Err(InferFailReason::FieldNotFound);
                    };
                    Some(owner)
                } else {
                    // Some annotation bundles define methods as `function TypeName:Method()`
                    // without binding a typed declaration for `TypeName` in scope. Expose those
                    // runtime members through a matching class without replacing their real owner.
                    //
                    // This is the last branch that can end without an owner, and a
                    // prefix that names no class reads exactly like one that has
                    // not settled: the lookup misses for the lowercase locals the
                    // real scripts use (`player._Sleeping`), and on a cold build
                    // the walk reaches the write before the prefix carries a type
                    // at all. Answering `Ok` there retires the member from every
                    // retry with nothing attached, which is what made its owner a
                    // function of how far the batch had run. Report it unresolved
                    // instead, so the caller keeps it queued until a pass that can
                    // still settle the prefix has run.
                    let LuaExpr::NameExpr(name_expr) = prefix_expr else {
                        return Err(InferFailReason::FieldNotFound);
                    };
                    let Some(name_token) = name_expr.get_name_token() else {
                        return Err(InferFailReason::FieldNotFound);
                    };
                    let type_decl_id = LuaTypeDeclId::global(name_token.get_name_text());
                    let Some(type_decl) = db.get_type_index().get_type_decl(&type_decl_id) else {
                        return Err(InferFailReason::FieldNotFound);
                    };
                    if !type_decl.is_class() {
                        return Err(InferFailReason::FieldNotFound);
                    }
                    let alias_owner = LuaMemberOwner::Type(type_decl_id);
                    writes.push(move |db, _| {
                        let _ = db
                            .get_member_index_mut()
                            .add_member_alias_to_owner(alias_owner, member_id);
                    });
                    None
                }
            }
        };
        // `set_owner_only` carries the same meaning it does in the Lua pass: a
        // `Ref` prefix names a declared class, so the member is re-homed onto it
        // but does not become one of its declared members.
        if let Some((member_owner, set_owner_only)) = member_owner {
            // A prefix that is a local alias of a known global path is a path
            // write for provenance purposes, so the member follows the path's
            // `---@class` flips. Only computed when the owner resolved to a
            // class: path and Element owners already carry their provenance
            // through the homing note.
            let alias_path = match &member_owner {
                LuaMemberOwner::Type(_) => {
                    alias_target_global_path(db, unresolve_member.file_id, prefix_expr)
                }
                _ => None,
            };
            // The whole gate + create + home block replays verbatim when the
            // write lands: `get_member` is re-asked against the live index,
            // so a member another write created in between is skipped exactly
            // as the serial attempt would skip it, and the member-key
            // inference inside `create_deferred_index_expr_member` reads the
            // state it would have read inline.
            writes.push({
                let prefix_type = prefix_type.clone();
                move |db, infer_manager| {
                    // The Lua pass creates a missing member before it looks at
                    // `set_owner_only`, so this must too: `set_member_owner`
                    // cannot re-home a member that does not exist, and a `Ref`
                    // prefix would otherwise leave the cold build with no
                    // member at all.
                    if db.get_member_index().get_member(&member_id).is_none() {
                        let cache = infer_manager.get_infer_cache(member_id.file_id);
                        create_deferred_index_expr_member(
                            db,
                            cache,
                            &prefix_type,
                            member_owner.clone(),
                            member_id,
                            alias_path.clone(),
                            set_owner_only,
                        );
                    }

                    if set_owner_only {
                        match alias_path {
                            Some(alias_path) => {
                                db.get_member_index_mut().home_alias_member_with_provenance(
                                    member_owner,
                                    member_id.file_id,
                                    member_id,
                                    alias_path,
                                    true,
                                );
                            }
                            None => {
                                db.get_member_index_mut().set_member_owner_only(
                                    member_owner,
                                    member_id.file_id,
                                    member_id,
                                );
                            }
                        }
                    } else {
                        match alias_path {
                            Some(alias_path) => {
                                db.get_member_index_mut().home_alias_member_with_provenance(
                                    member_owner,
                                    member_id.file_id,
                                    member_id,
                                    alias_path,
                                    false,
                                );
                            }
                            None => {
                                add_member(db, member_owner, member_id);
                            }
                        }
                    }
                }
            });
        }
        unresolve_member.prefix = None;
    }

    if let Some(expr) = unresolve_member.expr.clone() {
        let expr_type = match infer_expr(db, cache, expr.clone()) {
            Ok(typ) => typ,
            Err(reason) => {
                if reason.is_need_resolve()
                    && let Some(cached_type) =
                        cached_local_name_expr_type(db, unresolve_member.file_id, &expr)
                {
                    cached_type
                } else {
                    return Err(reason);
                }
            }
        };
        let expr_type = match &expr_type {
            LuaType::Variadic(multi) => multi
                .get_type(unresolve_member.ret_idx)
                .cloned()
                .unwrap_or(LuaType::Unknown),
            _ => expr_type,
        };

        if holds_unbound_iter_template(db, unresolve_member.file_id, &expr, &expr_type) {
            return Err(InferFailReason::UnResolveIterTemplate);
        }

        let member_id = unresolve_member.member_id;
        let type_owner: crate::LuaTypeOwner = member_id.into();
        writes.push(move |db, _| {
            bind_resolved_type(
                db,
                type_owner.clone(),
                LuaTypeCache::InferType(expr_type.clone()),
            );
        });
        writes.push(move |db, _| {
            crate::compilation::analyzer::lua::mark_resolved_member_assignment(db, member_id);
        });
    }

    Ok(())
}
fn infer_dynamic_index_expr_shape(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: &LuaExpr,
) -> Result<LuaType, InferFailReason> {
    let LuaExpr::IndexExpr(index_expr) = expr else {
        return Err(InferFailReason::FieldNotFound);
    };
    let prefix_expr = index_expr
        .get_prefix_expr()
        .ok_or(InferFailReason::FieldNotFound)?;
    let prefix_type = infer_expr(db, cache, prefix_expr)?;
    let index_key = index_expr
        .get_index_key()
        .ok_or(InferFailReason::FieldNotFound)?;
    let member_key = LuaMemberKey::from_index_key(db, cache, &index_key)?;
    resolve_dynamic_field_member(db, cache, &prefix_type, &member_key, None)
        .unwrap_or_default()
        .map(|resolution| resolution.typ)
        .ok_or(InferFailReason::FieldNotFound)
}

fn cached_local_name_expr_type(db: &DbIndex, file_id: FileId, expr: &LuaExpr) -> Option<LuaType> {
    let LuaExpr::NameExpr(name_expr) = expr else {
        return None;
    };
    let decl_id = db
        .get_reference_index()
        .get_local_reference(&file_id)?
        .get_decl_id(&name_expr.get_range())?;
    db.get_type_index()
        .get_type_cache(&decl_id.into())
        .map(|cache| cache.as_type().clone())
        .filter(local_cached_type_is_informative)
}

fn local_cached_type_is_informative(typ: &LuaType) -> bool {
    match typ {
        LuaType::Any | LuaType::Unknown | LuaType::Nil | LuaType::Never => false,
        LuaType::Union(union) => union.types().any(local_cached_type_is_informative),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .any(|(typ, _)| local_cached_type_is_informative(typ)),
        _ => true,
    }
}

pub fn try_resolve_module(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    module: &mut UnResolveModule,
) -> ResolveResult {
    let expr = module.expr.clone();
    let expr_type = infer_expr(db, cache, expr.clone())?;
    let expr_type = match &expr_type {
        LuaType::Variadic(multi) => multi.get_type(0).cloned().unwrap_or(LuaType::Unknown),
        _ => expr_type,
    };

    // Compute semantic_id for the exported expression using the shared helper
    let semantic_id = compute_module_semantic_id(db, module.file_id, &module.expr);

    let changed = db
        .get_module_index_mut()
        .set_export(module.file_id, expr_type, semantic_id)
        .ok_or(InferFailReason::None)?;
    // Re-settling an identical export moves nothing observable: only a novel
    // fact may bump the module epoch.
    if changed {
        db.get_module_index_mut().note_module_write(module.file_id);
    }
    Ok(())
}

pub fn try_resolve_return_point(
    db: &DbIndex,
    writes: &mut ResolveWrites,
    cache: &mut LuaInferCache,
    return_: &mut UnResolveReturn,
) -> ResolveResult {
    // Deriving a return means inferring every return expression in the
    // function, and `should_apply_resolved_return_docs` then discards the
    // result whenever the signature already holds a concrete inferred return —
    // it can only ever upgrade `unknown`/`any`. Asking that question first
    // costs two field reads instead of a full inference, and this pass
    // re-attempts the same signatures across waves.
    if let Some(signature) = db.get_signature_index().get(&return_.signature_id)
        && signature.resolve_return == SignatureReturnStatus::InferResolve
    {
        let current_return = signature.get_return_type();
        if !is_unsettled_inferred_return(&current_return) {
            return Ok(());
        }
    }

    let (return_correlations, return_docs) =
        derive_inferred_return_with_reads(db, cache, &return_.return_points);
    let return_docs = return_docs?;

    let inferred_return = return_docs_to_type(&return_docs);
    let inherited_tail = db
        .get_signature_index()
        .get(&return_.signature_id)
        .filter(|signature| {
            signature.resolve_return == SignatureReturnStatus::DocResolve
                && signature.return_docs.len() == 1
        })
        .and_then(|signature| {
            inferred_return_tail_matching_documented_first(
                &signature.return_docs[0].type_ref,
                &inferred_return,
                |slot| {
                    return_correlations.iter().any(|correlation| {
                        correlation.discriminant_slot == 0
                            && correlation.implied_non_nil_slots.contains(&slot)
                    })
                },
            )
        });

    let signature_id = return_.signature_id;
    if db.get_signature_index().get(&signature_id).is_none() {
        return Err(InferFailReason::None);
    }

    // The mutation reads the slot's live `resolve_return`/`return_docs` when
    // it lands, so the deferred closure replays the whole decision verbatim —
    // the same state the serial attempt would have mutated in place.
    writes.push(move |db, _| {
        let Some(signature) = db.get_signature_index_mut().get_mut(&signature_id) else {
            return;
        };

        if let Some(inherited_tail) = inherited_tail {
            signature
                .return_docs
                .extend(inherited_tail.into_iter().map(|type_ref| LuaDocReturnInfo {
                    name: None,
                    type_ref,
                    default_value: None,
                    description: None,
                    attributes: None,
                    return_kind: ReturnTypeKind::default(),
                }));
            signature.set_return_correlations(return_correlations);
            db.get_signature_index_mut().note_return_write(signature_id);
            return;
        }

        if should_apply_resolved_return_docs(signature, &return_docs) {
            let moved = signature
                .return_docs
                .iter()
                .map(|doc| &doc.type_ref)
                .ne(return_docs.iter().map(|doc| &doc.type_ref));
            let correlations_moved =
                signature.return_correlations() != return_correlations.as_slice();
            signature.resolve_return = SignatureReturnStatus::InferResolve;
            signature.return_docs = return_docs;
            signature.set_return_correlations(return_correlations);
            if moved || correlations_moved {
                db.get_signature_index_mut().note_return_write(signature_id);
            }
        }
    });

    Ok(())
}

fn should_apply_resolved_return_docs(
    signature: &LuaSignature,
    return_docs: &[LuaDocReturnInfo],
) -> bool {
    let current_return = signature.get_return_type();
    let new_return = return_docs_to_type(return_docs);

    if signature.resolve_return == SignatureReturnStatus::UnResolve {
        return true;
    }

    if signature.resolve_return != SignatureReturnStatus::InferResolve {
        return false;
    }

    inferred_return_improves(&current_return, &new_return)
}

/// An inferred return that says nothing yet. `never` belongs here: a function
/// whose body returns an expression cannot return no value, so a `never`
/// return is an operand that had not resolved when the body was read.
pub(crate) fn is_unsettled_inferred_return(return_type: &LuaType) -> bool {
    match return_type {
        LuaType::Unknown | LuaType::Any | LuaType::Never => true,
        LuaType::Variadic(variadic) => match variadic.as_ref() {
            VariadicType::Multi(types) => types.iter().any(is_unsettled_inferred_return),
            VariadicType::Base(typ) => is_unsettled_inferred_return(typ),
        },
        // `T|nil` says as much as `T` does: a nullable wrapper around an
        // unsettled slot is still an unsettled answer.
        LuaType::Union(union) => union
            .nullable_inner()
            .is_some_and(is_unsettled_inferred_return),
        _ => false,
    }
}

fn inferred_return_improves(current: &LuaType, new: &LuaType) -> bool {
    if current == new {
        return false;
    }
    if current.is_unknown() && new.is_any() {
        return true;
    }
    match (current, new) {
        (LuaType::Unknown | LuaType::Any | LuaType::Never, LuaType::Union(union))
            if union.nullable_inner() == Some(current) =>
        {
            true
        }
        (LuaType::Unknown | LuaType::Any | LuaType::Never, _) => !is_unsettled_inferred_return(new),
        (LuaType::Variadic(current_variadic), LuaType::Variadic(new_variadic)) => {
            match (current_variadic.as_ref(), new_variadic.as_ref()) {
                (VariadicType::Multi(current_types), VariadicType::Multi(new_types)) => {
                    current_types.len() == new_types.len()
                        && current_types.iter().zip(new_types.iter()).all(
                            |(current_slot, new_slot)| {
                                current_slot == new_slot
                                    || inferred_return_improves(current_slot, new_slot)
                            },
                        )
                }
                (VariadicType::Base(current_type), VariadicType::Base(new_type)) => {
                    inferred_return_improves(current_type, new_type)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn return_docs_to_type(return_docs: &[LuaDocReturnInfo]) -> LuaType {
    match return_docs.len() {
        0 => LuaType::Nil,
        1 => return_docs[0].type_ref.clone(),
        _ => LuaType::Variadic(
            VariadicType::Multi(
                return_docs
                    .iter()
                    .map(|info| info.type_ref.clone())
                    .collect(),
            )
            .into(),
        ),
    }
}

/// A resolved iterator-variable type plus the write mode it earned.
pub struct IterVarTypeUpdate {
    pub(crate) owner: LuaTypeOwner,
    pub(crate) cache: LuaTypeCache,
    pub(crate) mode: TypeCacheWriteMode,
}

/// The loop variables' types from `iter_var_types`: template arms an unbound
/// generic left are dropped, and proven overrides win.
pub(crate) fn iter_var_updates(
    db: &DbIndex,
    file_id: FileId,
    iter_var_types: &IterVarInference,
    var_positions: &[TextSize],
) -> Vec<IterVarTypeUpdate> {
    let mut updates = Vec::with_capacity(var_positions.len());
    for (idx, &position) in var_positions.iter().enumerate() {
        let decl_id = LuaDeclId::new(file_id, position);
        let ret_type = iter_var_types
            .get_type(idx)
            .cloned()
            .unwrap_or(LuaType::Unknown);
        let ret_type = TypeOps::Remove.apply(db, &ret_type, &LuaType::Nil);
        // A raw template ref is the placeholder an unbound generic leaves for
        // an arm it could not see into, not a type the variable holds, so the
        // arms that did bind are the answer. A placeholder alone stays as it
        // is: it is what marks the variable as still waiting.
        let ret_type = match &ret_type {
            LuaType::Union(union)
                if ret_type.contain_tpl() && union.types().any(|arm| !arm.contain_tpl()) =>
            {
                LuaType::from_vec(
                    union
                        .types()
                        .filter(|arm| !arm.contain_tpl())
                        .cloned()
                        .collect(),
                )
            }
            _ => ret_type,
        };

        let ret_type = if let Some(Some(proven)) = iter_var_types.settled_overrides.get(idx) {
            proven.clone()
        } else {
            ret_type
        };

        let owner: LuaTypeOwner = decl_id.into();
        let cached = db.get_type_index().get_type_cache(&owner);
        let mode = iter_var_write_mode(cached, &ret_type);
        updates.push(IterVarTypeUpdate {
            owner,
            cache: LuaTypeCache::InferType(ret_type),
            mode,
        });
    }
    updates
}

/// The write mode for a settled iterator-variable type.
///
/// A raw template ref is a placeholder left by an unbound generic, never a valid
/// fact, so a real type replaces it, as does a settled union that already
/// contains everything the cache holds. A documented type is an authority
/// decision — `---@param` is legal on a `for ... in` variable — so it is never
/// overwritten; anything else keeps insert-only precedence.
fn iter_var_write_mode(cached: Option<&LuaTypeCache>, settled: &LuaType) -> TypeCacheWriteMode {
    let Some(cached) = cached.filter(|cached| !cached.is_doc()) else {
        return TypeCacheWriteMode::InsertOnly;
    };
    if !settled.contain_tpl()
        && (cached.as_type().contain_tpl()
            || crate::compilation::analyzer::union_widens_cached_type(settled, cached.as_type()))
    {
        TypeCacheWriteMode::ForceOverwrite
    } else {
        TypeCacheWriteMode::InsertOnly
    }
}

pub fn try_resolve_special_call(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    unresolve_special_call: &mut UnResolveSpecialCall,
) -> ResolveResult {
    let call_expr = unresolve_special_call.call_expr.clone();
    let prefix_expr = call_expr.get_prefix_expr().ok_or(InferFailReason::None)?;
    let callable_param_infos = collect_special_call_param_infos_for_prefix(
        db,
        cache,
        unresolve_special_call.file_id,
        call_expr.get_position(),
        &call_expr,
        &prefix_expr,
    )?;
    let callable_out_param_infos = collect_special_call_out_param_infos_for_prefix(
        db,
        cache,
        unresolve_special_call.file_id,
        call_expr.get_position(),
        &call_expr,
        &prefix_expr,
    )?;
    if callable_param_infos.is_empty() && callable_out_param_infos.is_empty() {
        return Ok(());
    }

    let is_colon_call = unresolve_special_call.call_expr.is_colon_call();
    for param_info in callable_param_infos {
        materialize_str_tpl_class_from_call(
            db,
            cache,
            unresolve_special_call.file_id,
            &unresolve_special_call.call_expr,
            param_info.param_idx,
            &param_info.param_type,
            param_info.is_colon_define,
            is_colon_call,
        )?;

        if param_info.is_constructor {
            try_resolve_constructor_param(
                db,
                cache,
                unresolve_special_call.file_id,
                &unresolve_special_call.call_expr,
                &param_info,
            )?;
        }
    }

    for out_param_info in callable_out_param_infos {
        apply_out_param_from_call(
            db,
            cache,
            unresolve_special_call.file_id,
            &unresolve_special_call.call_expr,
            &out_param_info,
            is_colon_call,
        )?;
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct SpecialCallParamInfo {
    param_idx: usize,
    param_type: LuaType,
    is_constructor: bool,
    is_colon_define: bool,
    signature_id: Option<LuaSignatureId>,
}

#[derive(Debug, Clone)]
struct SpecialCallOutParamInfo {
    root: LuaOutParamRoot,
    field_path: Vec<String>,
    type_ref: LuaType,
    is_colon_define: bool,
}

#[derive(Debug, Clone)]
struct ResolvedOutParamTarget {
    owner: Option<LuaTypeOwner>,
    value_expr: Option<LuaExpr>,
}

fn collect_special_call_param_infos_for_prefix(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    caller_file_id: FileId,
    caller_position: rowan::TextSize,
    call_expr: &LuaCallExpr,
    prefix_expr: &LuaExpr,
) -> Result<Vec<SpecialCallParamInfo>, InferFailReason> {
    let mut visited_wrapped_decls = HashSet::new();
    collect_special_call_param_infos_for_prefix_inner(
        db,
        cache,
        caller_file_id,
        caller_position,
        call_expr,
        prefix_expr,
        &mut visited_wrapped_decls,
    )
}

fn collect_special_call_param_infos_for_prefix_inner(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    caller_file_id: FileId,
    caller_position: rowan::TextSize,
    call_expr: &LuaCallExpr,
    prefix_expr: &LuaExpr,
    visited_wrapped_decls: &mut HashSet<LuaSemanticDeclId>,
) -> Result<Vec<SpecialCallParamInfo>, InferFailReason> {
    let semantic_decl = try_infer_expr_semantic_decl(
        db,
        cache,
        prefix_expr.clone(),
        SemanticDeclGuard::default(),
        SemanticDeclLevel::default(),
    )?;

    if let Some(semantic_decl) = semantic_decl {
        let param_infos =
            collect_special_call_param_infos_from_semantic_decl(db, semantic_decl.clone())?;
        if !param_infos.is_empty() {
            return Ok(param_infos);
        }

        if visited_wrapped_decls.insert(semantic_decl.clone()) {
            if let Some(target_expr) = get_wrapped_callable_target_expr(db, semantic_decl) {
                let param_infos = collect_special_call_param_infos_for_prefix_inner(
                    db,
                    cache,
                    caller_file_id,
                    caller_position,
                    call_expr,
                    &target_expr,
                    visited_wrapped_decls,
                )?;
                if !param_infos.is_empty() {
                    return Ok(param_infos);
                }
            }
        }
    }

    let callable_type = infer_expr(db, cache, prefix_expr.clone())?;
    let param_infos = collect_special_call_param_infos(db, &callable_type);
    if !param_infos.is_empty() {
        return Ok(param_infos);
    }

    let operator_collection = collect_special_call_param_infos_from_callable_operators(
        db,
        caller_file_id,
        caller_position,
        &callable_type,
    );
    if operator_collection.had_operators {
        return Ok(operator_collection.param_infos);
    }

    let call_func = infer_call_expr_func(
        db,
        cache,
        call_expr.clone(),
        callable_type,
        &InferGuard::new(),
        None,
    )?;
    Ok(collect_doc_function_special_call_params(call_func.as_ref()))
}

fn collect_special_call_out_param_infos_for_prefix(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    caller_file_id: FileId,
    caller_position: rowan::TextSize,
    call_expr: &LuaCallExpr,
    prefix_expr: &LuaExpr,
) -> Result<Vec<SpecialCallOutParamInfo>, InferFailReason> {
    let mut visited_wrapped_decls = HashSet::new();
    collect_special_call_out_param_infos_for_prefix_inner(
        db,
        cache,
        caller_file_id,
        caller_position,
        call_expr,
        prefix_expr,
        &mut visited_wrapped_decls,
    )
}

fn collect_special_call_out_param_infos_for_prefix_inner(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    caller_file_id: FileId,
    caller_position: rowan::TextSize,
    _call_expr: &LuaCallExpr,
    prefix_expr: &LuaExpr,
    visited_wrapped_decls: &mut HashSet<LuaSemanticDeclId>,
) -> Result<Vec<SpecialCallOutParamInfo>, InferFailReason> {
    let semantic_decl = try_infer_expr_semantic_decl(
        db,
        cache,
        prefix_expr.clone(),
        SemanticDeclGuard::default(),
        SemanticDeclLevel::default(),
    )?;

    if let Some(semantic_decl) = semantic_decl {
        let out_params =
            collect_special_call_out_param_infos_from_semantic_decl(db, semantic_decl.clone())?;
        if !out_params.is_empty() {
            return Ok(out_params);
        }

        if visited_wrapped_decls.insert(semantic_decl.clone()) {
            if let Some(target_expr) = get_wrapped_callable_target_expr(db, semantic_decl) {
                let out_params = collect_special_call_out_param_infos_for_prefix_inner(
                    db,
                    cache,
                    caller_file_id,
                    caller_position,
                    _call_expr,
                    &target_expr,
                    visited_wrapped_decls,
                )?;
                if !out_params.is_empty() {
                    return Ok(out_params);
                }
            }
        }
    }

    let callable_type = infer_expr(db, cache, prefix_expr.clone())?;
    let out_params = collect_special_call_out_param_infos(db, &callable_type);
    if !out_params.is_empty() {
        return Ok(out_params);
    }

    let operator_collection = collect_special_call_out_param_infos_from_callable_operators(
        db,
        caller_file_id,
        caller_position,
        &callable_type,
    );
    if operator_collection.had_operators {
        return Ok(operator_collection.out_param_infos);
    }

    Ok(Vec::new())
}

pub(crate) fn get_wrapped_callable_target_expr(
    db: &DbIndex,
    semantic_decl: LuaSemanticDeclId,
) -> Option<LuaExpr> {
    let LuaExpr::CallExpr(call_expr) = semantic_decl_value_expr(db, semantic_decl)? else {
        return None;
    };
    get_setmetatable_call_target_expr(&call_expr)
}

pub(crate) fn get_setmetatable_call_target_expr(call_expr: &LuaCallExpr) -> Option<LuaExpr> {
    let LuaExpr::NameExpr(name_expr) = call_expr.get_prefix_expr()? else {
        return None;
    };
    if name_expr.get_name_text()? != "setmetatable" {
        return None;
    }

    let args = call_expr.get_args_list()?.get_args().collect::<Vec<_>>();
    let LuaExpr::TableExpr(metatable) = args.get(1)?.clone() else {
        return None;
    };

    metatable.get_fields().find_map(|field| {
        let field_name = match field.get_field_key()? {
            glua_parser::LuaIndexKey::Name(name) => name.get_name_text().to_string(),
            glua_parser::LuaIndexKey::String(string) => string.get_value(),
            _ => return None,
        };
        if field_name != "__call" {
            return None;
        }

        match field.get_value_expr()? {
            LuaExpr::NameExpr(name_expr) => Some(LuaExpr::NameExpr(name_expr)),
            LuaExpr::IndexExpr(index_expr) => Some(LuaExpr::IndexExpr(index_expr)),
            _ => None,
        }
    })
}

fn signature_has_overload_special_call_params(signature: &LuaSignature) -> bool {
    signature
        .overloads
        .iter()
        .any(|overload| overload_has_special_call_params(overload))
}

fn overload_has_special_call_params(func: &LuaFunctionType) -> bool {
    func.get_params().iter().any(|(_, param_type)| {
        param_type
            .as_ref()
            .map(type_contains_str_tpl_ref)
            .unwrap_or(false)
    })
}

fn collect_signature_overload_special_call_params(
    signature: &LuaSignature,
) -> Vec<SpecialCallParamInfo> {
    signature
        .overloads
        .iter()
        .flat_map(|overload| collect_doc_function_special_call_params(overload))
        .collect()
}

fn signature_has_any_special_call_params(signature: &LuaSignature) -> bool {
    signature.has_special_call_params() || signature_has_overload_special_call_params(signature)
}

fn collect_special_call_param_infos_from_callable_operators(
    db: &DbIndex,
    caller_file_id: FileId,
    caller_position: rowan::TextSize,
    callable_type: &LuaType,
) -> SpecialCallOperatorCollection {
    match callable_type {
        LuaType::TableConst(in_file_range) => db
            .get_metatable_index()
            .get(in_file_range)
            .map(|meta_table| {
                collect_special_call_param_infos_from_operator_owner(
                    db,
                    caller_file_id,
                    caller_position,
                    &LuaOperatorOwner::Table(meta_table.clone()),
                )
            })
            .unwrap_or_default(),
        LuaType::Def(type_decl_id) | LuaType::Ref(type_decl_id) => {
            collect_special_call_param_infos_from_operator_owner(
                db,
                caller_file_id,
                caller_position,
                &LuaOperatorOwner::Type(type_decl_id.clone()),
            )
        }
        LuaType::Instance(instance) => collect_special_call_param_infos_from_callable_operators(
            db,
            caller_file_id,
            caller_position,
            instance.get_base(),
        ),
        LuaType::TypeGuard(inner) => collect_special_call_param_infos_from_callable_operators(
            db,
            caller_file_id,
            caller_position,
            inner,
        ),
        LuaType::Union(union) => union.types().fold(
            SpecialCallOperatorCollection::default(),
            |mut collection, union_type| {
                collection.extend(collect_special_call_param_infos_from_callable_operators(
                    db,
                    caller_file_id,
                    caller_position,
                    union_type,
                ));
                collection
            },
        ),
        LuaType::Intersection(intersection) => intersection.get_types().iter().fold(
            SpecialCallOperatorCollection::default(),
            |mut collection, intersection_type| {
                collection.extend(collect_special_call_param_infos_from_callable_operators(
                    db,
                    caller_file_id,
                    caller_position,
                    intersection_type,
                ));
                collection
            },
        ),
        LuaType::MultiLineUnion(union) => union.get_unions().iter().fold(
            SpecialCallOperatorCollection::default(),
            |mut collection, (union_type, _)| {
                collection.extend(collect_special_call_param_infos_from_callable_operators(
                    db,
                    caller_file_id,
                    caller_position,
                    union_type,
                ));
                collection
            },
        ),
        _ => SpecialCallOperatorCollection::default(),
    }
}

fn collect_special_call_param_infos_from_operator_owner(
    db: &DbIndex,
    caller_file_id: FileId,
    caller_position: rowan::TextSize,
    owner: &LuaOperatorOwner,
) -> SpecialCallOperatorCollection {
    let Some(operator_ids) = db
        .get_operator_index()
        .get_operators(owner, LuaOperatorMetaMethod::Call)
    else {
        return SpecialCallOperatorCollection::default();
    };

    let priority_tiers = get_operator_id_priority_tiers(db, caller_file_id, operator_ids);
    let visible_operator_ids = select_operator_ids_by_workspace_and_realm(
        db,
        caller_file_id,
        caller_position,
        priority_tiers,
    );

    let param_infos = visible_operator_ids
        .iter()
        .flat_map(|operator_id| {
            let Some(operator) = db.get_operator_index().get_operator(operator_id) else {
                return Vec::new();
            };

            match operator.get_operator_func(db) {
                LuaType::Signature(signature_id) => db
                    .get_signature_index()
                    .get(&signature_id)
                    .map(|signature| {
                        adjust_operator_special_call_param_infos(
                            collect_signature_special_call_params(signature, signature_id),
                            should_strip_first_operator_param(signature.is_colon_define, owner),
                        )
                    })
                    .unwrap_or_default(),
                LuaType::DocFunction(func) => adjust_operator_special_call_param_infos(
                    collect_doc_function_special_call_params(func.as_ref()),
                    should_strip_first_operator_param(func.is_colon_define(), owner),
                ),
                _ => Vec::new(),
            }
        })
        .collect();

    SpecialCallOperatorCollection {
        param_infos,
        had_operators: true,
    }
}

#[derive(Debug, Default)]
struct SpecialCallOperatorCollection {
    param_infos: Vec<SpecialCallParamInfo>,
    had_operators: bool,
}

impl SpecialCallOperatorCollection {
    fn extend(&mut self, other: SpecialCallOperatorCollection) {
        self.had_operators |= other.had_operators;
        self.param_infos.extend(other.param_infos);
    }
}

fn collect_special_call_out_param_infos_from_callable_operators(
    db: &DbIndex,
    caller_file_id: FileId,
    caller_position: rowan::TextSize,
    callable_type: &LuaType,
) -> SpecialCallOutParamOperatorCollection {
    match callable_type {
        LuaType::TableConst(in_file_range) => db
            .get_metatable_index()
            .get(in_file_range)
            .map(|meta_table| {
                collect_special_call_out_param_infos_from_operator_owner(
                    db,
                    caller_file_id,
                    caller_position,
                    &LuaOperatorOwner::Table(meta_table.clone()),
                )
            })
            .unwrap_or_default(),
        LuaType::Def(type_decl_id) | LuaType::Ref(type_decl_id) => {
            collect_special_call_out_param_infos_from_operator_owner(
                db,
                caller_file_id,
                caller_position,
                &LuaOperatorOwner::Type(type_decl_id.clone()),
            )
        }
        LuaType::Instance(instance) => {
            collect_special_call_out_param_infos_from_callable_operators(
                db,
                caller_file_id,
                caller_position,
                instance.get_base(),
            )
        }
        LuaType::TypeGuard(inner) => collect_special_call_out_param_infos_from_callable_operators(
            db,
            caller_file_id,
            caller_position,
            inner,
        ),
        LuaType::Union(union) => union.types().fold(
            SpecialCallOutParamOperatorCollection::default(),
            |mut collection, union_type| {
                collection.extend(
                    collect_special_call_out_param_infos_from_callable_operators(
                        db,
                        caller_file_id,
                        caller_position,
                        union_type,
                    ),
                );
                collection
            },
        ),
        LuaType::Intersection(intersection) => intersection.get_types().iter().fold(
            SpecialCallOutParamOperatorCollection::default(),
            |mut collection, intersection_type| {
                collection.extend(
                    collect_special_call_out_param_infos_from_callable_operators(
                        db,
                        caller_file_id,
                        caller_position,
                        intersection_type,
                    ),
                );
                collection
            },
        ),
        LuaType::MultiLineUnion(union) => union.get_unions().iter().fold(
            SpecialCallOutParamOperatorCollection::default(),
            |mut collection, (union_type, _)| {
                collection.extend(
                    collect_special_call_out_param_infos_from_callable_operators(
                        db,
                        caller_file_id,
                        caller_position,
                        union_type,
                    ),
                );
                collection
            },
        ),
        _ => SpecialCallOutParamOperatorCollection::default(),
    }
}

fn collect_special_call_out_param_infos_from_operator_owner(
    db: &DbIndex,
    caller_file_id: FileId,
    caller_position: rowan::TextSize,
    owner: &LuaOperatorOwner,
) -> SpecialCallOutParamOperatorCollection {
    let Some(operator_ids) = db
        .get_operator_index()
        .get_operators(owner, LuaOperatorMetaMethod::Call)
    else {
        return SpecialCallOutParamOperatorCollection::default();
    };

    let priority_tiers = get_operator_id_priority_tiers(db, caller_file_id, operator_ids);
    let visible_operator_ids = select_operator_ids_by_workspace_and_realm(
        db,
        caller_file_id,
        caller_position,
        priority_tiers,
    );

    let out_param_infos = visible_operator_ids
        .iter()
        .flat_map(|operator_id| {
            let Some(operator) = db.get_operator_index().get_operator(operator_id) else {
                return Vec::new();
            };

            match operator.get_operator_func(db) {
                LuaType::Signature(signature_id) => db
                    .get_signature_index()
                    .get(&signature_id)
                    .map(|signature| {
                        adjust_operator_special_call_out_param_infos(
                            collect_signature_special_call_out_params(signature),
                            should_strip_first_operator_param(signature.is_colon_define, owner),
                        )
                    })
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
        })
        .collect();

    SpecialCallOutParamOperatorCollection {
        out_param_infos,
        had_operators: true,
    }
}

#[derive(Debug, Default)]
struct SpecialCallOutParamOperatorCollection {
    out_param_infos: Vec<SpecialCallOutParamInfo>,
    had_operators: bool,
}

impl SpecialCallOutParamOperatorCollection {
    fn extend(&mut self, other: SpecialCallOutParamOperatorCollection) {
        self.had_operators |= other.had_operators;
        self.out_param_infos.extend(other.out_param_infos);
    }
}

fn get_operator_id_priority_tiers(
    db: &DbIndex,
    caller_file_id: FileId,
    operator_ids: &[crate::LuaOperatorId],
) -> Vec<(u8, Vec<crate::LuaOperatorId>)> {
    let module_index = db.get_module_index();
    let Some(caller_workspace_id) = module_index.get_workspace_id(caller_file_id) else {
        return vec![(0, operator_ids.to_vec())];
    };

    let mut priority_tiers = BTreeMap::new();
    for operator_id in operator_ids {
        let candidate_workspace_id = module_index
            .get_workspace_id(operator_id.file_id)
            .unwrap_or(crate::WorkspaceId::MAIN);
        let Some(priority) =
            module_index.workspace_resolution_priority(caller_workspace_id, candidate_workspace_id)
        else {
            continue;
        };

        priority_tiers
            .entry(priority)
            .or_insert_with(Vec::new)
            .push(*operator_id);
    }

    priority_tiers.into_iter().collect()
}

fn select_operator_ids_by_workspace_and_realm(
    db: &DbIndex,
    caller_file_id: FileId,
    caller_position: rowan::TextSize,
    priority_tiers: Vec<(u8, Vec<crate::LuaOperatorId>)>,
) -> Vec<crate::LuaOperatorId> {
    let fallback_operator_ids = priority_tiers
        .first()
        .map(|(_, operator_ids)| operator_ids.clone())
        .unwrap_or_default();

    if !db.get_emmyrc().gmod.enabled {
        return fallback_operator_ids;
    }

    let infer_index = db.get_gmod_infer_index();
    let caller_mask = infer_index.get_state_mask_at_offset(&caller_file_id, caller_position);
    for (_, tier_operator_ids) in priority_tiers {
        let compatible_operator_ids = tier_operator_ids
            .into_iter()
            .filter(|operator_id| {
                let operator_mask = infer_index
                    .get_state_mask_at_offset(&operator_id.file_id, operator_id.position);
                caller_mask.is_compatible_with(operator_mask)
            })
            .collect::<Vec<_>>();
        if !compatible_operator_ids.is_empty() {
            return compatible_operator_ids;
        }
    }

    fallback_operator_ids
}

fn should_strip_first_operator_param(is_colon_define: bool, owner: &LuaOperatorOwner) -> bool {
    matches!(owner, LuaOperatorOwner::Type(_)) && !is_colon_define
}

fn adjust_operator_special_call_param_infos(
    param_infos: Vec<SpecialCallParamInfo>,
    strip_first_param: bool,
) -> Vec<SpecialCallParamInfo> {
    if !strip_first_param {
        return param_infos;
    }

    param_infos
        .into_iter()
        .filter_map(|mut param_info| {
            param_info.param_idx = param_info.param_idx.checked_sub(1)?;
            param_info.is_colon_define = false;
            Some(param_info)
        })
        .collect()
}

fn adjust_operator_special_call_out_param_infos(
    out_param_infos: Vec<SpecialCallOutParamInfo>,
    strip_first_param: bool,
) -> Vec<SpecialCallOutParamInfo> {
    if !strip_first_param {
        return out_param_infos;
    }

    out_param_infos
        .into_iter()
        .filter_map(|mut out_param_info| {
            let LuaOutParamRoot::Param(param_idx) = out_param_info.root else {
                return Some(out_param_info);
            };
            out_param_info.root = LuaOutParamRoot::Param(param_idx.checked_sub(1)?);
            out_param_info.is_colon_define = false;
            Some(out_param_info)
        })
        .collect()
}

fn collect_special_call_param_infos_from_semantic_decl(
    db: &DbIndex,
    semantic_decl: LuaSemanticDeclId,
) -> Result<Vec<SpecialCallParamInfo>, InferFailReason> {
    match semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            let type_cache = db
                .get_type_index()
                .get_type_cache(&decl_id.into())
                .ok_or(InferFailReason::UnResolveDeclType(decl_id))?;
            Ok(collect_special_call_param_infos(db, type_cache.as_type()))
        }
        LuaSemanticDeclId::Member(member_id) => {
            let type_cache = db
                .get_type_index()
                .get_type_cache(&member_id.into())
                .ok_or(InferFailReason::UnResolveMemberType(member_id))?;
            Ok(collect_special_call_param_infos(db, type_cache.as_type()))
        }
        LuaSemanticDeclId::Signature(signature_id) => Ok(db
            .get_signature_index()
            .get(&signature_id)
            .filter(|signature| signature_has_any_special_call_params(signature))
            .map(|signature| collect_signature_special_call_params(signature, signature_id))
            .unwrap_or_default()),
        LuaSemanticDeclId::TypeDecl(_) => Ok(Vec::new()),
    }
}

fn collect_special_call_out_param_infos_from_semantic_decl(
    db: &DbIndex,
    semantic_decl: LuaSemanticDeclId,
) -> Result<Vec<SpecialCallOutParamInfo>, InferFailReason> {
    match semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            if let Some(signature_id) = db
                .get_property_index()
                .get_signature_owner(&LuaSemanticDeclId::LuaDecl(decl_id))
            {
                let out_params = db
                    .get_signature_index()
                    .get(&signature_id)
                    .map(collect_signature_special_call_out_params)
                    .unwrap_or_default();
                if !out_params.is_empty() {
                    return Ok(out_params);
                }
            }
            if let Some(signature_id) =
                semantic_decl_closure_signature_id(db, LuaSemanticDeclId::LuaDecl(decl_id))
            {
                let out_params = db
                    .get_signature_index()
                    .get(&signature_id)
                    .map(collect_signature_special_call_out_params)
                    .unwrap_or_default();
                if !out_params.is_empty() {
                    return Ok(out_params);
                }
            }
            let type_cache = db
                .get_type_index()
                .get_type_cache(&decl_id.into())
                .ok_or(InferFailReason::UnResolveDeclType(decl_id))?;
            Ok(collect_special_call_out_param_infos(
                db,
                type_cache.as_type(),
            ))
        }
        LuaSemanticDeclId::Member(member_id) => {
            if let Some(signature_id) = db
                .get_property_index()
                .get_signature_owner(&LuaSemanticDeclId::Member(member_id))
            {
                let out_params = db
                    .get_signature_index()
                    .get(&signature_id)
                    .map(collect_signature_special_call_out_params)
                    .unwrap_or_default();
                if !out_params.is_empty() {
                    return Ok(out_params);
                }
            }
            if let Some(signature_id) =
                semantic_decl_closure_signature_id(db, LuaSemanticDeclId::Member(member_id))
            {
                let out_params = db
                    .get_signature_index()
                    .get(&signature_id)
                    .map(collect_signature_special_call_out_params)
                    .unwrap_or_default();
                if !out_params.is_empty() {
                    return Ok(out_params);
                }
            }

            let fallback_out_params =
                collect_same_member_key_special_call_out_params(db, member_id);
            if !fallback_out_params.is_empty() {
                return Ok(fallback_out_params);
            }

            let type_cache = db
                .get_type_index()
                .get_type_cache(&member_id.into())
                .ok_or(InferFailReason::UnResolveMemberType(member_id))?;
            Ok(collect_special_call_out_param_infos(
                db,
                type_cache.as_type(),
            ))
        }
        LuaSemanticDeclId::Signature(signature_id) => Ok(db
            .get_signature_index()
            .get(&signature_id)
            .filter(|signature| signature_has_any_special_call_params(signature))
            .map(collect_signature_special_call_out_params)
            .unwrap_or_default()),
        LuaSemanticDeclId::TypeDecl(_) => Ok(Vec::new()),
    }
}

fn collect_same_member_key_special_call_out_params(
    db: &DbIndex,
    current_member_id: LuaMemberId,
) -> Vec<SpecialCallOutParamInfo> {
    let member_index = db.get_member_index();
    let Some(owner) = member_index.get_current_owner(&current_member_id) else {
        return Vec::new();
    };
    let Some(current_member) = member_index.get_member(&current_member_id) else {
        return Vec::new();
    };
    let key = current_member.get_key();

    member_index
        .get_current_owner_members_for_key(owner, key)
        .into_iter()
        .filter(|member| member.get_id() != current_member_id)
        .filter_map(|member| db.get_type_index().get_type_cache(&member.get_id().into()))
        .flat_map(|type_cache| collect_special_call_out_param_infos(db, type_cache.as_type()))
        .collect()
}

fn collect_special_call_param_infos(
    db: &DbIndex,
    callable_type: &LuaType,
) -> Vec<SpecialCallParamInfo> {
    match callable_type {
        LuaType::Signature(signature_id) => db
            .get_signature_index()
            .get(signature_id)
            .filter(|signature| signature_has_any_special_call_params(signature))
            .map(|signature| collect_signature_special_call_params(signature, *signature_id))
            .unwrap_or_default(),
        LuaType::DocFunction(func) => collect_doc_function_special_call_params(func),
        LuaType::TypeGuard(inner) => collect_special_call_param_infos(db, inner),
        LuaType::Union(union) => union
            .types()
            .flat_map(|union_type| collect_special_call_param_infos(db, union_type))
            .collect(),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .flat_map(|intersection_type| collect_special_call_param_infos(db, intersection_type))
            .collect(),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .flat_map(|(union_type, _)| collect_special_call_param_infos(db, union_type))
            .collect(),
        _ => Vec::new(),
    }
}

fn collect_special_call_out_param_infos(
    db: &DbIndex,
    callable_type: &LuaType,
) -> Vec<SpecialCallOutParamInfo> {
    match callable_type {
        LuaType::Signature(signature_id) => db
            .get_signature_index()
            .get(signature_id)
            .filter(|signature| signature_has_any_special_call_params(signature))
            .map(collect_signature_special_call_out_params)
            .unwrap_or_default(),
        LuaType::TypeGuard(inner) => collect_special_call_out_param_infos(db, inner),
        LuaType::Union(union) => union
            .types()
            .flat_map(|union_type| collect_special_call_out_param_infos(db, union_type))
            .collect(),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .flat_map(|intersection_type| {
                collect_special_call_out_param_infos(db, intersection_type)
            })
            .collect(),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .flat_map(|(union_type, _)| collect_special_call_out_param_infos(db, union_type))
            .collect(),
        _ => Vec::new(),
    }
}

fn collect_signature_special_call_params(
    signature: &LuaSignature,
    signature_id: LuaSignatureId,
) -> Vec<SpecialCallParamInfo> {
    let mut param_infos = Vec::new();
    for (idx, param_info) in &signature.param_docs {
        let is_constructor = param_info.get_attribute_by_name("constructor").is_some();
        let has_str_tpl = type_contains_str_tpl_ref(&param_info.type_ref);
        if is_constructor || has_str_tpl {
            param_infos.push(SpecialCallParamInfo {
                param_idx: *idx,
                param_type: param_info.type_ref.clone(),
                is_constructor,
                is_colon_define: signature.is_colon_define,
                signature_id: Some(signature_id),
            });
        }
    }

    param_infos.extend(collect_signature_overload_special_call_params(signature));

    param_infos.sort_by_key(|param_info| param_info.param_idx);
    param_infos
}

fn collect_signature_special_call_out_params(
    signature: &LuaSignature,
) -> Vec<SpecialCallOutParamInfo> {
    let mut out_params = signature
        .out_params
        .iter()
        .map(|out_param| SpecialCallOutParamInfo {
            root: out_param.root.clone(),
            field_path: out_param.field_path.clone(),
            type_ref: out_param.type_ref.clone(),
            is_colon_define: signature.is_colon_define,
        })
        .collect::<Vec<_>>();
    out_params.sort_by_key(|out_param| match out_param.root {
        LuaOutParamRoot::Param(param_idx) => (0, param_idx),
        LuaOutParamRoot::SelfReceiver => (1, 0),
    });
    out_params
}

fn collect_doc_function_special_call_params(func: &LuaFunctionType) -> Vec<SpecialCallParamInfo> {
    func.get_params()
        .iter()
        .enumerate()
        .filter_map(|(idx, (_, param_type))| {
            let param_type = param_type.as_ref()?;
            if !type_contains_str_tpl_ref(param_type) {
                return None;
            }

            Some(SpecialCallParamInfo {
                param_idx: idx,
                param_type: param_type.clone(),
                is_constructor: false,
                is_colon_define: func.is_colon_define(),
                signature_id: None,
            })
        })
        .collect()
}

fn type_contains_str_tpl_ref(typ: &LuaType) -> bool {
    match typ {
        LuaType::StrTplRef(_) => true,
        LuaType::TypeGuard(inner) => type_contains_str_tpl_ref(inner),
        LuaType::Union(union) => union.types().any(type_contains_str_tpl_ref),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .any(type_contains_str_tpl_ref),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .any(|(union_type, _)| type_contains_str_tpl_ref(union_type)),
        _ => false,
    }
}

fn apply_out_param_from_call(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    call_expr: &LuaCallExpr,
    out_param_info: &SpecialCallOutParamInfo,
    is_colon_call: bool,
) -> ResolveResult {
    let Some(arg_expr) = get_out_param_root_expr(call_expr, out_param_info, is_colon_call) else {
        return Ok(());
    };

    let arg_expr_for_effects = arg_expr.clone();
    let mut visited = HashSet::new();
    let Some(target) = resolve_out_param_target(
        db,
        cache,
        file_id,
        arg_expr,
        &out_param_info.field_path,
        &mut visited,
    )?
    else {
        return Ok(());
    };

    let effect_targets = collect_out_param_effect_targets(
        db,
        cache,
        arg_expr_for_effects,
        &out_param_info.field_path,
        &target,
        matches!(out_param_info.root, LuaOutParamRoot::Param(_)),
    )?;
    for effect_target in effect_targets {
        db.get_flow_index_mut().add_special_call_effect(
            file_id,
            call_expr.get_position(),
            effect_target,
            out_param_info.type_ref.clone(),
        );
    }

    Ok(())
}

fn get_out_param_root_expr(
    call_expr: &LuaCallExpr,
    out_param_info: &SpecialCallOutParamInfo,
    is_colon_call: bool,
) -> Option<LuaExpr> {
    match out_param_info.root {
        LuaOutParamRoot::Param(param_idx) => get_call_arg_expr(
            call_expr,
            param_idx,
            out_param_info.is_colon_define,
            is_colon_call,
        ),
        LuaOutParamRoot::SelfReceiver if is_colon_call => {
            let LuaExpr::IndexExpr(index_expr) = call_expr.get_prefix_expr()? else {
                return None;
            };
            index_expr.get_prefix_expr()
        }
        LuaOutParamRoot::SelfReceiver => call_expr.get_args_list()?.get_args().next(),
    }
}

fn resolve_out_param_target(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    expr: LuaExpr,
    field_path: &[String],
    visited: &mut HashSet<LuaSemanticDeclId>,
) -> Result<Option<ResolvedOutParamTarget>, InferFailReason> {
    if field_path.is_empty() {
        return Ok(Some(ResolvedOutParamTarget {
            owner: expr_type_owner(db, cache, expr.clone())?,
            value_expr: Some(expr),
        }));
    }

    if let LuaExpr::TableExpr(table_expr) = expr.clone()
        && let Some(field) = find_table_field_by_name(&table_expr, &field_path[0])
    {
        let owner = Some(LuaMemberId::new(field.get_syntax_id(), file_id).into());
        let value_expr = field.get_value_expr();
        if field_path.len() == 1 {
            return Ok(Some(ResolvedOutParamTarget { owner, value_expr }));
        }

        if let Some(value_expr) = value_expr {
            return resolve_out_param_target(
                db,
                cache,
                file_id,
                value_expr,
                &field_path[1..],
                visited,
            );
        }
    }

    if let Some(semantic_decl) = try_infer_expr_semantic_decl(
        db,
        cache,
        expr.clone(),
        SemanticDeclGuard::default(),
        SemanticDeclLevel::default(),
    )? && visited.insert(semantic_decl.clone())
        && let Some(value_expr) = semantic_decl_value_expr(db, semantic_decl)
        && let Some(target) =
            resolve_out_param_target(db, cache, file_id, value_expr, field_path, visited)?
    {
        return Ok(Some(target));
    }

    let Some(target) =
        resolve_out_param_target_from_member_lookup(db, cache, expr, &field_path[0])?
    else {
        return Ok(None);
    };
    if field_path.len() == 1 {
        return Ok(Some(target));
    }

    let Some(value_expr) = target.value_expr.clone() else {
        return Ok(None);
    };
    resolve_out_param_target(db, cache, file_id, value_expr, &field_path[1..], visited)
}

fn resolve_out_param_target_from_member_lookup(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: LuaExpr,
    field_name: &str,
) -> Result<Option<ResolvedOutParamTarget>, InferFailReason> {
    let expr_type = infer_expr(db, cache, expr)?;
    let Some(member_infos) =
        find_members_with_key(db, &expr_type, LuaMemberKey::Name(field_name.into()), true)
    else {
        return Ok(None);
    };

    Ok(member_infos.into_iter().find_map(|member_info| {
        let semantic_decl = member_info.property_owner_id?;
        Some(ResolvedOutParamTarget {
            owner: semantic_decl_to_type_owner(semantic_decl.clone()),
            value_expr: semantic_decl_value_expr(db, semantic_decl),
        })
    }))
}

fn find_table_field_by_name(table_expr: &LuaTableExpr, field_name: &str) -> Option<LuaTableField> {
    table_expr
        .get_fields()
        .find(|field| match field.get_field_key() {
            Some(glua_parser::LuaIndexKey::Name(name)) => name.get_name_text() == field_name,
            Some(glua_parser::LuaIndexKey::String(string)) => string.get_value() == field_name,
            _ => false,
        })
}

fn expr_type_owner(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: LuaExpr,
) -> Result<Option<LuaTypeOwner>, InferFailReason> {
    Ok(try_infer_expr_semantic_decl(
        db,
        cache,
        expr,
        SemanticDeclGuard::default(),
        SemanticDeclLevel::default(),
    )?
    .and_then(semantic_decl_to_type_owner))
}

fn semantic_decl_to_type_owner(semantic_decl: LuaSemanticDeclId) -> Option<LuaTypeOwner> {
    match semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => Some(decl_id.into()),
        LuaSemanticDeclId::Member(member_id) => Some(member_id.into()),
        LuaSemanticDeclId::Signature(_) | LuaSemanticDeclId::TypeDecl(_) => None,
    }
}

fn collect_out_param_effect_targets(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    arg_expr: LuaExpr,
    field_path: &[String],
    target: &ResolvedOutParamTarget,
    include_resolved_targets: bool,
) -> Result<Vec<VarRefId>, InferFailReason> {
    let mut targets = Vec::new();
    let mut visited_aliases = HashSet::new();
    let call_position = arg_expr.get_range().start();

    collect_out_param_effect_path_targets(
        db,
        cache,
        arg_expr,
        call_position,
        field_path,
        &mut targets,
        &mut visited_aliases,
    )?;

    if include_resolved_targets
        && let Some(owner) = target.owner.clone()
        && let Some(owner_target) = type_owner_to_var_ref_id(owner)
        && !targets.iter().any(|existing| existing == &owner_target)
    {
        targets.push(owner_target);
    }

    if include_resolved_targets
        && let Some(value_expr) = target.value_expr.clone()
        && let Some(value_target) = get_var_expr_var_ref_id(db, cache, value_expr)
        && !targets.iter().any(|existing| existing == &value_target)
    {
        targets.push(value_target);
    }

    Ok(targets)
}

fn collect_out_param_effect_path_targets(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    arg_expr: LuaExpr,
    call_position: TextSize,
    field_path: &[String],
    targets: &mut Vec<VarRefId>,
    visited_aliases: &mut HashSet<LuaSemanticDeclId>,
) -> Result<(), InferFailReason> {
    let arg_access_path = get_expr_access_path(&arg_expr);
    if let Some(base_var_ref_id) = get_var_expr_var_ref_id(db, cache, arg_expr.clone()) {
        if let Some(path_target) = extend_var_ref_id_with_path(
            base_var_ref_id.clone(),
            arg_access_path.as_deref(),
            field_path,
        ) && !targets.iter().any(|existing| existing == &path_target)
        {
            targets.push(path_target);
        }

        if let Some(semantic_decl) = semantic_decl_from_var_ref_id(&base_var_ref_id)
            && visited_aliases.insert(semantic_decl.clone())
            && !semantic_decl_has_write_before_position(db, &semantic_decl, call_position)
            && let Some(value_expr) = semantic_decl_value_expr(db, semantic_decl)
        {
            collect_out_param_effect_path_targets(
                db,
                cache,
                value_expr,
                call_position,
                field_path,
                targets,
                visited_aliases,
            )?;
        }
        return Ok(());
    }

    if let Some(semantic_decl) = try_infer_expr_semantic_decl(
        db,
        cache,
        arg_expr,
        SemanticDeclGuard::default(),
        SemanticDeclLevel::default(),
    )? && visited_aliases.insert(semantic_decl.clone())
        && !semantic_decl_has_write_before_position(db, &semantic_decl, call_position)
        && let Some(value_expr) = semantic_decl_value_expr(db, semantic_decl)
    {
        collect_out_param_effect_path_targets(
            db,
            cache,
            value_expr,
            call_position,
            field_path,
            targets,
            visited_aliases,
        )?;
    }

    Ok(())
}

fn semantic_decl_has_write_before_position(
    db: &DbIndex,
    semantic_decl: &LuaSemanticDeclId,
    position: TextSize,
) -> bool {
    let LuaSemanticDeclId::LuaDecl(decl_id) = semantic_decl else {
        return false;
    };

    let Some(decl) = db.get_decl_index().get_decl(decl_id) else {
        return false;
    };
    if !decl.is_local() {
        return false;
    }

    db.get_reference_index()
        .get_decl_references(&decl.get_file_id(), decl_id)
        .is_some_and(|decl_refs| {
            let decl_position = decl.get_position();
            decl_refs.cells.iter().any(|decl_ref| {
                decl_ref.is_write
                    && decl_ref.range.start() > decl_position
                    && decl_ref.range.start() < position
            })
        })
}

fn semantic_decl_from_var_ref_id(var_ref_id: &VarRefId) -> Option<LuaSemanticDeclId> {
    match var_ref_id {
        VarRefId::VarRef(decl_id) => Some(LuaSemanticDeclId::LuaDecl(*decl_id)),
        VarRefId::SelfRef(self_ref_id) => match &self_ref_id.receiver {
            LuaDeclOrMemberId::Decl(decl_id) => Some(LuaSemanticDeclId::LuaDecl(*decl_id)),
            LuaDeclOrMemberId::Member(member_id) => Some(LuaSemanticDeclId::Member(*member_id)),
        },
        VarRefId::IndexRef(_, _) | VarRefId::GlobalName(_, _) => None,
    }
}

fn get_expr_access_path(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::NameExpr(name_expr) => Some(name_expr.get_name_text()?.to_string()),
        LuaExpr::IndexExpr(index_expr) => index_expr.get_access_path().map(Into::into),
        _ => None,
    }
}

fn type_owner_to_var_ref_id(type_owner: LuaTypeOwner) -> Option<VarRefId> {
    match type_owner {
        LuaTypeOwner::Decl(decl_id) => Some(VarRefId::VarRef(decl_id)),
        LuaTypeOwner::Member(member_id) => {
            // Represent a member-owner effect target as a member-rooted `SelfRef`.
            // The `self_decl_id` is synthesized from the member's own location so
            // the identity is unique; `receiver` carries the member used for
            // base/member type lookup and index-ref extension.
            Some(VarRefId::SelfRef(SelfRefId {
                self_decl_id: LuaDeclId::new(member_id.file_id, member_id.get_position()),
                receiver: LuaDeclOrMemberId::Member(member_id),
            }))
        }
        LuaTypeOwner::SyntaxId(_) => None,
    }
}

fn extend_var_ref_id_with_path(
    var_ref_id: VarRefId,
    access_path: Option<&str>,
    field_path: &[String],
) -> Option<VarRefId> {
    if field_path.is_empty() {
        return Some(var_ref_id);
    }

    let full_path = match access_path {
        Some(access_path) => format!("{}.{}", access_path, field_path.join(".")),
        None => field_path.join("."),
    };
    let arc_path = crate::db_index::intern_smol_str(&full_path);
    match var_ref_id {
        VarRefId::VarRef(decl_id) => {
            Some(VarRefId::IndexRef(VarRefRootId::Decl(decl_id), arc_path))
        }
        VarRefId::SelfRef(self_ref_id) => Some(VarRefId::IndexRef(
            VarRefRootId::SelfRef(self_ref_id),
            arc_path,
        )),
        VarRefId::IndexRef(root, _) => Some(VarRefId::IndexRef(root, arc_path)),
        VarRefId::GlobalName(_, _) => None,
    }
}

fn materialize_str_tpl_class_from_call(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    file_id: crate::FileId,
    call_expr: &LuaCallExpr,
    param_idx: usize,
    param_type: &LuaType,
    is_colon_define: bool,
    is_colon_call: bool,
) -> ResolveResult {
    let Some(str_tpl) = find_str_tpl_ref(db, param_type) else {
        return Ok(());
    };

    let constraint = match str_tpl.get_constraint() {
        Some(LuaType::Ref(type_decl_id)) => type_decl_id.clone(),
        _ => return Ok(()),
    };
    let is_class_constraint = db
        .get_type_index()
        .get_type_decl(&constraint)
        .map(|decl| decl.is_class())
        .unwrap_or(false);
    if !is_class_constraint {
        return Ok(());
    }

    let Some(arg_expr) = get_call_arg_expr(call_expr, param_idx, is_colon_define, is_colon_call)
    else {
        return Ok(());
    };
    // The name a call binds the template to, as the call's generic
    // instantiation binds it.
    let Ok(arg_type) = infer_expr(db, cache, arg_expr.clone()) else {
        return Ok(());
    };
    let LuaType::StringConst(arg_name) = crate::semantic::str_tpl_arg_type(
        db, cache, param_type, &arg_expr, call_expr, param_idx, arg_type,
    ) else {
        return Ok(());
    };

    let class_name = format!(
        "{}{}{}",
        str_tpl.get_prefix(),
        arg_name,
        str_tpl.get_suffix()
    );
    let class_decl_id = LuaTypeDeclId::global(&class_name);
    // Whether a declaration owns the name is only final once every
    // registration has run: deciding earlier would leave a placeholder behind
    // for a class a later registration declares.
    if !db.stage_rule_holds(StageRule::FloorUnsettled) {
        return Ok(());
    }
    // A real declaration already owns this name (a `---@class` the author
    // wrote, or a Register-synthesized one): the materialization must not add
    // edges behind its back.
    let should_attach_super = !db.get_type_index().has_non_auto_declaration(&class_decl_id);
    // A re-settled call replays the same materialization: only a novel decl
    // or super edge may bump the special-call epoch. The facts carry their
    // full identity (call-site, class, constraint, source), so replaying them
    // in any order hits the same dedup keys.
    let call_position = call_expr.get_position();
    let mut novel_facts = Vec::new();
    // Every site materializing the class declares it: which one ran first
    // must not decide where the class is declared.
    if should_attach_super {
        db.get_type_index_mut().add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                arg_expr.get_range(),
                class_decl_id.get_simple_name().to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::AutoGenerated.into(),
                class_decl_id.clone(),
            ),
        );
        novel_facts.push(SpecialCallFact::str_tpl_decl(
            file_id,
            call_position,
            class_decl_id.clone(),
            LuaType::Ref(constraint.clone()),
            param_idx,
            is_colon_define,
            is_colon_call,
        ));
    }

    if !should_attach_super {
        if !novel_facts.is_empty() {
            db.get_flow_index_mut()
                .install_special_call_facts(file_id, call_position, novel_facts);
        }
        return Ok(());
    }

    let super_type = LuaType::Ref(constraint);
    let source_range = arg_expr.get_range();
    if db.get_type_index_mut().add_super_type(
        class_decl_id.clone(),
        file_id,
        source_range,
        super_type.clone(),
    ) {
        novel_facts.push(SpecialCallFact::super_edge(
            file_id,
            call_position,
            class_decl_id,
            super_type,
            file_id,
            source_range,
        ));
    }
    if !novel_facts.is_empty() {
        db.get_flow_index_mut()
            .install_special_call_facts(file_id, call_position, novel_facts);
    }

    Ok(())
}

fn try_resolve_constructor_param(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    file_id: crate::FileId,
    call_expr: &LuaCallExpr,
    param_info: &SpecialCallParamInfo,
) -> ResolveResult {
    let signature_id = param_info.signature_id.ok_or(InferFailReason::None)?;
    let (_, target_signature_name, root_class, strip_self, return_self) = {
        let signature = db
            .get_signature_index()
            .get(&signature_id)
            .ok_or(InferFailReason::None)?;
        let param_doc = signature
            .get_param_info_by_id(param_info.param_idx)
            .ok_or(InferFailReason::None)?;
        let constructor_use = param_doc
            .get_attribute_by_name("constructor")
            .ok_or(InferFailReason::None)?;

        let target_signature_name = constructor_use
            .get_param_by_name("name")
            .and_then(|typ| match typ {
                LuaType::DocStringConst(value) => Some(value.deref().clone()),
                _ => None,
            })
            .ok_or(InferFailReason::None)?;
        let root_class =
            constructor_use
                .get_param_by_name("root_class")
                .and_then(|typ| match typ {
                    LuaType::DocStringConst(value) => Some(value.deref().clone()),
                    _ => None,
                });
        let strip_self = constructor_use
            .get_param_by_name("strip_self")
            .and_then(|typ| match typ {
                LuaType::DocBooleanConst(value) => Some(*value),
                _ => None,
            })
            .unwrap_or(true);
        let return_self = constructor_use
            .get_param_by_name("return_self")
            .and_then(|typ| match typ {
                LuaType::DocBooleanConst(value) => Some(*value),
                _ => None,
            })
            .unwrap_or(true);

        Ok::<_, InferFailReason>((
            param_doc.type_ref.clone(),
            target_signature_name,
            root_class,
            strip_self,
            return_self,
        ))
    }?;

    let target_id = get_constructor_target_type(
        db,
        cache,
        &param_info.param_type,
        call_expr.clone(),
        param_info.param_idx,
        param_info.is_colon_define,
        call_expr.is_colon_call(),
    )
    .ok_or(InferFailReason::None)?;

    if let Some(root_class) = root_class {
        let root_type_id = LuaTypeDeclId::global(&root_class);
        if let Some(type_decl) = db.get_type_index().get_type_decl(&root_type_id)
            && type_decl.is_class()
        {
            let source_range = get_call_arg_expr(
                call_expr,
                param_info.param_idx,
                param_info.is_colon_define,
                call_expr.is_colon_call(),
            )
            .map(|arg| arg.get_range())
            .unwrap_or_else(|| call_expr.get_range());
            let root_type = LuaType::Ref(root_type_id.clone());
            if db.get_type_index_mut().add_super_type(
                target_id.clone(),
                file_id,
                source_range,
                root_type.clone(),
            ) {
                db.get_flow_index_mut().install_special_call_facts(
                    file_id,
                    call_expr.get_position(),
                    [SpecialCallFact::super_edge(
                        file_id,
                        call_expr.get_position(),
                        target_id.clone(),
                        root_type,
                        file_id,
                        source_range,
                    )],
                );
            }
        }
    }

    let target_type = LuaType::Ref(target_id);
    let member_key = LuaMemberKey::Name(target_signature_name);
    let members = db
        .get_module_index()
        .get_workspace_id(file_id)
        .and_then(|workspace_id| {
            crate::semantic::find_members_with_key_in_workspace_for_file_at_offset(
                db,
                &target_type,
                member_key.clone(),
                true,
                workspace_id,
                file_id,
                call_expr.get_position(),
            )
        })
        .or_else(|| {
            db.get_module_index()
                .get_workspace_id(file_id)
                .is_none()
                .then(|| find_members_with_key(db, &target_type, member_key, true))?
        })
        .ok_or(InferFailReason::FieldNotFound)?;
    let ctor_signature_member = members.first().ok_or(InferFailReason::FieldNotFound)?;

    set_signature_to_default_call(
        db,
        cache,
        ctor_signature_member,
        strip_self,
        return_self,
        file_id,
        call_expr.get_position(),
    )
    .ok_or(InferFailReason::FieldNotFound)?;

    Ok(())
}

fn set_signature_to_default_call(
    db: &mut DbIndex,
    cache: &mut LuaInferCache,
    member_info: &LuaMemberInfo,
    strip_self: bool,
    return_self: bool,
    site_file: FileId,
    site_position: TextSize,
) -> Option<bool> {
    let LuaType::Signature(signature_id) = member_info.typ else {
        return None;
    };
    let Some(LuaSemanticDeclId::Member(member_id)) = member_info.property_owner_id else {
        return None;
    };
    // We still need another check to confirm the origin is a `Def` type
    let index_expr = LuaIndexExpr::cast(
        db.get_vfs()
            .syntax_node_around(&member_id.file_id, *member_id.get_syntax_id())?,
    )?;
    let prefix_expr = index_expr.get_prefix_expr()?;
    let prefix_type = infer_expr(db, cache, prefix_expr.clone()).ok()?;
    let LuaType::Def(decl_id) = prefix_type else {
        return None;
    };
    // Do not add if an explicit `__call` definition already exists
    let call = db.get_operator_index().get_operators(
        &LuaOperatorOwner::Type(decl_id.clone()),
        LuaOperatorMetaMethod::Call,
    );
    if call.is_some() {
        return None;
    }

    let operator = LuaOperator::new(
        decl_id.clone().into(),
        LuaOperatorMetaMethod::Call,
        member_id.file_id,
        // Must point at the name; the full range of index_expr would not jump to the function
        index_expr.get_name_token()?.syntax().text_range(),
        OperatorFunction::DefaultClassCtor {
            id: signature_id,
            strip_self,
            return_self,
        },
    );
    let operator_id = operator.get_id();
    let installed = db.get_operator_index_mut().add_operator(operator);
    if installed {
        db.get_flow_index_mut().install_special_call_facts(
            site_file,
            site_position,
            [SpecialCallFact::call_operator(
                site_file,
                site_position,
                operator_id,
                LuaOperatorOwner::Type(decl_id),
                signature_id,
                strip_self,
                return_self,
            )],
        );
    }
    Some(installed)
}

fn get_constructor_target_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    param_type: &LuaType,
    call_expr: LuaCallExpr,
    call_index: usize,
    is_colon_define: bool,
    is_colon_call: bool,
) -> Option<LuaTypeDeclId> {
    if let Some(str_tpl) = find_str_tpl_ref(db, param_type) {
        let arg_expr = get_call_arg_expr(&call_expr, call_index, is_colon_define, is_colon_call)?;
        let name = infer_string_const_arg(db, cache, &arg_expr)?;
        let type_decl_id: LuaTypeDeclId = LuaTypeDeclId::global(
            format!("{}{}{}", str_tpl.get_prefix(), name, str_tpl.get_suffix()).as_str(),
        );
        let type_decl = db.get_type_index().get_type_decl(&type_decl_id)?;
        if type_decl.is_class() {
            return Some(type_decl_id);
        }
    }

    None
}

fn find_str_tpl_ref(db: &DbIndex, typ: &LuaType) -> Option<Arc<crate::LuaStringTplType>> {
    match typ {
        LuaType::StrTplRef(str_tpl) => Some(str_tpl.clone()),
        LuaType::TypeGuard(inner) => find_str_tpl_ref(db, inner),
        LuaType::Union(union) => union
            .types()
            .filter_map(|union_type| find_str_tpl_ref(db, union_type))
            .min_by_key(|str_tpl| str_tpl_selection_key(db, str_tpl)),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .filter_map(|intersection_type| find_str_tpl_ref(db, intersection_type))
            .min_by_key(|str_tpl| str_tpl_selection_key(db, str_tpl)),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .filter_map(|(union_type, _)| find_str_tpl_ref(db, union_type))
            .min_by_key(|str_tpl| str_tpl_selection_key(db, str_tpl)),
        _ => None,
    }
}

fn str_tpl_selection_key(db: &DbIndex, str_tpl: &crate::LuaStringTplType) -> String {
    let constraint_key = str_tpl
        .get_constraint()
        .map(|constraint| humanize_type(db, constraint, RenderLevel::Detailed))
        .unwrap_or_default();
    format!(
        "{}|{}|{}|{}",
        str_tpl.get_prefix(),
        str_tpl.get_name(),
        str_tpl.get_suffix(),
        constraint_key
    )
}

fn get_call_arg_expr(
    call_expr: &LuaCallExpr,
    param_idx: usize,
    is_colon_define: bool,
    is_colon_call: bool,
) -> Option<LuaExpr> {
    let arg_idx = match (is_colon_define, is_colon_call) {
        (true, false) => param_idx.checked_add(1)?,
        (false, true) => param_idx.checked_sub(1)?,
        _ => param_idx,
    };
    call_expr.get_args_list()?.get_args().nth(arg_idx)
}

fn infer_string_const_arg(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    arg_expr: &LuaExpr,
) -> Option<String> {
    match infer_expr(db, cache, arg_expr.clone()).ok()? {
        LuaType::StringConst(s) => Some(s.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use rowan::{TextRange, TextSize};

    use super::{
        TypeCacheWriteMode, find_str_tpl_ref, get_operator_id_priority_tiers, iter_var_write_mode,
        local_cached_type_is_informative, select_operator_ids_by_workspace_and_realm,
    };
    use crate::{
        DbIndex, FileId, GenericTplId, GmodRealm, GmodRealmFileMetadata, InFiled, LuaOperator,
        LuaOperatorMetaMethod, LuaOperatorOwner, LuaType, LuaTypeCache, LuaTypeDeclId, WorkspaceId,
        db_index::{
            AsyncState, LuaFunctionType, LuaStringTplType, LuaUnionType, OperatorFunction,
            WorkspaceKind,
        },
    };

    fn make_db() -> DbIndex {
        let mut db = DbIndex::new();
        db.get_module_index_mut()
            .set_module_extract_patterns(["?.lua".to_string(), "?/init.lua".to_string()].to_vec());
        db
    }

    fn add_call_operator(
        db: &mut DbIndex,
        owner: &LuaOperatorOwner,
        file_id: FileId,
        start: u32,
    ) -> crate::LuaOperatorId {
        let range = TextRange::new(TextSize::new(start), TextSize::new(start + 1));
        let operator = LuaOperator::new(
            owner.clone(),
            LuaOperatorMetaMethod::Call,
            file_id,
            range,
            OperatorFunction::Func(std::sync::Arc::new(LuaFunctionType::new(
                AsyncState::None,
                false,
                false,
                vec![("arg".to_string(), Some(LuaType::String))],
                LuaType::Boolean,
            ))),
        );
        let id = operator.get_id();
        db.get_operator_index_mut().add_operator(operator);
        id
    }

    fn set_file_realms(db: &mut DbIndex, file_realms: &[(FileId, GmodRealm)]) {
        db.get_gmod_infer_index_mut().set_all_realm_file_metadata(
            file_realms
                .iter()
                .map(|(file_id, realm)| {
                    (
                        *file_id,
                        GmodRealmFileMetadata {
                            inferred_realm: *realm,
                            ..Default::default()
                        },
                    )
                })
                .collect(),
        );
    }

    #[test]
    fn local_cached_type_informative_accepts_nullable_concrete_unions() {
        let typ = LuaType::Union(
            LuaUnionType::from_vec(vec![
                LuaType::Nil,
                LuaType::Ref(LuaTypeDeclId::global("base_glide")),
            ])
            .into(),
        );

        assert!(local_cached_type_is_informative(&typ));
    }

    #[test]
    fn local_cached_type_informative_rejects_weak_only_unions() {
        let typ = LuaType::Union(
            LuaUnionType::from_vec(vec![LuaType::Nil, LuaType::Unknown, LuaType::Any]).into(),
        );

        assert!(!local_cached_type_is_informative(&typ));
    }

    fn widening_union() -> LuaType {
        LuaType::Union(Arc::new(LuaUnionType::from_vec(vec![
            LuaType::String,
            LuaType::Integer,
        ])))
    }

    fn tpl_placeholder() -> LuaType {
        LuaType::StrTplRef(Arc::new(LuaStringTplType::new(
            "",
            "T",
            GenericTplId::Func(0),
            "",
            None,
        )))
    }

    #[test]
    fn iter_var_doc_type_survives_a_settled_widening() {
        // `---@param v string` is legal on a `for ... in` variable.
        let cached = LuaTypeCache::DocType(LuaType::String);

        assert_eq!(
            iter_var_write_mode(Some(&cached), &widening_union()),
            TypeCacheWriteMode::InsertOnly
        );
    }

    #[test]
    fn iter_var_doc_template_placeholder_survives_a_settled_type() {
        let cached = LuaTypeCache::DocType(tpl_placeholder());

        assert_eq!(
            iter_var_write_mode(Some(&cached), &LuaType::String),
            TypeCacheWriteMode::InsertOnly
        );
    }

    #[test]
    fn iter_var_inferred_type_is_replaced_by_a_settled_widening() {
        let cached = LuaTypeCache::InferType(LuaType::String);

        assert_eq!(
            iter_var_write_mode(Some(&cached), &widening_union()),
            TypeCacheWriteMode::ForceOverwrite
        );
    }

    #[test]
    fn iter_var_inferred_template_placeholder_is_replaced_by_a_settled_type() {
        let cached = LuaTypeCache::InferType(tpl_placeholder());

        assert_eq!(
            iter_var_write_mode(Some(&cached), &LuaType::String),
            TypeCacheWriteMode::ForceOverwrite
        );
    }

    #[test]
    fn operator_id_priority_tiers_keep_workspace_priority_order() {
        let mut db = make_db();
        let module_index = db.get_module_index_mut();

        let workspace_a = WorkspaceId::MAIN;
        let workspace_b = WorkspaceId { id: 3 };
        let library_workspace = WorkspaceId { id: 4 };

        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA").into(),
            workspace_a,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectB").into(),
            workspace_b,
            WorkspaceKind::Main,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/ProjectA/lua/lib").into(),
            library_workspace,
            WorkspaceKind::Library,
        );
        module_index.add_workspace_root_with_kind(
            Path::new("C:/Users/username/.lua/std").into(),
            WorkspaceId::STD,
            WorkspaceKind::Std,
        );

        let caller_file = FileId::new(1);
        module_index.add_module_by_path(caller_file, "C:/Users/username/ProjectA/init.lua");

        let library_file = FileId::new(2);
        module_index.add_module_by_path(
            library_file,
            "C:/Users/username/ProjectA/lua/lib/shared.lua",
        );

        let std_file = FileId::new(3);
        module_index.add_module_by_path(std_file, "C:/Users/username/.lua/std/math.lua");

        let other_main_file = FileId::new(4);
        module_index.add_module_by_path(other_main_file, "C:/Users/username/ProjectB/init.lua");

        let owner = LuaOperatorOwner::Type(LuaTypeDeclId::global("Callable"));
        let library_operator = add_call_operator(&mut db, &owner, library_file, 1);
        let std_operator = add_call_operator(&mut db, &owner, std_file, 2);
        let _other_main_operator = add_call_operator(&mut db, &owner, other_main_file, 3);

        let tiers = get_operator_id_priority_tiers(
            &db,
            caller_file,
            &[library_operator, std_operator, _other_main_operator],
        );

        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0], (1, vec![library_operator]));
        assert_eq!(tiers[1], (2, vec![std_operator]));
    }

    #[test]
    fn select_operator_ids_by_workspace_and_realm_uses_first_compatible_tier() {
        let mut db = make_db();
        let caller_file = FileId::new(1);
        let owner = LuaOperatorOwner::Table(InFiled::new(
            FileId::new(99),
            TextRange::new(TextSize::new(0), TextSize::new(1)),
        ));
        let tier_one_operator = add_call_operator(&mut db, &owner, FileId::new(10), 1);
        let tier_two_operator = add_call_operator(&mut db, &owner, FileId::new(11), 2);

        set_file_realms(
            &mut db,
            &[
                (caller_file, GmodRealm::Client),
                (tier_one_operator.file_id, GmodRealm::Shared),
                (tier_two_operator.file_id, GmodRealm::Server),
            ],
        );

        let selected = select_operator_ids_by_workspace_and_realm(
            &db,
            caller_file,
            TextSize::new(0),
            vec![(0, vec![tier_one_operator]), (1, vec![tier_two_operator])],
        );

        assert_eq!(selected, vec![tier_one_operator]);
    }

    #[test]
    fn select_operator_ids_by_workspace_and_realm_falls_back_to_best_tier_when_needed() {
        let mut db = make_db();
        let caller_file = FileId::new(1);
        let owner = LuaOperatorOwner::Table(InFiled::new(
            FileId::new(99),
            TextRange::new(TextSize::new(0), TextSize::new(1)),
        ));
        let best_tier_operator = add_call_operator(&mut db, &owner, FileId::new(20), 1);
        let lower_tier_operator = add_call_operator(&mut db, &owner, FileId::new(21), 2);

        set_file_realms(
            &mut db,
            &[
                (caller_file, GmodRealm::Client),
                (best_tier_operator.file_id, GmodRealm::Server),
                (lower_tier_operator.file_id, GmodRealm::Server),
            ],
        );

        let selected = select_operator_ids_by_workspace_and_realm(
            &db,
            caller_file,
            TextSize::new(0),
            vec![
                (0, vec![best_tier_operator]),
                (1, vec![lower_tier_operator]),
            ],
        );

        assert_eq!(selected, vec![best_tier_operator]);
    }

    #[test]
    fn find_str_tpl_ref_union_order_is_deterministic() {
        let alpha_tpl = LuaType::StrTplRef(Arc::new(LuaStringTplType::new(
            "alpha.",
            "T",
            GenericTplId::Func(0),
            "",
            Some(LuaType::Ref(LuaTypeDeclId::global("Entity"))),
        )));
        let beta_tpl = LuaType::StrTplRef(Arc::new(LuaStringTplType::new(
            "beta.",
            "T",
            GenericTplId::Func(0),
            "",
            Some(LuaType::Ref(LuaTypeDeclId::global("Entity"))),
        )));

        let alpha_first = LuaType::Union(Arc::new(LuaUnionType::from_vec(vec![
            alpha_tpl.clone(),
            beta_tpl.clone(),
        ])));
        let beta_first =
            LuaType::Union(Arc::new(LuaUnionType::from_vec(vec![beta_tpl, alpha_tpl])));

        let db = make_db();
        let alpha_first_selected = find_str_tpl_ref(&db, &alpha_first)
            .expect("expected string template in alpha-first union");
        let beta_first_selected = find_str_tpl_ref(&db, &beta_first)
            .expect("expected string template in beta-first union");

        assert_eq!(
            alpha_first_selected.get_prefix(),
            beta_first_selected.get_prefix(),
            "string template selection should be independent of union member order"
        );
    }

    const SPECIAL_DEFS: &str = r#"
        ---@class Entity

        ents = {}

        ---@generic T: Entity
        ---@param class `T`
        ---@return T[]
        function ents.FindByClass(class) end

        ---@class TraceResult
        ---@field Hit boolean
        local TraceResult = {}

        util = {}

        ---@outparam traceConfig.output TraceResult
        ---@param traceConfig table
        function util.TraceLine(traceConfig) end
    "#;

    const SPECIAL_CALLER_A: &str = r#"local found_a = ents.FindByClass("prop_vehicle_jeep")"#;
    const SPECIAL_CALLER_B: &str = r#"local found_b = ents.FindByClass("prop_vehicle_jeep")"#;
    const SPECIAL_CALLER_C: &str = r#"
        local ray = {}
        local traceData = {
            output = ray,
        }

        util.TraceLine(traceData)
    "#;

    /// Everything special calls have settled, as sorted text.
    fn special_call_snapshot(db: &DbIndex) -> Vec<String> {
        let flow = db.get_flow_index();
        let mut snapshot = flow
            .settled_special_call_facts()
            .map(|fact| format!("{fact:?}"))
            .chain(
                flow.special_call_effects()
                    .map(|entry| format!("{entry:?}")),
            )
            .collect::<Vec<_>>();
        snapshot.sort();
        snapshot
    }

    fn special_call_workspace() -> (
        crate::VirtualWorkspace,
        Vec<(FileId, Vec<glua_parser::LuaCallExpr>)>,
    ) {
        use glua_parser::LuaAstNode;

        let mut ws = crate::VirtualWorkspace::new();
        ws.def_file("defs.lua", SPECIAL_DEFS);
        let caller_ids = [
            ws.def_file("caller_a.lua", SPECIAL_CALLER_A),
            ws.def_file("caller_b.lua", SPECIAL_CALLER_B),
            ws.def_file("caller_c.lua", SPECIAL_CALLER_C),
        ];
        let db = ws.get_db_mut();
        assert!(
            !special_call_snapshot(db).is_empty(),
            "the fixture must settle special-call facts"
        );
        let generated = LuaTypeDeclId::global("prop_vehicle_jeep");
        assert!(
            db.get_type_index().get_type_decl(&generated).is_some(),
            "the fixture must synthesize the str-tpl class"
        );

        let mut sites = Vec::new();
        for file_id in caller_ids {
            let calls = db
                .get_vfs()
                .get_syntax_tree(&file_id)
                .expect("expected syntax tree")
                .get_chunk_node()
                .descendants::<glua_parser::LuaCallExpr>()
                .collect::<Vec<_>>();
            assert!(
                !calls.is_empty(),
                "expected call expressions in {file_id:?}"
            );
            sites.push((file_id, calls));
        }
        (ws, sites)
    }

    /// Re-drives every special-call site in `order`, returning whether the
    /// replay changed what special calls have settled.
    fn resettle_special_calls(
        db: &mut DbIndex,
        sites: &[(FileId, Vec<glua_parser::LuaCallExpr>)],
        order: &[usize],
    ) -> bool {
        use super::{UnResolveSpecialCall, try_resolve_special_call};

        let before = special_call_snapshot(db);
        for &idx in order {
            let (file_id, calls) = &sites[idx];
            for call_expr in calls {
                let mut item = UnResolveSpecialCall {
                    file_id: *file_id,
                    call_expr: call_expr.clone(),
                };
                let mut cache = crate::LuaInferCache::new(*file_id, Default::default());
                try_resolve_special_call(db, &mut cache, &mut item)
                    .expect("expected the settled call to re-derive");
            }
        }
        special_call_snapshot(db) != before
    }

    /// Replaying the same special-call fact set in two orders changes nothing
    /// on the second pass: identical facts build identical dedup keys however
    /// the pass interleaves the sites.
    #[test]
    fn resettling_special_calls_in_any_order_changes_nothing() {
        use glua_parser::LuaAstNode;

        let (mut ws, mut sites) = special_call_workspace();

        let db = ws.get_db_mut();
        assert!(
            !resettle_special_calls(db, &sites, &[0, 1, 2]),
            "replaying settled special calls changes nothing"
        );
        assert!(
            !resettle_special_calls(db, &sites, &[2, 1, 0]),
            "replaying settled special calls in reverse order changes nothing"
        );

        // A genuinely new class still settles.
        let live_before = special_call_snapshot(db);
        let caller_d = ws.def_file(
            "caller_d.lua",
            r#"local found_d = ents.FindByClass("prop_vehicle_apc")"#,
        );
        assert_ne!(
            special_call_snapshot(ws.get_db_mut()),
            live_before,
            "a novel class install still settles"
        );
        let db = ws.get_db_mut();
        let new_calls = db
            .get_vfs()
            .get_syntax_tree(&caller_d)
            .expect("expected syntax tree")
            .get_chunk_node()
            .descendants::<glua_parser::LuaCallExpr>()
            .collect::<Vec<_>>();
        sites.push((caller_d, new_calls));
        assert!(
            !resettle_special_calls(db, &sites, &[3, 2, 1, 0]),
            "replaying after a novel install changes nothing"
        );
    }

    /// Which caller runs first does not decide where a class both call sites
    /// materialize is declared, nor which facts settle.
    #[test]
    fn a_str_tpl_class_two_sites_materialize_settles_the_same_in_either_order() {
        let settle = |schedule| {
            let mut ws = crate::VirtualWorkspace::new();
            ws.analysis.compilation.set_schedule(schedule);
            ws.def_files(vec![
                ("defs.lua", SPECIAL_DEFS),
                ("caller_a.lua", SPECIAL_CALLER_A),
                ("caller_b.lua", SPECIAL_CALLER_B),
            ]);
            let db = ws.get_db_mut();
            let locations = db
                .get_type_index()
                .get_type_decl(&LuaTypeDeclId::global("prop_vehicle_jeep"))
                .expect("the str-tpl class is materialized")
                .get_locations()
                .iter()
                .map(|location| (location.file_id, location.range))
                .collect::<Vec<_>>();
            (locations, special_call_snapshot(db))
        };

        let canonical = settle(crate::Schedule::Canonical);

        assert_eq!(settle(crate::Schedule::Reversed), canonical);
        assert_eq!(canonical.0.len(), 2);
    }

    /// Defining the same callers in the opposite order settles as many facts:
    /// which site materializes a shared fact first must not change the count.
    #[test]
    fn mirrored_workspaces_settle_the_same_number_of_special_call_facts() {
        let mut forward = crate::VirtualWorkspace::new();
        forward.def_file("defs.lua", SPECIAL_DEFS);
        forward.def_file("caller_a.lua", SPECIAL_CALLER_A);
        forward.def_file("caller_b.lua", SPECIAL_CALLER_B);
        forward.def_file("caller_c.lua", SPECIAL_CALLER_C);

        let mut reverse = crate::VirtualWorkspace::new();
        reverse.def_file("defs.lua", SPECIAL_DEFS);
        reverse.def_file("caller_c.lua", SPECIAL_CALLER_C);
        reverse.def_file("caller_b.lua", SPECIAL_CALLER_B);
        reverse.def_file("caller_a.lua", SPECIAL_CALLER_A);

        assert_eq!(
            special_call_snapshot(forward.get_db_mut()).len(),
            special_call_snapshot(reverse.get_db_mut()).len(),
            "definition order must not change how many facts settle"
        );
    }

    /// A constructor call's `__call` install reports itself the same way on
    /// replay without changing anything: an existing install is not novel.
    #[test]
    fn resettling_constructor_special_call_changes_nothing() {
        use glua_parser::LuaAstNode;

        use super::{UnResolveSpecialCall, try_resolve_special_call};

        let mut ws = crate::VirtualWorkspace::new();
        ws.def_file(
            "defs.lua",
            r#"
                ---@attribute constructor(name: string, root_class: string?, strip_self: boolean?, return_self: boolean?)

                ---@class Entity

                ents = {}

                ---@generic T: Entity
                ---@[constructor("__init", "Entity")]
                ---@param class `T`
                ---@return T[]
                function ents.FindByClass(class)
                end
            "#,
        );
        let caller_id = ws.def_file(
            "caller.lua",
            r#"local found = ents.FindByClass("prop_vehicle_jeep")"#,
        );

        let db = ws.get_db_mut();
        let baseline = special_call_snapshot(db);
        assert!(
            !baseline.is_empty(),
            "the fixture must settle special-call facts"
        );
        let calls = db
            .get_vfs()
            .get_syntax_tree(&caller_id)
            .expect("expected syntax tree")
            .get_chunk_node()
            .descendants::<glua_parser::LuaCallExpr>()
            .collect::<Vec<_>>();
        assert!(!calls.is_empty(), "expected the constructor call");

        for call_expr in &calls {
            let mut item = UnResolveSpecialCall {
                file_id: caller_id,
                call_expr: call_expr.clone(),
            };
            let mut cache = crate::LuaInferCache::new(caller_id, Default::default());
            // An already-installed `__call` reports itself instead of
            // reinstalling; either way nothing changes.
            let _ = try_resolve_special_call(db, &mut cache, &mut item);
        }
        assert_eq!(
            special_call_snapshot(db),
            baseline,
            "replaying a settled constructor call changes nothing"
        );
    }

    fn variadic_multi(types: Vec<LuaType>) -> LuaType {
        LuaType::Variadic(crate::VariadicType::Multi(types).into())
    }

    #[test]
    fn unsettled_inferred_return_reaches_into_multi_slots() {
        assert!(super::is_unsettled_inferred_return(&variadic_multi(vec![
            LuaType::Unknown,
            LuaType::Integer
        ])));
        assert!(!super::is_unsettled_inferred_return(&variadic_multi(vec![
            LuaType::String,
            LuaType::Integer
        ])));
    }

    #[test]
    fn inferred_return_improves_partial_multi_slots() {
        assert!(super::inferred_return_improves(
            &variadic_multi(vec![LuaType::Unknown, LuaType::Unknown]),
            &variadic_multi(vec![LuaType::Unknown, LuaType::Integer])
        ));
    }

    #[test]
    fn inferred_return_rejects_regressing_slot() {
        assert!(!super::inferred_return_improves(
            &variadic_multi(vec![LuaType::String, LuaType::Unknown]),
            &variadic_multi(vec![LuaType::Unknown, LuaType::Integer])
        ));
    }

    #[test]
    fn inferred_return_rejects_equal_multi() {
        assert!(!super::inferred_return_improves(
            &variadic_multi(vec![LuaType::Unknown, LuaType::Integer]),
            &variadic_multi(vec![LuaType::Unknown, LuaType::Integer])
        ));
    }

    #[test]
    fn inferred_return_preserves_unknown_any_direction() {
        assert!(super::inferred_return_improves(
            &LuaType::Unknown,
            &LuaType::Any
        ));
        assert!(!super::inferred_return_improves(
            &LuaType::Any,
            &LuaType::Unknown
        ));
        assert!(super::inferred_return_improves(
            &LuaType::Unknown,
            &LuaType::Union(Arc::new(LuaUnionType::from_vec(vec![
                LuaType::Unknown,
                LuaType::Nil,
            ])))
        ));
    }
}
