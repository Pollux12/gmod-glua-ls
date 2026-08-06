use glua_parser::{
    BinaryOperator, LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaFuncStat, LuaIndexKey,
    LuaTableExpr, LuaTableField, LuaVarExpr,
};

use crate::{
    DbIndex, InFiled, InferFailReason, LuaDeclExtra, LuaInferCache, LuaInstanceType, LuaMemberKey,
    LuaMemberOwner, LuaType, LuaUnionType, SemanticDeclLevel, infer_expr,
    semantic::{
        SemanticDeclGuard, get_member_value_expr, infer::InferResult, infer_expr_semantic_decl,
        member::find_members_with_key,
    },
};

pub fn infer_setmetatable_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: LuaCallExpr,
) -> InferResult {
    let arg_list = call_expr.get_args_list().ok_or(InferFailReason::None)?;
    let args = arg_list.get_args().collect::<Vec<LuaExpr>>();

    if args.len() != 2 {
        return Ok(LuaType::Any);
    }

    let basic_table = args[0].clone();
    let metatable = args[1].clone();

    // A metatable whose own type is not inferred yet must not be answered
    // with the bare table: that fallback is indistinguishable from "this
    // metatable really has no `__index`", and committing it freezes the
    // wrong result for good. Deferring lets the resolution machinery retry
    // once the metatable has a type — which is how a self-referential
    // `setmetatable(T, T)` constructor resolves at all.
    let (meta_type, is_index) = match infer_metatable_index_type(db, cache, metatable.clone())? {
        MetatableIndex::Unresolved => {
            return Err(InferFailReason::UnResolveExpr(InFiled::new(
                cache.get_file_id(),
                metatable,
            )));
        }
        MetatableIndex::Index(index_type) => (index_type, true),
        MetatableIndex::NoIndex(meta_type) => (meta_type, false),
    };
    match &basic_table {
        LuaExpr::TableExpr(table_expr) => {
            if table_expr.is_empty() && is_index {
                return Ok(meta_type);
            }

            if is_index {
                return Ok(LuaType::Instance(
                    LuaInstanceType::new(
                        meta_type,
                        InFiled::new(cache.get_file_id(), table_expr.get_range()),
                    )
                    .into(),
                ));
            }

            Ok(LuaType::TableConst(InFiled::new(
                cache.get_file_id(),
                table_expr.get_range(),
            )))
        }
        _ => {
            if meta_type.is_unknown() {
                return infer_expr(db, cache, basic_table);
            }

            if is_index && let Some(range) = resolve_table_backing_range(db, cache, &basic_table) {
                return Ok(LuaType::Instance(
                    LuaInstanceType::new(meta_type, range).into(),
                ));
            }

            Ok(meta_type)
        }
    }
}

fn resolve_table_backing_range(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: &LuaExpr,
) -> Option<InFiled<rowan::TextRange>> {
    if let Some(range) = table_backing_range_from_expr(cache.get_file_id(), expr) {
        return Some(range);
    }

    let semantic_decl = infer_expr_semantic_decl(
        db,
        cache,
        expr.clone(),
        SemanticDeclGuard::default(),
        SemanticDeclLevel::default(),
    )?;
    let (value_file_id, value_expr) = match semantic_decl {
        crate::LuaSemanticDeclId::LuaDecl(decl_id) => {
            let root = db
                .get_vfs()
                .get_syntax_tree(&decl_id.file_id)?
                .get_red_root();
            (
                decl_id.file_id,
                db.get_decl_index()
                    .get_decl(&decl_id)?
                    .get_value_syntax_id()?
                    .to_node_from_root(&root)
                    .and_then(LuaExpr::cast)?,
            )
        }
        crate::LuaSemanticDeclId::Member(member_id) => {
            (member_id.file_id, get_member_value_expr(db, member_id)?)
        }
        _ => return None,
    };

    table_backing_range_from_expr(value_file_id, &value_expr)
}

