use std::{ops::Deref, sync::Arc};

use glua_parser::{
    LuaAssignStat, LuaAst, LuaAstNode, LuaCallArgList, LuaCallExpr, LuaExpr, LuaIndexMemberExpr,
    LuaLiteralToken, LuaLocalStat, LuaReturnStat, LuaTableExpr, LuaTableField, LuaVarExpr,
};

use crate::{
    InFiled, InferGuard, LuaArrayType, LuaDeclId, LuaInferCache, LuaMemberId, LuaTupleStatus,
    LuaTupleType, LuaUnionType, TypeOps, VariadicType, check_type_compact,
    db_index::{DbIndex, LuaType},
    infer_call_expr_func, infer_expr,
};

use super::{
    InferFailReason, InferResult,
    infer_index::{infer_member_by_member_key, infer_member_by_operator},
};

pub fn infer_table_expr(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    table: LuaTableExpr,
) -> InferResult {
    // A sequential literal whose rows are themselves table literals carries
    // meaningful per-index shape. Materialize it as a dynamic table (TableConst)
    // so integer-keyed members (`[1]`, `[2]`, ...) hold the rich row shapes and
    // the table stays mutable. Simple scalar arrays fall through to the array
    // summary path below so they remain `T[]`.
    if table.is_shaped_array_literal() {
        return Ok(LuaType::TableConst(crate::InFiled {
            file_id: cache.get_file_id(),
            value: table.get_range(),
        }));
    }

    if table.is_array() {
        return infer_table_array_summary(db, cache, table);
    }

    Ok(LuaType::TableConst(crate::InFiled {
        file_id: cache.get_file_id(),
        value: table.get_range(),
    }))
}

/// Summarize a sequential ("array-style") table literal that is NOT a shaped
/// table-of-tables (see [`LuaTableExpr::is_shaped_array_literal`]).
///
/// Small scalar/mixed literals (`{1, 2, 3}`, `{ self, "player" }`) are inferred
/// as an infer-resolve [`LuaType::Tuple`]. Despite the name, this is NOT an
/// immutable tuple value: it is an internal *positional-evidence carrier* that
/// preserves exact per-index types so machinery like `table.unpack`,
/// `std.Unpack<T>`, multi-value assignment, and positional `[1]`/`[2]` field
/// checks stay precise. Mutation of such a table is treated leniently elsewhere
/// (see `assign_type_mismatch`), so it behaves as a dynamic table for
/// diagnostics. Large literals collapse to `T[]`; dots/variadic spreads are
/// handled as before.
fn infer_table_array_summary(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    table: LuaTableExpr,
) -> InferResult {
    let fields = table.get_fields().collect::<Vec<_>>();
    if fields.len() > 50 {
        let first_type = infer_expr(
            db,
            cache,
            fields[0].get_value_expr().ok_or(InferFailReason::None)?,
        )?;
        return Ok(LuaType::Array(
            LuaArrayType::from_base_type(first_type).into(),
        ));
    }

    if let Some(first_field) = fields.first() {
        let first_value_expr = first_field.get_value_expr().ok_or(InferFailReason::None)?;

        if is_dots_expr(&first_value_expr).unwrap_or(false) {
            let first_expr_type = infer_expr(db, cache, first_value_expr)?;
            match &first_expr_type {
                LuaType::Variadic(multi) => match &multi.deref() {
                    VariadicType::Base(base) => {
                        return Ok(LuaType::Array(
                            LuaArrayType::from_base_type(base.clone()).into(),
                        ));
                    }
                    VariadicType::Multi(tuple) => {
                        return Ok(LuaType::Tuple(
                            LuaTupleType::new(tuple.clone(), LuaTupleStatus::InferResolve).into(),
                        ));
                    }
                },
                _ => {
                    return Ok(LuaType::Array(
                        LuaArrayType::from_base_type(first_expr_type).into(),
                    ));
                }
            };
        }
    }

    if let Some(last_field) = fields.last() {
        let last_value_expr = last_field.get_value_expr().ok_or(InferFailReason::None)?;
        let last_expr_type = infer_expr(db, cache, last_value_expr)?;
        if let LuaType::Variadic(multi) = last_expr_type
            && let VariadicType::Base(base) = &multi.deref()
        {
            let non_nil_base = TypeOps::Remove.apply(db, base, &LuaType::Nil);
            if fields.len() <= 1 {
                return Ok(LuaType::Array(
                    LuaArrayType::from_base_type(non_nil_base).into(),
                ));
            }
            let len = fields.len() - 1;
            let mut all_can_accept_base = true;
            for i in 0..len {
                let field = fields.get(i).ok_or(InferFailReason::None)?;
                let value_expr = field.get_value_expr().ok_or(InferFailReason::None)?;
                let typ = infer_expr(db, cache, value_expr)?;
                if check_type_compact(db, &non_nil_base, &typ).is_err() {
                    all_can_accept_base = false;
                    break;
                }
            }

            if all_can_accept_base {
                return Ok(LuaType::Array(
                    LuaArrayType::from_base_type(non_nil_base).into(),
                ));
            }
        };
    }

    // Small scalar/mixed sequential literal: retain exact per-index types as an
    // infer-resolve tuple (a positional-evidence carrier; see the function doc).
    // Mutation leniency is handled in the diagnostic layer.
    let mut types = Vec::new();
    for field in fields {
        let value_expr = field.get_value_expr().ok_or(InferFailReason::None)?;
        let typ = infer_expr(db, cache, value_expr)?;
        match typ {
            LuaType::Variadic(multi) => flatten_multi_into_tuple(&mut types, &multi),
            _ => {
                types.push(typ);
            }
        }
    }

    Ok(LuaType::Tuple(
        LuaTupleType::new(types, LuaTupleStatus::InferResolve).into(),
    ))
}

