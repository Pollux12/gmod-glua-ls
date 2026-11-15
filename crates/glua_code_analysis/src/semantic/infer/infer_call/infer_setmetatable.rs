use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAstNode, LuaCallArgList, LuaCallExpr, LuaClosureExpr,
    LuaExpr, LuaFuncStat, LuaIndexExpr, LuaIndexKey, LuaNameExpr, LuaStat, LuaTableExpr,
    LuaTableField, LuaVarExpr,
};

use crate::semantic::semantic_decl_value;
use crate::{
    DbIndex, InFiled, InferFailReason, LuaDeclExtra, LuaDeclId, LuaInferCache, LuaInstanceType,
    LuaMemberKey, LuaMemberOwner, LuaType, SemanticDeclLevel, infer_expr,
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

    // Not a call shape this can read: that is a statement about the analyzer,
    // not a promise that the result accepts anything.
    if args.len() != 2 {
        return Ok(LuaType::Unknown);
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
                metatable.get_syntax_id(),
            )));
        }
        MetatableIndex::Index(index_type) => (index_type, true),
        MetatableIndex::NoIndex(meta_type) => (meta_type, false),
    };
    match &basic_table {
        LuaExpr::TableExpr(table_expr) => {
            if table_expr.is_empty() && is_index && !result_used_as_class(db, cache, &call_expr) {
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

/// Whether the table `setmetatable({}, mt)` makes is used as a class of its
/// own: the variable holding it gets its own `__index`, or is the metatable of
/// another `setmetatable` call. Such a table is a subclass that keeps its own
/// members; a plain instance shares its class's.
fn result_used_as_class(db: &DbIndex, cache: &LuaInferCache, call_expr: &LuaCallExpr) -> bool {
    let Some(holder) = call_expr.get_parent::<LuaStat>() else {
        return false;
    };
    let Some(root) = call_expr.syntax().ancestors().last() else {
        return false;
    };
    let file_id = cache.get_file_id();
    match holder {
        LuaStat::LocalStat(local_stat) => {
            let Some(name) = local_stat.get_local_name_list().next() else {
                return false;
            };
            let decl_id = LuaDeclId::new(file_id, name.get_position());
            db.get_reference_index()
                .get_decl_references(&file_id, &decl_id)
                .is_some_and(|references| {
                    references.cells.iter().any(|cell| {
                        root.covering_element(cell.range)
                            .parent()
                            .and_then(LuaNameExpr::cast)
                            .is_some_and(|use_| is_class_use(&use_))
                    })
                })
        }
        LuaStat::AssignStat(assign_stat) => {
            let (vars, _) = assign_stat.get_var_and_expr_list();
            let Some(LuaVarExpr::NameExpr(global)) = vars.first() else {
                return false;
            };
            let Some(name) = global.get_name_text() else {
                return false;
            };
            root.descendants()
                .filter_map(LuaNameExpr::cast)
                .any(|use_| use_.get_name_text() == Some(name.clone()) && is_class_use(&use_))
        }
        _ => false,
    }
}

/// `name.__index = ...` or `setmetatable(x, name)`.
fn is_class_use(name: &LuaNameExpr) -> bool {
    let Some(parent) = name.syntax().parent() else {
        return false;
    };
    if let Some(index_expr) = LuaIndexExpr::cast(parent.clone()) {
        return index_expr
            .get_name_token()
            .is_some_and(|key| key.get_name_text() == "__index")
            && index_expr
                .get_parent::<LuaAssignStat>()
                .is_some_and(|assign| {
                    assign
                        .get_var_and_expr_list()
                        .0
                        .iter()
                        .any(|var| var.syntax() == index_expr.syntax())
                });
    }
    let Some(args) = LuaCallArgList::cast(parent) else {
        return false;
    };
    args.get_args().nth(1).is_some_and(|arg| arg.syntax() == name.syntax())
        && args
            .get_parent::<LuaCallExpr>()
            .and_then(|call| call.get_prefix_expr())
            .is_some_and(|prefix| {
                matches!(prefix, LuaExpr::NameExpr(callee) if callee.get_name_text().as_deref() == Some("setmetatable"))
            })
}

/// `getmetatable(t)` for a table `setmetatable` gave a metatable, or a
/// proxy `newproxy` gave one: that metatable. `None` for any other call,
/// which then reads as declared.
pub fn infer_getmetatable_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> Option<LuaType> {
    getmetatable_call_metatable(db, cache, call_expr).map(|metatable| metatable_type(db, metatable))
}

/// The table a `getmetatable(t)` call returns, before any registration made
/// a class of it.
pub(crate) fn getmetatable_call_metatable(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> Option<InFiled<rowan::TextRange>> {
    let LuaExpr::NameExpr(name) = call_expr.get_prefix_expr()? else {
        return None;
    };
    if name.get_name_text()? != "getmetatable" {
        return None;
    }
    let [table]: [LuaExpr; 1] = call_expr
        .get_args_list()?
        .get_args()
        .collect::<Vec<_>>()
        .try_into()
        .ok()?;
    if let Some(metatable) = proxy_metatable_range(db, cache, &table, &mut Vec::new()) {
        return Some(metatable);
    }
    let range = resolve_table_backing_range(db, cache, &table)
        .or_else(|| setmetatable_target_range(db, cache, &table))?;
    db.get_metatable_index().get(&range).cloned()
}

/// The class a registration made of `metatable`, or the table itself.
fn metatable_type(db: &DbIndex, metatable: InFiled<rowan::TextRange>) -> LuaType {
    db.get_type_index()
        .table_class(&metatable)
        .cloned()
        .unwrap_or(LuaType::TableConst(metatable))
}

/// `newproxy(...)` for a proxy whose metatable has a table `__index`: what
/// reads on the proxy reach. `None` for any other call, which then reads as
/// declared.
pub fn infer_newproxy_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> Option<LuaType> {
    let call = LuaExpr::CallExpr(call_expr.clone());
    let metatable = proxy_metatable_range(db, cache, &call, &mut Vec::new())?;
    match index_type_from_writers(db, &metatable_type(db, metatable))? {
        MetatableIndex::Index(index_type) => Some(index_type),
        MetatableIndex::NoIndex(_) | MetatableIndex::Unresolved => None,
    }
}

/// The metatable of the proxy `expr` is, or evaluates to.
pub(crate) fn proxy_metatable(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: &LuaExpr,
) -> Option<InFiled<rowan::TextRange>> {
    proxy_metatable_range(db, cache, expr, &mut Vec::new())
}

/// The metatable of the proxy `expr` is, or evaluates to. `newproxy(true)`
/// creates a metatable, named here by the call's range; `newproxy(p)`
/// shares p's.
fn proxy_metatable_range(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: &LuaExpr,
    visited: &mut Vec<InFiled<rowan::TextRange>>,
) -> Option<InFiled<rowan::TextRange>> {
    let (file_id, value) = match expr {
        LuaExpr::CallExpr(_) => (cache.get_file_id(), expr.clone()),
        LuaExpr::ParenExpr(paren_expr) => {
            return proxy_metatable_range(db, cache, &paren_expr.get_expr()?, visited);
        }
        _ => semantic_decl_value(
            db,
            infer_expr_semantic_decl(
                db,
                cache,
                expr.clone(),
                SemanticDeclGuard::default(),
                SemanticDeclLevel::default(),
            )?,
        )?,
    };
    let value_range = InFiled::new(file_id, value.get_range());
    if visited.contains(&value_range) {
        return None;
    }
    visited.push(value_range);
    if file_id != cache.get_file_id() {
        let mut value_cache = LuaInferCache::new(file_id, cache.get_config().clone());
        return proxy_metatable_of_value(db, &mut value_cache, &value, visited);
    }
    proxy_metatable_of_value(db, cache, &value, visited)
}

fn proxy_metatable_of_value(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    value: &LuaExpr,
    visited: &mut Vec<InFiled<rowan::TextRange>>,
) -> Option<InFiled<rowan::TextRange>> {
    let LuaExpr::CallExpr(call) = value else {
        return proxy_metatable_range(db, cache, value, visited);
    };
    let LuaExpr::NameExpr(name) = call.get_prefix_expr()? else {
        return None;
    };
    if name.get_name_text()? != "newproxy" {
        return None;
    }
    let arg = call.get_args_list()?.get_args().next()?;
    match infer_expr(db, cache, arg.clone()).ok()? {
        LuaType::BooleanConst(true) | LuaType::DocBooleanConst(true) => {
            Some(InFiled::new(cache.get_file_id(), call.get_range()))
        }
        LuaType::BooleanConst(false) | LuaType::DocBooleanConst(false) => None,
        _ => proxy_metatable_range(db, cache, &arg, visited),
    }
}

/// The table literal a `setmetatable(t, mt)` call `expr` evaluates to, or is
/// the value of: the call returns `t`.
fn setmetatable_target_range(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: &LuaExpr,
) -> Option<InFiled<rowan::TextRange>> {
    let (file_id, value) = match expr {
        LuaExpr::CallExpr(_) => (cache.get_file_id(), expr.clone()),
        _ => semantic_decl_value(
            db,
            infer_expr_semantic_decl(
                db,
                cache,
                expr.clone(),
                SemanticDeclGuard::default(),
                SemanticDeclLevel::default(),
            )?,
        )?,
    };
    let LuaExpr::CallExpr(call) = value else {
        return None;
    };
    if !call.is_setmetatable() {
        return None;
    }
    let target = call.get_args_list()?.get_args().next()?;
    table_backing_range_from_expr(file_id, &target)
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
    let (value_file_id, value_expr) = semantic_decl_value(db, semantic_decl)?;

    table_backing_range_from_expr(value_file_id, &value_expr)
}

/// The table literal `expr` evaluates to, read through parentheses and the right of `or`.
pub(crate) fn table_backing_range_from_expr(
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
//         // If index_members contains all members of table_members, return meta_type
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
                let index_type = infer_expr(db, cache, index_value)?;
                if index_type.is_function() {
                    return Ok(MetatableIndex::NoIndex(LuaType::Unknown));
                }
                return Ok(MetatableIndex::Index(index_type));
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
    match exact_table_index_type(db, cache, &meta_type, metatable_expr.get_position()) {
        ExactMetatableIndexType::Exact(index_type) => {
            return Ok(MetatableIndex::Index(index_type));
        }
        ExactMetatableIndexType::Rejected => return Ok(MetatableIndex::NoIndex(LuaType::Unknown)),
        ExactMetatableIndexType::None => {}
    }

    if let Some(index) = index_type_from_writers(db, &meta_type) {
        return Ok(index);
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

/// The `__index` every writer of it on `meta_type` agrees on, so writers that
/// disagree are seen to disagree. `None` when nothing writes `__index`.
fn index_type_from_writers(db: &DbIndex, meta_type: &LuaType) -> Option<MetatableIndex> {
    let meta_members =
        find_members_with_key(db, meta_type, LuaMemberKey::Name("__index".into()), true)?;
    let mut index_types = Vec::with_capacity(meta_members.len());
    for meta_member in meta_members {
        match classify_metatable_index_candidate(&meta_member.typ) {
            MetatableIndexCandidate::Supported(index_type) => index_types.push(index_type),
            MetatableIndexCandidate::Unsupported => {
                return Some(MetatableIndex::NoIndex(LuaType::Unknown));
            }
        }
    }
    let index_type = index_types.first()?;
    Some(
        if index_types.iter().all(|candidate| candidate == index_type) {
            MetatableIndex::Index(index_type.clone())
        } else {
            MetatableIndex::NoIndex(LuaType::Unknown)
        },
    )
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
    ReceiverOwnerType::NotReceiver
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

pub(crate) enum MetatableIndexCandidate {
    Unsupported,
    Supported(LuaType),
}

fn exact_table_index_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    meta_type: &LuaType,
    caller_position: rowan::TextSize,
) -> ExactMetatableIndexType {
    let table_range = match meta_type {
        LuaType::TableConst(range) => range,
        LuaType::Instance(instance) => instance.get_range(),
        _ => return ExactMetatableIndexType::None,
    };
    let owner = LuaMemberOwner::Element(table_range.clone());
    let key = LuaMemberKey::Name("__index".into());
    // The writers a reader at the call sees: a later plain write in the same
    // file has replaced the earlier ones by then.
    let mut member_ids = crate::LuaMemberIndexItem::Many(
        db.get_member_index()
            .get_members_for_owner_key(&owner, &key)
            .into_iter()
            .map(|member| member.get_id())
            .collect(),
    )
    .visible_member_ids_with_realm_at_offset(db, &cache.get_file_id(), caller_position);
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
        .syntax_tree_around(&table_range.file_id, table_range.value)?
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

pub(crate) fn classify_metatable_index_candidate(typ: &LuaType) -> MetatableIndexCandidate {
    match typ {
        LuaType::Union(union) => match union.nullable_inner() {
            Some(inner) => classify_metatable_index_candidate(inner),
            None => {
                let mut supported_types = Vec::new();
                for typ in union.types().filter(|typ| !typ.is_nil()) {
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