fn table_backing_range_from_expr(
    file_id: crate::FileId,
    expr: &LuaExpr,
) -> Option<InFiled<rowan::TextRange>> {
    match expr {
        LuaExpr::TableExpr(table_expr) => Some(InFiled::new(file_id, table_expr.get_range())),
        LuaExpr::ParenExpr(paren_expr) => {
            table_backing_range_from_expr(file_id, &paren_expr.get_expr()?)
        }
        LuaExpr::BinaryExpr(binary_expr)
            if binary_expr.get_op_token().map(|op| op.get_op()) == Some(BinaryOperator::OpOr) =>
        {
            let (_, right) = binary_expr.get_exprs()?;
            table_backing_range_from_expr(file_id, &right)
        }
        _ => None,
    }
}

// wrong implementation, should be removed
// fn meta_type_contain_table(
//     db: &DbIndex,
//     cache: &mut LuaInferCache,
//     meta_type: LuaType,
//     table_expr: LuaTableExpr,
// ) -> Option<LuaType> {
//     let meta_members =
//         find_members_with_key(db, &meta_type, LuaMemberKey::Name("__index".into()), true)?;
//     for member in meta_members {
//         let index_members = find_members(db, &member.typ)?;
//         let table_type = infer_expr(db, cache, LuaExpr::TableExpr(table_expr.clone())).ok()?;
//         let table_members = find_members(db, &table_type)?;
//         // 如果 index_members 包含了 table_members 中的所有成员，则返回 meta_type
//         if table_members.iter().all(|table_member| {
//             index_members
//                 .iter()
//                 .any(|index_member| index_member.key.to_path() == table_member.key.to_path())
//         }) {
//             return Some(meta_type);
//         }
//     }
//     None
// }

/// Outcome of looking for a metatable's `__index`.
enum MetatableIndex {
    /// `__index` resolved to this type.
    Index(LuaType),
    /// The metatable resolved, and it has no usable `__index`.
    NoIndex(LuaType),
    /// The metatable's own type has not been inferred yet.
    Unresolved,
}

fn infer_metatable_index_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    metatable: LuaExpr,
) -> Result<MetatableIndex, InferFailReason> {
    let metatable_expr = metatable.clone();
    if let LuaExpr::TableExpr(table) = &metatable {
        if let Some(index_value) = last_table_literal_index_value(table) {
            if matches!(
                index_value,
                LuaExpr::TableExpr(_)
                    | LuaExpr::CallExpr(_)
                    | LuaExpr::IndexExpr(_)
                    | LuaExpr::NameExpr(_)
            ) {
                return Ok(MetatableIndex::Index(infer_expr(db, cache, index_value)?));
            }

            let inferred_type = infer_expr(db, cache, index_value.clone()).ok();
            let index_type = inferred_type
                .as_ref()
                .filter(|typ| !typ.is_unknown())
                .cloned()
                .or_else(|| {
                    resolve_table_backing_range(db, cache, &index_value).map(LuaType::TableConst)
                })
                .or(inferred_type);
            if let Some(index_type) = index_type {
                return Ok(match classify_metatable_index_candidate(&index_type) {
                    MetatableIndexCandidate::Supported(index_type) => {
                        MetatableIndex::Index(index_type)
                    }
                    MetatableIndexCandidate::Unsupported => {
                        MetatableIndex::NoIndex(LuaType::Unknown)
                    }
                });
            }
        }
    }

    let meta_type = match infer_receiver_owner_type(db, cache, &metatable) {
        ReceiverOwnerType::Exact(owner_type) => owner_type,
        ReceiverOwnerType::Rejected => return Ok(MetatableIndex::NoIndex(LuaType::Unknown)),
        ReceiverOwnerType::NotReceiver => infer_expr(db, cache, metatable)?,
    };
    match exact_table_index_type(db, cache, &meta_type) {
        ExactMetatableIndexType::Exact(index_type) => {
            return Ok(MetatableIndex::Index(index_type));
        }
        ExactMetatableIndexType::Rejected => return Ok(MetatableIndex::NoIndex(LuaType::Unknown)),
        ExactMetatableIndexType::None => {}
    }

    if let Some(meta_members) =
        find_members_with_key(db, &meta_type, LuaMemberKey::Name("__index".into()), false)
    {
        let mut index_types = Vec::with_capacity(meta_members.len());
        for meta_member in meta_members {
            match classify_metatable_index_candidate(&meta_member.typ) {
                MetatableIndexCandidate::Supported(index_type) => index_types.push(index_type),
                MetatableIndexCandidate::Unsupported => {
                    return Ok(MetatableIndex::NoIndex(LuaType::Unknown));
                }
            }
        }

        if let Some(index_type) = index_types.first() {
            if index_types.iter().all(|candidate| candidate == index_type) {
                return Ok(MetatableIndex::Index(index_type.clone()));
            }
            return Ok(MetatableIndex::NoIndex(LuaType::Unknown));
        }
    }

    // No `__index` was found. Whether that is an answer depends on whether
    // the metatable itself is known.
    if meta_type.is_unknown()
        && metatable_is_receiver_field(db, cache, &metatable_expr)
        && !metatable_already_finalised(db, cache, &metatable_expr)
    {
        return Ok(MetatableIndex::Unresolved);
    }
    Ok(MetatableIndex::NoIndex(meta_type))
}