fn flatten_multi_into_tuple(tuple_list: &mut Vec<LuaType>, multi: &VariadicType) {
    match multi {
        VariadicType::Base(base) => {
            tuple_list.push(LuaType::Variadic(VariadicType::Base(base.clone()).into()));
        }
        VariadicType::Multi(multi) => {
            for typ in multi {
                match typ {
                    LuaType::Variadic(multi) => {
                        flatten_multi_into_tuple(tuple_list, multi.deref());
                    }
                    _ => {
                        tuple_list.push(typ.clone());
                    }
                }
            }
        }
    }
}

fn is_dots_expr(expr: &LuaExpr) -> Option<bool> {
    if let LuaExpr::LiteralExpr(literal) = expr
        && let LuaLiteralToken::Dots(_) = literal.get_literal()?
    {
        return Some(true);
    }

    Some(false)
}

pub fn infer_table_should_be(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    table: LuaTableExpr,
) -> InferResult {
    let table_syntax_owner = InFiled::new(cache.get_file_id(), table.get_syntax_id());
    if let Some(bind_type_cache) = db
        .get_type_index()
        .get_type_cache(&table_syntax_owner.into())
    {
        return Ok(bind_type_cache.as_type().clone());
    }

    match table.get_parent::<LuaAst>().ok_or(InferFailReason::None)? {
        LuaAst::LuaCallArgList(call_arg_list) => {
            infer_table_type_by_callee(db, cache, call_arg_list, table)
        }
        LuaAst::LuaTableField(field) => infer_table_field_type_by_parent(db, cache, field),
        LuaAst::LuaLocalStat(local) => infer_table_type_by_local(db, cache, local, table),
        LuaAst::LuaAssignStat(assign_stat) => {
            infer_table_type_by_assign_stat(db, cache, assign_stat, table)
        }
        LuaAst::LuaReturnStat(return_stat) => {
            infer_table_type_by_return_stat(db, cache, return_stat, table)
        }
        _ => Err(InferFailReason::None),
    }
}

pub fn infer_table_field_value_should_be(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    table_field: LuaTableField,
) -> InferResult {
    let parnet_table_expr = table_field
        .get_parent::<LuaTableExpr>()
        .ok_or(InferFailReason::None)?;
    let parent_table_expr_type = infer_table_should_be(db, cache, parnet_table_expr)?;
    let index = LuaIndexMemberExpr::TableField(table_field.clone());
    let reason = match infer_member_by_member_key(
        db,
        cache,
        &parent_table_expr_type,
        index.clone(),
        &InferGuard::new(),
    ) {
        Ok(member_type) => return Ok(member_type),
        Err(InferFailReason::FieldNotFound) => InferFailReason::FieldNotFound,
        Err(err) => return Err(err),
    };

    match infer_member_by_operator(
        db,
        cache,
        &parent_table_expr_type,
        index,
        &InferGuard::new(),
    ) {
        Ok(member_type) => return Ok(member_type),
        Err(InferFailReason::FieldNotFound) => {}
        Err(err) => return Err(err),
    }

    let member_id = LuaMemberId::new(table_field.get_syntax_id(), cache.get_file_id());
    if let Some(type_cache) = db.get_type_index().get_type_cache(&member_id.into()) {
        return Ok(type_cache.as_type().clone());
    };

    Err(reason)
}

