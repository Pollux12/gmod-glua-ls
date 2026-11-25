//! A table a function constructs on each call holds what that call passed:
//! the function's code reads a parameter as the argument the call gave it.

use std::cell::RefCell;

use glua_parser::{LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaFuncStat, LuaVarExpr};
use rowan::TextRange;
use rustc_hash::FxHashSet;

use crate::{
    DbIndex, FileId, InFiled, LuaDeclExtra, LuaInferCache, LuaSignatureId, LuaType,
    semantic::infer::{infer_call::get_prefix_expr_signature_id, infer_expr},
};

thread_local! {
    /// Arguments currently being forwarded across files, to stop mutual factory
    /// cycles before `with_file_cache` opens fresh caches without end.
    static FORWARDING: RefCell<FxHashSet<(FileId, TextRange)>> = RefCell::default();
}

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
    infer_forwarded_arg(db, cache, calls[at].file_id, arg, &calls[..at])
}

/// What the argument `arg` of a call in `file_id` holds, `nil` when the call
/// passes none, reading a parameter of the function it is written in as what
/// the innermost of `calls` calling that function passes.
fn infer_forwarded_arg(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: crate::FileId,
    arg: Option<LuaExpr>,
    calls: &[InFiled<TextRange>],
) -> Option<LuaType> {
    let Some(arg) = arg else {
        return Some(LuaType::Nil);
    };
    let key = (file_id, arg.get_range());
    if !FORWARDING.with(|active| active.borrow_mut().insert(key)) {
        return None;
    }
    let arg = InFiled::new(file_id, arg);
    let result = match bound_arg(db, cache, &arg, calls) {
        Some((at, _, outer)) => {
            infer_forwarded_arg(db, cache, calls[at].file_id, outer, &calls[..at])
        }
        None => with_file_cache(cache, arg.file_id, |cache| {
            infer_expr(db, cache, arg.value).ok()
        })
        .map(|typ| match typ {
            LuaType::Variadic(multi) => multi.get_type(0).cloned().unwrap_or(LuaType::Nil),
            other => other,
        }),
    };
    FORWARDING.with(|active| active.borrow_mut().remove(&key));
    result
}

/// The index into `calls` of the innermost call of the function whose
/// parameter `expr` names, that function, and the argument the call passes
/// for the parameter: `None` when it passes nothing there. A parameter the
/// function assigns to holds what it assigned, not what a call passed.
fn bound_arg(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: &InFiled<LuaExpr>,
    calls: &[InFiled<TextRange>],
) -> Option<(usize, LuaSignatureId, Option<LuaExpr>)> {
    let LuaExpr::NameExpr(name) = &expr.value else {
        return None;
    };
    let references = db.get_reference_index();
    let decl_id = references.get_var_reference_decl(&expr.file_id, name.get_range())?;
    if references
        .get_decl_references(&expr.file_id, &decl_id)
        .is_some_and(|refs| refs.is_written_after_declaration(decl_id.position))
    {
        return None;
    }
    let (signature_id, param) = match db.get_decl_index().get_decl(&decl_id)?.extra {
        LuaDeclExtra::Param {
            idx, signature_id, ..
        } => {
            let signature = db.get_signature_index().get(&signature_id)?;
            (signature_id, idx + usize::from(signature.is_colon_define))
        }
        LuaDeclExtra::ImplicitSelf { .. } => {
            let closure = name
                .syntax()
                .ancestors()
                .filter_map(LuaFuncStat::cast)
                .find(|func_stat| {
                    matches!(
                        func_stat.get_func_name(),
                        Some(LuaVarExpr::IndexExpr(index_expr))
                            if index_expr
                                .get_index_token()
                                .is_some_and(|token| {
                                    token.is_colon()
                                        && glua_parser::LuaAstToken::get_range(&token).start()
                                            == decl_id.position
                                })
                    )
                })
                .and_then(|func_stat| func_stat.get_closure())?;
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
        Some((at, signature_id, passed_arg(&call_expr, param)?))
    })
}

/// What `call` passes for parameter `param`: `Some(None)` when it passes
/// nothing there, which the parameter reads as `nil`, and `None` when a last
/// argument that expands may reach it.
pub(crate) fn passed_arg(call: &LuaCallExpr, param: usize) -> Option<Option<LuaExpr>> {
    if let Some(arg) = arg_for_param(call, param) {
        return Some(Some(arg));
    }
    let last = call.get_args_list()?.get_args().last();
    (!last.is_some_and(|last| crate::compilation::analyzer::expr_expands_to_fill_slots(&last)))
        .then_some(None)
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
        .syntax_tree_around(&call.file_id, call.value)?
        .get_red_root()
        .token_at_offset(call.value.start())
        .right_biased()?
        .parent_ancestors()
        .find_map(LuaCallExpr::cast)
        .filter(|call_expr| call_expr.get_range() == call.value)
}

fn signature_closure(db: &DbIndex, signature_id: LuaSignatureId) -> Option<LuaClosureExpr> {
    let position = signature_id.get_position();
    db.get_vfs()
        .syntax_tree_around(&signature_id.get_file_id(), TextRange::empty(position))?
        .get_red_root()
        .token_at_offset(position)
        .right_biased()?
        .parent_ancestors()
        .filter_map(LuaClosureExpr::cast)
        .find(|closure| closure.get_position() == position)
}

pub(crate) fn with_file_cache<T>(
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