/// Whether the resolution machinery has already given up on this
/// expression.
fn metatable_already_finalised(db: &DbIndex, cache: &LuaInferCache, metatable: &LuaExpr) -> bool {
    let owner =
        crate::LuaTypeOwner::SyntaxId(InFiled::new(cache.get_file_id(), metatable.get_syntax_id()));
    db.get_type_index().get_type_cache(&owner).is_some()
}

/// Whether the metatable expression is a field reached from the enclosing
/// function's receiver.
fn metatable_is_receiver_field(db: &DbIndex, cache: &LuaInferCache, metatable: &LuaExpr) -> bool {
    let LuaExpr::IndexExpr(index_expr) = metatable else {
        return false;
    };
    match index_expr.get_prefix_expr() {
        // `self.a.b` is still rooted at the receiver.
        Some(prefix @ LuaExpr::IndexExpr(_)) => metatable_is_receiver_field(db, cache, &prefix),
        Some(LuaExpr::NameExpr(name_expr)) => {
            if name_expr.get_name_text().as_deref() != Some("self") {
                return false;
            }
            let Some(decl_id) = db
                .get_reference_index()
                .get_var_reference_decl(&cache.get_file_id(), name_expr.get_range())
            else {
                return false;
            };
            db.get_decl_index().get_decl(&decl_id).is_some_and(|decl| {
                matches!(
                    &decl.extra,
                    LuaDeclExtra::Param { idx: 0, .. } | LuaDeclExtra::ImplicitSelf { .. }
                )
            })
        }
        _ => false,
    }
}

enum ReceiverOwnerType {
    NotReceiver,
    Rejected,
    Exact(LuaType),
}

fn infer_receiver_owner_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    metatable: &LuaExpr,
) -> ReceiverOwnerType {
    let LuaExpr::NameExpr(name_expr) = metatable else {
        return ReceiverOwnerType::NotReceiver;
    };
    let Some(decl_id) = db
        .get_reference_index()
        .get_var_reference_decl(&cache.get_file_id(), name_expr.get_range())
    else {
        return ReceiverOwnerType::NotReceiver;
    };
    let Some(decl) = db.get_decl_index().get_decl(&decl_id) else {
        return ReceiverOwnerType::NotReceiver;
    };
    let (signature_id, implicit_self) = match &decl.extra {
        LuaDeclExtra::Param {
            idx: 0,
            signature_id,
            ..
        } => (*signature_id, false),
        LuaDeclExtra::ImplicitSelf { .. } => {
            let Some(closure) = name_expr
                .syntax()
                .ancestors()
                .find_map(LuaClosureExpr::cast)
            else {
                return ReceiverOwnerType::Rejected;
            };
            (
                crate::LuaSignatureId::from_closure(cache.get_file_id(), &closure),
                true,
            )
        }
        _ => return ReceiverOwnerType::NotReceiver,
    };
    let Some(signature) = db.get_signature_index().get(&signature_id) else {
        return ReceiverOwnerType::Rejected;
    };

    if implicit_self && signature.is_colon_define {
        if decl_is_mutated(db, &decl_id) {
            return ReceiverOwnerType::Rejected;
        }
        let Some(func_stat) = name_expr.syntax().ancestors().find_map(LuaFuncStat::cast) else {
            return ReceiverOwnerType::Rejected;
        };
        let Some(LuaVarExpr::IndexExpr(func_name)) = func_stat.get_func_name() else {
            return ReceiverOwnerType::Rejected;
        };
        let Some(prefix) = func_name.get_prefix_expr() else {
            return ReceiverOwnerType::Rejected;
        };
        return infer_expr(db, cache, prefix)
            .map(ReceiverOwnerType::Exact)
            .unwrap_or(ReceiverOwnerType::Rejected);
    }
    if implicit_self || signature.is_colon_define {
        return ReceiverOwnerType::NotReceiver;
    }

    let Some(closure) = name_expr
        .syntax()
        .ancestors()
        .find_map(LuaClosureExpr::cast)
    else {
        return ReceiverOwnerType::NotReceiver;
    };
    if crate::LuaSignatureId::from_closure(cache.get_file_id(), &closure) != signature_id {
        return ReceiverOwnerType::NotReceiver;
    }
    let Some(field) = closure.syntax().parent().and_then(LuaTableField::cast) else {
        return ReceiverOwnerType::NotReceiver;
    };
    let Some(LuaIndexKey::Name(key)) = field.get_field_key() else {
        return ReceiverOwnerType::NotReceiver;
    };
    if key.get_name_text() != "__call" {
        return ReceiverOwnerType::NotReceiver;
    }
    let Some(owner) = field.syntax().parent().and_then(LuaTableExpr::cast) else {
        return ReceiverOwnerType::NotReceiver;
    };
    if decl_is_mutated(db, &decl_id) {
        return ReceiverOwnerType::Rejected;
    }
    ReceiverOwnerType::Exact(LuaType::TableConst(InFiled::new(
        cache.get_file_id(),
        owner.get_range(),
    )))
}

