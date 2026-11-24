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
/// does not hold reaches `__index(self, key)`. Each `P[key]` or
/// `rawget(P, key)` it returns (see [`KeyRead`]) gives `P`'s field `key`,
/// widened as the stored value can change, and every other value it returns
/// counts as itself. Without a lookup that finds the field the read is what
/// the function returns.
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
    let mut read = KeyRead {
        db,
        signature_id,
        key_param_idx: if signature.is_colon_define { 0 } else { 1 },
        key_name: LuaMemberKey::Name(key_name.into()),
        cache: LuaInferCache::new(signature_id.get_file_id(), cache.get_config().clone()),
        found_field: false,
    };
    let mut returned_types = Vec::new();
    for return_stat in closure.descendants::<LuaReturnStat>() {
        if return_stat.ancestors::<LuaClosureExpr>().next().as_ref() != Some(&closure) {
            continue;
        }
        let returned_type = match return_stat.get_expr_list().next() {
            Some(returned) => read.value_type(returned),
            None => Some(LuaType::Nil),
        };
        returned_types.extend(returned_type);
    }

    if read.found_field
        && let Some(returned_type) = returned_types
            .into_iter()
            .reduce(|left, right| TypeOps::Union.apply(db, &left, &right))
    {
        return Some(returned_type);
    }
    match signature.get_return_type() {
        LuaType::Nil => Some(LuaType::Unknown),
        returned => Some(returned),
    }
}

/// Reads of the key parameter of an `__index` function from the tables it
/// looks in: `P[key]` or `rawget(P, key)`, also through a local holding it.
struct KeyRead<'a> {
    db: &'a DbIndex,
    signature_id: &'a LuaSignatureId,
    key_param_idx: usize,
    key_name: LuaMemberKey,
    cache: LuaInferCache,
    /// Whether a lookup found the field in the table it looks in.
    found_field: bool,
}

impl KeyRead<'_> {
    /// What `expr` gives when the function returns it: `None` when it is a
    /// lookup of a table without the field, or cannot be inferred.
    fn value_type(&mut self, expr: LuaExpr) -> Option<LuaType> {
        let expr = unaliased(self.db, self.signature_id, expr.clone()).unwrap_or(expr);
        if let Some(source) = self.lookup_source(&expr) {
            return self.field_type(source);
        }
        if let LuaExpr::BinaryExpr(binary) = &expr
            && binary.get_op_token().map(|op| op.get_op()) == Some(BinaryOperator::OpOr)
        {
            let (left, right) = binary.get_exprs()?;
            let left = self.value_type(left);
            let right = self.value_type(right);
            return match (left, right) {
                (Some(left), _) if !left.is_nullable() => Some(left),
                (Some(left), Some(right)) => Some(TypeOps::Union.apply(
                    self.db,
                    &TypeOps::Remove.apply(self.db, &left, &LuaType::Nil),
                    &right,
                )),
                (left, right) => left.or(right),
            };
        }
        infer_expr(self.db, &mut self.cache, expr)
            .ok()
            .map(|typ| widen_literal_type_for_assignment(&typ))
    }

    /// `P` of `P[key]` or `rawget(P, key)`.
    fn lookup_source(&self, expr: &LuaExpr) -> Option<LuaExpr> {
        match expr {
            LuaExpr::IndexExpr(lookup) => match lookup.get_index_key() {
                Some(LuaIndexKey::Expr(key)) if self.is_key(&key) => lookup.get_prefix_expr(),
                _ => None,
            },
            LuaExpr::CallExpr(call) => {
                let is_rawget = matches!(
                    call.get_prefix_expr(),
                    Some(LuaExpr::NameExpr(callee)) if callee.get_name_text().as_deref() == Some("rawget")
                );
                let mut args = call.get_args_list()?.get_args();
                let (source, key) = (args.next()?, args.next()?);
                (is_rawget && self.is_key(&key)).then_some(source)
            }
            _ => None,
        }
    }

    fn is_key(&self, expr: &LuaExpr) -> bool {
        matches!(expr, LuaExpr::NameExpr(name) if is_param(self.db, self.signature_id, name, self.key_param_idx))
    }

    fn field_type(&mut self, source: LuaExpr) -> Option<LuaType> {
        let source_type = infer_expr(self.db, &mut self.cache, source).ok()?;
        let field_type = find_members_with_key(self.db, &source_type, self.key_name.clone(), true)?
            .into_iter()
            .map(|member| widen_literal_type_for_assignment(&member.typ))
            .reduce(|left, right| TypeOps::Union.apply(self.db, &left, &right))?;
        self.found_field = true;
        Some(field_type)
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