fn infer_table_type_by_callee(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_arg_list: LuaCallArgList,
    table_expr: LuaTableExpr,
) -> InferResult {
    let call_arg_number = call_arg_list
        .children::<LuaAst>()
        .enumerate()
        .find(|(_, arg)| arg.get_position() == table_expr.get_position())
        .ok_or(InferFailReason::None)?
        .0;
    let call_expr = call_arg_list
        .get_parent::<LuaCallExpr>()
        .ok_or(InferFailReason::None)?;
    let typ = infer_call_arg_should_be(db, cache, call_expr, call_arg_number)?;
    match &typ {
        LuaType::TableConst(_) => {}
        LuaType::Union(union) => {
            // TODO: 假设存在多个匹配项, 我们需要根据字段的匹配情况来确定最终的类型
            return Ok(union_remove_non_table_type(db, union));
        }
        _ => {}
    }

    Ok(typ)
}

fn infer_call_arg_should_be(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: LuaCallExpr,
    mut call_arg_number: usize,
) -> InferResult {
    let prefix_expr = call_expr.get_prefix_expr().ok_or(InferFailReason::None)?;
    let prefix_type = infer_expr(db, cache, prefix_expr)?;
    let func_type = infer_call_expr_func(
        db,
        cache,
        call_expr.clone(),
        prefix_type,
        &InferGuard::new(),
        None,
    )?;
    match (func_type.is_colon_define(), call_expr.is_colon_call()) {
        (true, true) | (false, false) => {}
        (false, true) => {
            call_arg_number += 1;
        }
        (true, false) => {
            call_arg_number = call_arg_number.saturating_sub(1);
        }
    }
    let typ = func_type
        .get_params()
        .get(call_arg_number)
        .ok_or(InferFailReason::None)?
        .1
        .clone()
        .unwrap_or(LuaType::Any);
    Ok(typ)
}

/// 移除掉一些非`table`类型
fn union_remove_non_table_type(db: &DbIndex, union: &Arc<LuaUnionType>) -> LuaType {
    let mut result = LuaType::Unknown;
    for typ in union.into_set().into_iter() {
        match typ {
            LuaType::Signature(_) | LuaType::DocFunction(_) => {}
            _ if typ.is_string() || typ.is_number() || typ.is_boolean() => {}
            _ => {
                result = TypeOps::Union.apply(db, &result, &typ);
            }
        }
    }
    result
}

fn infer_table_field_type_by_parent(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    field: LuaTableField,
) -> InferResult {
    let member_id = LuaMemberId::new(field.get_syntax_id(), cache.get_file_id());
    if let Some(type_cache) = db.get_type_index().get_type_cache(&member_id.into()) {
        if type_cache.is_doc() {
            let typ = type_cache.as_type();
            match typ {
                LuaType::TableConst(_) => {}
                LuaType::Tuple(tuple) => {
                    let types = tuple.get_types();
                    // 这种情况下缓存的类型可能是不精确的
                    if tuple.is_infer_resolve() && types.len() == 1 && types[0].is_unknown() {
                    } else {
                        return Ok(typ.clone());
                    }
                }
                typ => return Ok(typ.clone()),
            }
        }
    } else if field.is_value_field() {
        return infer_table_field_value_should_be(db, cache, field);
    } else {
        return Err(InferFailReason::UnResolveMemberType(member_id));
    }

    let parnet_table_expr = field
        .get_parent::<LuaTableExpr>()
        .ok_or(InferFailReason::None)?;
    let parent_table_expr_type = infer_table_should_be(db, cache, parnet_table_expr)?;

    let index = LuaIndexMemberExpr::TableField(field);
    let reason = match infer_member_by_member_key(
        db,
        cache,
        &parent_table_expr_type,
        index.clone(),
        &InferGuard::new(),
    ) {
        Ok(member_type) => return Ok(member_type),
        Err(InferFailReason::FieldNotFound) => InferFailReason::FieldNotFound,
        Err(err) => return Err(err),
    };

    match infer_member_by_operator(
        db,
        cache,
        &parent_table_expr_type,
        index,
        &InferGuard::new(),
    ) {
        Ok(member_type) => return Ok(member_type),
        Err(InferFailReason::FieldNotFound) => {}
        Err(err) => return Err(err),
    }

    Err(reason)
}