fn decl_is_mutated(db: &DbIndex, decl_id: &crate::LuaDeclId) -> bool {
    db.get_reference_index()
        .get_decl_references(&decl_id.file_id, decl_id)
        .is_some_and(|references| references.mutable)
}

enum ExactMetatableIndexType {
    None,
    Rejected,
    Exact(LuaType),
}

enum MetatableIndexCandidate {
    Unsupported,
    Supported(LuaType),
}

fn exact_table_index_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    meta_type: &LuaType,
) -> ExactMetatableIndexType {
    let table_range = match meta_type {
        LuaType::TableConst(range) => range,
        LuaType::Instance(instance) => instance.get_range(),
        _ => return ExactMetatableIndexType::None,
    };
    let owner = LuaMemberOwner::Element(table_range.clone());
    let key = LuaMemberKey::Name("__index".into());
    let mut member_ids = db
        .get_member_index()
        .get_members_for_owner_key(&owner, &key)
        .into_iter()
        .map(|member| member.get_id())
        .collect::<Vec<_>>();
    member_ids.sort_by_key(|id| (id.file_id.id, u32::from(id.get_position())));
    if member_ids.is_empty() {
        let Some(index_type) = last_table_literal_index_type(db, cache, table_range) else {
            return ExactMetatableIndexType::None;
        };
        return match classify_metatable_index_candidate(&index_type) {
            MetatableIndexCandidate::Supported(index_type) => {
                ExactMetatableIndexType::Exact(index_type)
            }
            MetatableIndexCandidate::Unsupported => ExactMetatableIndexType::Rejected,
        };
    }

    let mut index_types = Vec::new();
    for member_id in member_ids {
        let Some(index_type) = member_metatable_index_type(db, cache, member_id) else {
            return ExactMetatableIndexType::Rejected;
        };
        match classify_metatable_index_candidate(&index_type) {
            MetatableIndexCandidate::Supported(index_type) => index_types.push(index_type),
            MetatableIndexCandidate::Unsupported => return ExactMetatableIndexType::Rejected,
        }
    }

    let Some(index_type) = index_types.first() else {
        return ExactMetatableIndexType::None;
    };
    if index_types.iter().all(|candidate| candidate == index_type) {
        ExactMetatableIndexType::Exact(index_type.clone())
    } else {
        ExactMetatableIndexType::Rejected
    }
}

