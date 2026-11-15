//! A table a function constructs on each call holds what that call passed:
//! the function's code reads a parameter as the argument the call gave it.

use glua_parser::{LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaExpr};
use rowan::TextRange;

use crate::{
    DbIndex, InFiled, LuaDeclExtra, LuaInferCache, LuaSignatureId, LuaType,
    semantic::infer::{infer_call::get_prefix_expr_signature_id, infer_expr},
};

/// The value `expr`, written into the table constructor `table`, holds in the
/// copy of it one of `calls`, innermost last, made. `expr` names a parameter
/// of the function constructing `table`, which reads as what the call of that
/// function passes; an argument naming a parameter of the function the call
/// is in reads in turn as what an earlier call passes. `None` when no call
/// binds `expr`.
pub(super) fn infer_factory_call_value(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    table: &InFiled<TextRange>,
    expr: InFiled<LuaExpr>,
    calls: &[InFiled<TextRange>],
) -> Option<LuaType> {
    let (at, signature_id, arg) = bound_arg(db, cache, &expr, calls)?;
    let constructs_table = table.file_id == signature_id.get_file_id()
        && signature_closure(db, signature_id)
            .is_some_and(|closure| closure.get_range().contains_range(table.value));
    if !constructs_table {
        return None;
    }
    Some(infer_forwarded_arg(
        db,
        cache,
        InFiled::new(calls[at].file_id, arg),
        &calls[..at],
    )?)
}

/// What the argument `arg` holds, reading a parameter of the function it is
/// written in as what the innermost of `calls` calling that function passes.
fn infer_forwarded_arg(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    arg: InFiled<LuaExpr>,
    calls: &[InFiled<TextRange>],
) -> Option<LuaType> {
    match bound_arg(db, cache, &arg, calls) {
        Some((at, _, outer)) => infer_forwarded_arg(
            db,
            cache,
            InFiled::new(calls[at].file_id, outer),
            &calls[..at],
        ),
        None => with_file_cache(cache, arg.file_id, |cache| {
            infer_expr(db, cache, arg.value).ok()
        }),
    }
}

/// The index into `calls` of the innermost call of the function whose
/// parameter `expr` names, that function, and the argument the call passes
/// for the parameter.
fn bound_arg(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: &InFiled<LuaExpr>,
    calls: &[InFiled<TextRange>],
) -> Option<(usize, LuaSignatureId, LuaExpr)> {
    let LuaExpr::NameExpr(name) = &expr.value else {
        return None;
    };
    let decl_id = db
        .get_reference_index()
        .get_var_reference_decl(&expr.file_id, name.get_range())?;
    let (signature_id, param) = match db.get_decl_index().get_decl(&decl_id)?.extra {
        LuaDeclExtra::Param {
            idx, signature_id, ..
        } => {
            let signature = db.get_signature_index().get(&signature_id)?;
            (signature_id, idx + usize::from(signature.is_colon_define))
        }
        LuaDeclExtra::ImplicitSelf { .. } => {
            let closure = name.syntax().ancestors().find_map(LuaClosureExpr::cast)?;
            (LuaSignatureId::from_closure(expr.file_id, &closure), 0)
        }
        _ => return None,
    };
    calls.iter().enumerate().rev().find_map(|(at, call)| {
        let call_expr = call_at(db, call)?;
        let callee = with_file_cache(cache, call.file_id, |cache| {
            get_prefix_expr_signature_id(db, cache, &call_expr)
        })?;
        if callee != signature_id {
            return None;
        }
        Some((at, signature_id, arg_for_param(&call_expr, param)?))
    })
}

/// What `call` passes for parameter `param`, counting a method's `self` as
/// parameter 0.
fn arg_for_param(call: &LuaCallExpr, param: usize) -> Option<LuaExpr> {
    let mut args = call.get_args_list()?.get_args();
    if !call.is_colon_call() {
        return args.nth(param);
    }
    match param.checked_sub(1) {
        Some(arg) => args.nth(arg),
        None => match call.get_prefix_expr()? {
            LuaExpr::IndexExpr(index_expr) => index_expr.get_prefix_expr(),
            _ => None,
        },
    }
}

fn call_at(db: &DbIndex, call: &InFiled<TextRange>) -> Option<LuaCallExpr> {
    db.get_vfs()
        .get_syntax_tree(&call.file_id)?
        .get_red_root()
        .token_at_offset(call.value.start())
        .right_biased()?
        .parent_ancestors()
        .find_map(LuaCallExpr::cast)
        .filter(|call_expr| call_expr.get_range() == call.value)
}

fn signature_closure(db: &DbIndex, signature_id: LuaSignatureId) -> Option<LuaClosureExpr> {
    db.get_vfs()
        .get_syntax_tree(&signature_id.get_file_id())?
        .get_red_root()
        .token_at_offset(signature_id.get_position())
        .right_biased()?
        .parent_ancestors()
        .filter_map(LuaClosureExpr::cast)
        .find(|closure| closure.get_position() == signature_id.get_position())
}

fn with_file_cache<T>(
    cache: &mut LuaInferCache,
    file_id: crate::FileId,
    f: impl FnOnce(&mut LuaInferCache) -> Option<T>,
) -> Option<T> {
    if file_id == cache.get_file_id() {
        return f(cache);
    }
    let mut file_cache = LuaInferCache::new(file_id, cache.get_config().clone());
    f(&mut file_cache)
}