fn infer_table_type_by_local(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    local: LuaLocalStat,
    table_expr: LuaTableExpr,
) -> InferResult {
    let local_names = local.get_local_name_list().collect::<Vec<_>>();
    let values = local.get_value_exprs().collect::<Vec<_>>();
    let num = values
        .iter()
        .enumerate()
        .find(|(_, value)| value.get_position() == table_expr.get_position())
        .ok_or(InferFailReason::None)?
        .0;

    let local_name = local_names.get(num).ok_or(InferFailReason::None)?;
    let decl_id = LuaDeclId::new(cache.get_file_id(), local_name.get_position());
    match db.get_type_index().get_type_cache(&decl_id.into()) {
        Some(type_cache) => match type_cache.as_type() {
            LuaType::TableConst(_) => {
                infer_table_type_from_local_call_references(db, cache, decl_id)
            }
            typ => Ok(typ.clone()),
        },
        None => infer_table_type_from_local_call_references(db, cache, decl_id)
            .map_err(|_| InferFailReason::UnResolveDeclType(decl_id)),
    }
}

fn infer_table_type_from_local_call_references(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    decl_id: LuaDeclId,
) -> InferResult {
    let file_id = cache.get_file_id();
    let references = db
        .get_reference_index()
        .get_decl_references(&file_id, &decl_id)
        .ok_or(InferFailReason::None)?;
    if references
        .cells
        .iter()
        .any(|cell| cell.is_write && cell.range.start() != decl_id.position)
    {
        return Err(InferFailReason::None);
    }

    let root = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .ok_or(InferFailReason::None)?
        .get_chunk_node();

    let mut typ = LuaType::Unknown;
    for cell in &references.cells {
        if cell.is_write {
            continue;
        }

        let Some(reference_expr) = root
            .descendants::<LuaExpr>()
            .find(|expr| expr.get_range() == cell.range)
        else {
            continue;
        };

        let Some(call_arg_list) = reference_expr.get_parent::<LuaCallArgList>() else {
            continue;
        };
        let Some(call_expr) = call_arg_list.get_parent::<LuaCallExpr>() else {
            continue;
        };
        let Some(call_arg_number) = call_arg_list
            .get_args()
            .position(|arg| arg.get_position() == reference_expr.get_position())
        else {
            continue;
        };
        let Ok(call_arg_type) = infer_call_arg_should_be(db, cache, call_expr, call_arg_number)
        else {
            continue;
        };
        if call_arg_type.is_any() || call_arg_type.is_unknown() {
            continue;
        }

        typ = TypeOps::Union.apply(db, &typ, &call_arg_type);
    }

    if typ.is_unknown() {
        Err(InferFailReason::None)
    } else {
        Ok(typ)
    }
}

fn infer_table_type_by_assign_stat(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    assign_stat: LuaAssignStat,
    table_expr: LuaTableExpr,
) -> InferResult {
    let (vars, exprs) = assign_stat.get_var_and_expr_list();
    let num = exprs
        .iter()
        .enumerate()
        .find(|(_, expr)| expr.get_position() == table_expr.get_position())
        .ok_or(InferFailReason::None)?
        .0;
    let name = vars.get(num).ok_or(InferFailReason::None)?;

    let decl_id = LuaDeclId::new(cache.get_file_id(), name.get_position());
    if db.get_decl_index().get_decl(&decl_id).is_some() {
        match db.get_type_index().get_type_cache(&decl_id.into()) {
            Some(type_cache) => match type_cache.as_type() {
                LuaType::TableConst(_) => Err(InferFailReason::None),
                typ => Ok(typ.clone()),
            },
            None => Err(InferFailReason::UnResolveDeclType(decl_id)),
        }
    } else {
        if let LuaVarExpr::IndexExpr(index_expr) = name {
            let member_id = LuaMemberId::new(index_expr.get_syntax_id(), cache.get_file_id());
            if let Some(type_cache) = db.get_type_index().get_type_cache(&member_id.into()) {
                return match type_cache.as_type() {
                    LuaType::TableConst(_) => Err(InferFailReason::None),
                    typ => Ok(typ.clone()),
                };
            }
        }

        infer_expr(
            db,
            cache,
            LuaExpr::cast(name.syntax().clone()).ok_or(InferFailReason::None)?,
        )
    }
}

fn infer_table_type_by_return_stat(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    return_stat: LuaReturnStat,
    table_expr: LuaTableExpr,
) -> InferResult {
    let in_file_syntax_id = InFiled::new(cache.get_file_id(), return_stat.get_syntax_id());
    let cache_type = match db
        .get_type_index()
        .get_type_cache(&in_file_syntax_id.into())
    {
        Some(cache) => cache,
        None => {
            let in_file_range = InFiled::new(cache.get_file_id(), table_expr.get_range());
            return Ok(LuaType::TableConst(in_file_range));
        }
    };
    Ok(cache_type.as_type().clone())
}