fn member_metatable_index_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    member_id: crate::LuaMemberId,
) -> Option<LuaType> {
    let value_expr = get_member_value_expr(db, member_id)?;
    if member_id.file_id == cache.get_file_id() {
        return infer_expr(db, cache, value_expr.clone())
            .ok()
            .filter(|typ| !typ.is_unknown())
            .or_else(|| {
                resolve_table_backing_range(db, cache, &value_expr).map(LuaType::TableConst)
            });
    }

    if let Some(cached) = db.get_type_index().get_type_cache(&member_id.into()) {
        let cached_type = cached.as_type().clone();
        if !cached_type.is_unknown() {
            return Some(cached_type);
        }
    }

    let mut definition_cache = LuaInferCache::new(member_id.file_id, cache.get_config().clone());
    infer_expr(db, &mut definition_cache, value_expr.clone())
        .ok()
        .filter(|typ| !typ.is_unknown())
        .or_else(|| {
            resolve_table_backing_range(db, &mut definition_cache, &value_expr)
                .map(LuaType::TableConst)
        })
}

fn last_table_literal_index_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    table_range: &InFiled<rowan::TextRange>,
) -> Option<LuaType> {
    let root = db
        .get_vfs()
        .get_syntax_tree(&table_range.file_id)?
        .get_red_root();
    let table = root
        .token_at_offset(table_range.value.start())
        .right_biased()?
        .parent_ancestors()
        .find_map(LuaTableExpr::cast)
        .filter(|table| table.get_range() == table_range.value)?;
    let index_value = last_table_literal_index_value(&table)?;

    if table_range.file_id == cache.get_file_id() {
        return infer_expr(db, cache, index_value.clone())
            .ok()
            .filter(|typ| !typ.is_unknown())
            .or_else(|| {
                resolve_table_backing_range(db, cache, &index_value).map(LuaType::TableConst)
            });
    }

    let mut definition_cache = LuaInferCache::new(table_range.file_id, cache.get_config().clone());
    infer_expr(db, &mut definition_cache, index_value.clone())
        .ok()
        .filter(|typ| !typ.is_unknown())
        .or_else(|| {
            resolve_table_backing_range(db, &mut definition_cache, &index_value)
                .map(LuaType::TableConst)
        })
}

fn last_table_literal_index_value(table: &LuaTableExpr) -> Option<LuaExpr> {
    let fields = table.get_fields().collect::<Vec<_>>();
    fields.into_iter().rev().find_map(|field| {
        let key = field.get_field_key()?;
        let is_index = match key {
            LuaIndexKey::Name(key) => key.get_name_text() == "__index",
            LuaIndexKey::String(key) => key.get_value() == "__index",
            _ => false,
        };
        is_index.then(|| field.get_value_expr()).flatten()
    })
}

fn classify_metatable_index_candidate(typ: &LuaType) -> MetatableIndexCandidate {
    match typ {
        LuaType::Union(union) => match union.as_ref() {
            LuaUnionType::Nullable(inner) => classify_metatable_index_candidate(inner),
            LuaUnionType::Multi(types) => {
                let mut supported_types = Vec::new();
                for typ in types.iter().filter(|typ| !typ.is_nil()) {
                    match classify_metatable_index_candidate(typ) {
                        MetatableIndexCandidate::Supported(typ) => supported_types.push(typ),
                        MetatableIndexCandidate::Unsupported => {
                            return MetatableIndexCandidate::Unsupported;
                        }
                    }
                }
                let Some(index_type) = supported_types.first() else {
                    return MetatableIndexCandidate::Unsupported;
                };
                if supported_types
                    .iter()
                    .all(|candidate| candidate == index_type)
                {
                    MetatableIndexCandidate::Supported(index_type.clone())
                } else {
                    MetatableIndexCandidate::Unsupported
                }
            }
        },
        LuaType::TypeGuard(inner) => classify_metatable_index_candidate(inner),
        LuaType::Instance(instance) => {
            match classify_metatable_index_candidate(instance.get_base()) {
                MetatableIndexCandidate::Supported(_) => {
                    MetatableIndexCandidate::Supported(typ.clone())
                }
                MetatableIndexCandidate::Unsupported => MetatableIndexCandidate::Unsupported,
            }
        }
        _ if typ.is_table() || typ.is_custom_type() || typ.is_object() => {
            MetatableIndexCandidate::Supported(typ.clone())
        }
        _ => MetatableIndexCandidate::Unsupported,
    }
}
