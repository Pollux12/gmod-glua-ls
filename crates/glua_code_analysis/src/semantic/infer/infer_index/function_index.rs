use glua_parser::{
    BinaryOperator, LuaAstNode, LuaClosureExpr, LuaExpr, LuaIndexKey, LuaNameExpr, LuaReturnStat,
};

use crate::db_index::widen_literal_type_for_assignment;
use crate::semantic::member::LuaMemberInfo;
use crate::{
    DbIndex, LuaDeclExtra, LuaInferCache, LuaMemberKey, LuaMemberOwner, LuaSemanticDeclId,
    LuaSignatureId, LuaType, TypeOps, infer_expr,
    semantic::{local_initializer_expr, member::find_members_with_key},
};

/// What a read of `key` gets from an `__index` function: a key the table
/// does not hold reaches `__index(self, key)`. Each `return P[key]` or
/// `rawget(P, key)` (see [`collect_key_sources`]) gives `P`'s field `key`,
/// widened as the stored value can change. Without such a return the read is what the
/// function returns.
pub(super) fn infer_function_index_member(
    db: &DbIndex,
    cache: &LuaInferCache,
    index_function: &LuaType,
    key: &LuaIndexKey,
) -> Option<LuaType> {
    let LuaType::Signature(signature_id) = index_function else {
        return None;
    };
    let key_name = match key {
        LuaIndexKey::Name(name) => name.get_name_text().to_string(),
        LuaIndexKey::String(string) => string.get_value(),
        _ => return Some(LuaType::Unknown),
    };
    let closure = signature_closure(db, signature_id)?;
    let signature = db.get_signature_index().get(signature_id)?;
    let key_param_idx = if signature.is_colon_define { 0 } else { 1 };

    let file_id = signature_id.get_file_id();
    let mut value_cache = LuaInferCache::new(file_id, cache.get_config().clone());
    let mut field_types = Vec::new();
    for return_stat in closure.descendants::<LuaReturnStat>() {
        if return_stat.ancestors::<LuaClosureExpr>().next().as_ref() != Some(&closure) {
            continue;
        }
        let Some(returned) = return_stat.get_expr_list().next() else {
            continue;
        };
        let mut sources = Vec::new();
        collect_key_sources(db, signature_id, key_param_idx, returned, &mut sources);
        for source in sources {
            let Ok(source_type) = infer_expr(db, &mut value_cache, source) else {
                continue;
            };
            let members = find_members_with_key(
                db,
                &source_type,
                LuaMemberKey::Name(key_name.as_str().into()),
                true,
            );
            field_types.extend(
                members
                    .into_iter()
                    .flatten()
                    .map(|member| widen_literal_type_for_assignment(&member.typ)),
            );
        }
    }

    if let Some(field_type) = field_types
        .into_iter()
        .reduce(|left, right| TypeOps::Union.apply(db, &left, &right))
    {
        return Some(field_type);
    }
    match signature.get_return_type() {
        LuaType::Nil => Some(LuaType::Unknown),
        returned => Some(returned),
    }
}

/// The tables `returned` reads the key param from: `P` of `P[key]` or
/// `rawget(P, key)`, also through a local holding it or either side of `or`.
fn collect_key_sources(
    db: &DbIndex,
    signature_id: &LuaSignatureId,
    key_param_idx: usize,
    returned: LuaExpr,
    sources: &mut Vec<LuaExpr>,
) {
    let is_key = |expr: Option<LuaExpr>| matches!(expr, Some(LuaExpr::NameExpr(name)) if is_param(db, signature_id, &name, key_param_idx));
    match unaliased(db, signature_id, returned) {
        Some(LuaExpr::IndexExpr(lookup)) => {
            let key = match lookup.get_index_key() {
                Some(LuaIndexKey::Expr(key)) => Some(key),
                _ => None,
            };
            if is_key(key)
                && let Some(source) = lookup.get_prefix_expr()
            {
                sources.push(source);
            }
        }
        Some(LuaExpr::CallExpr(call)) => {
            let is_rawget = matches!(
                call.get_prefix_expr(),
                Some(LuaExpr::NameExpr(callee)) if callee.get_name_text().as_deref() == Some("rawget")
            );
            let mut args = call
                .get_args_list()
                .into_iter()
                .flat_map(|args| args.get_args());
            if let (true, Some(source), key) = (is_rawget, args.next(), args.next())
                && is_key(key)
            {
                sources.push(source);
            }
        }
        Some(LuaExpr::BinaryExpr(binary))
            if binary.get_op_token().map(|op| op.get_op()) == Some(BinaryOperator::OpOr) =>
        {
            if let Some((left, right)) = binary.get_exprs() {
                collect_key_sources(db, signature_id, key_param_idx, left, sources);
                collect_key_sources(db, signature_id, key_param_idx, right, sources);
            }
        }
        _ => {}
    }
}

/// The `__index` function code gives values of `prefix_type`, its own or one
/// it inherits. One written on a class a doc declares does not count: that
/// class keeps the shape it is declared with.
pub(crate) fn code_index_function(db: &DbIndex, prefix_type: &LuaType) -> Option<LuaType> {
    find_members_with_key(db, prefix_type, LuaMemberKey::Name("__index".into()), true)?
        .into_iter()
        .filter(|member| matches!(member.typ, LuaType::Signature(_)))
        .find(|member| !written_on_declared_class(db, member))
        .map(|member| member.typ)
}

fn written_on_declared_class(db: &DbIndex, member: &LuaMemberInfo) -> bool {
    let Some(LuaSemanticDeclId::Member(member_id)) = member.property_owner_id else {
        return false;
    };
    matches!(
        db.get_member_index().get_member_owner(&member_id),
        Some(LuaMemberOwner::Type(type_id))
            if db.get_type_index().get_source_type_decl(type_id).is_some()
    )
}

fn signature_closure(db: &DbIndex, signature_id: &LuaSignatureId) -> Option<LuaClosureExpr> {
    let position = signature_id.get_position();
    db.get_vfs()
        .syntax_tree_around(
            &signature_id.get_file_id(),
            rowan::TextRange::empty(position),
        )?
        .get_red_root()
        .token_at_offset(position)
        .right_biased()?
        .parent_ancestors()
        .find_map(LuaClosureExpr::cast)
}

/// `expr`, or the value of the never reassigned local of the function it names.
fn unaliased(db: &DbIndex, signature_id: &LuaSignatureId, expr: LuaExpr) -> Option<LuaExpr> {
    let LuaExpr::NameExpr(name) = &expr else {
        return Some(expr);
    };
    let file_id = signature_id.get_file_id();
    let decl_id = db
        .get_reference_index()
        .get_var_reference_decl(&file_id, name.get_range())?;
    let is_mutated = db
        .get_reference_index()
        .get_decl_references(&file_id, &decl_id)
        .is_some_and(|references| references.mutable);
    if !db.get_decl_index().get_decl(&decl_id)?.is_local() || is_mutated {
        return None;
    }
    match local_initializer_expr(db, decl_id)? {
        (0, value) => Some(value),
        _ => None,
    }
}

fn is_param(db: &DbIndex, signature_id: &LuaSignatureId, name: &LuaNameExpr, idx: usize) -> bool {
    db.get_reference_index()
        .get_var_reference_decl(&signature_id.get_file_id(), name.get_range())
        .and_then(|decl_id| db.get_decl_index().get_decl(&decl_id))
        .is_some_and(|decl| {
            matches!(
                &decl.extra,
                LuaDeclExtra::Param { idx: param_idx, signature_id: owner, .. }
                    if *param_idx == idx && owner == signature_id
            )
        })
}
