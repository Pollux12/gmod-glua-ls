use glua_parser::{LuaAstNode, LuaCallExpr, LuaClosureExpr};

use crate::{AsyncState, DiagnosticCode, LuaSignatureId, LuaType, SemanticModel, get_real_type};

use super::{Checker, DiagnosticContext};

pub struct AwaitInSyncChecker;

impl Checker for AwaitInSyncChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::AwaitInSync];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let Some(buckets) = context.file_buckets() else {
            return;
        };
        for call_expr in buckets.call_exprs.iter().cloned() {
            check_call_in_async(context, semantic_model, call_expr.clone());
            check_call_as_arg(context, semantic_model, call_expr);
        }
    }
}

fn check_call_in_async(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    call_expr: LuaCallExpr,
) -> Option<()> {
    let direct_state = get_direct_call_async_state(semantic_model, &call_expr)?;

    let async_state = match direct_state {
        DirectCallAsyncState::Known(async_state) => async_state,
        DirectCallAsyncState::NeedFullResolve => semantic_model
            .infer_call_expr_func(call_expr.clone(), None)?
            .get_async_state(),
    };

    if async_state == AsyncState::Async
        && let Some(prefix_expr) = call_expr.get_prefix_expr()
    {
        let is_sync_call = check_async_func_in_sync_call(semantic_model, call_expr).is_err();
        if is_sync_call {
            context.add_diagnostic(
                DiagnosticCode::AwaitInSync,
                prefix_expr.get_range(),
                "Async function can only be called in async function.".to_string(),
                None,
            );
        }
    }

    Some(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectCallAsyncState {
    Known(AsyncState),
    NeedFullResolve,
}

fn get_direct_call_async_state(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
) -> Option<DirectCallAsyncState> {
    let prefix_expr = call_expr.get_prefix_expr()?;
    let prefix_type = semantic_model.infer_expr(prefix_expr).ok()?;
    Some(get_type_async_state(semantic_model, &prefix_type))
}

fn get_type_async_state(semantic_model: &SemanticModel, typ: &LuaType) -> DirectCallAsyncState {
    let db = semantic_model.get_db();
    let real_type = get_real_type(db, typ).unwrap_or(typ);
    match real_type {
        LuaType::DocFunction(func) => DirectCallAsyncState::Known(func.get_async_state()),
        LuaType::Signature(signature_id) => {
            let async_state = db
                .get_signature_index()
                .get(signature_id)
                .map(|signature| signature.async_state)
                .unwrap_or(AsyncState::None);
            DirectCallAsyncState::Known(async_state)
        }
        LuaType::Union(union) => {
            let mut saw_unknown = false;
            for member_type in union.types() {
                match get_type_async_state(semantic_model, member_type) {
                    DirectCallAsyncState::Known(AsyncState::Async) => {
                        return DirectCallAsyncState::Known(AsyncState::Async);
                    }
                    DirectCallAsyncState::Known(_) => {}
                    DirectCallAsyncState::NeedFullResolve => saw_unknown = true,
                }
            }

            if saw_unknown {
                DirectCallAsyncState::NeedFullResolve
            } else {
                DirectCallAsyncState::Known(AsyncState::None)
            }
        }
        LuaType::Any | LuaType::Unknown | LuaType::SelfInfer | LuaType::Global | LuaType::Never => {
            DirectCallAsyncState::Known(AsyncState::None)
        }
        LuaType::TableConst(_)
        | LuaType::Ref(_)
        | LuaType::Def(_)
        | LuaType::Generic(_)
        | LuaType::Instance(_)
        | LuaType::Object(_)
        | LuaType::TableOf(_)
        | LuaType::Intersection(_) => DirectCallAsyncState::NeedFullResolve,
        _ => DirectCallAsyncState::Known(AsyncState::None),
    }
}

fn check_call_as_arg(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    call_expr: LuaCallExpr,
) -> Option<()> {
    let func = semantic_model.infer_call_expr_func(call_expr.clone(), None)?;
    let colon_define = func.is_colon_define();
    let colon_call = call_expr.is_colon_call();
    for (i, arg_type) in func.get_params().iter().enumerate() {
        if let Some(LuaType::DocFunction(f)) = &arg_type.1 {
            let async_state = f.get_async_state();
            if async_state == AsyncState::Sync {
                let arg_list = call_expr.get_args_list()?;
                let arg_idx = match (colon_define, colon_call) {
                    (true, false) => i + 1,
                    (false, true) => {
                        if i == 0 {
                            return None; // colon call should not have a self argument
                        }
                        i - 1
                    }
                    _ => i,
                };
                let arg = arg_list.get_args().nth(arg_idx)?;
                let arg_type = semantic_model
                    .infer_expr(arg.clone())
                    .unwrap_or(LuaType::Any);
                let async_state = match &arg_type {
                    LuaType::DocFunction(f) => f.get_async_state(),
                    LuaType::Signature(sig) => {
                        let signature = semantic_model.get_db().get_signature_index().get(sig)?;
                        signature.async_state
                    }
                    _ => continue,
                };

                if async_state == AsyncState::Async {
                    let is_sync_call =
                        check_async_func_in_sync_call(semantic_model, call_expr.clone()).is_err();
                    if is_sync_call {
                        context.add_diagnostic(
                            DiagnosticCode::AwaitInSync,
                            arg.get_range(),
                            "Async function can only be called in async function.".to_string(),
                            None,
                        );
                    }
                }
            }
        }
    }

    Some(())
}

fn check_async_func_in_sync_call(
    semantic_model: &SemanticModel,
    call_expr: LuaCallExpr,
) -> Result<(), ()> {
    let file_id = semantic_model.get_file_id();
    let closures = call_expr.ancestors::<LuaClosureExpr>();
    for closure in closures {
        let signature_id = LuaSignatureId::from_closure(file_id, &closure);
        let Some(signature) = semantic_model
            .get_db()
            .get_signature_index()
            .get(&signature_id)
        else {
            return Ok(());
        };

        match signature.async_state {
            AsyncState::Sync => continue,
            AsyncState::None => {
                return Err(());
            }
            _ => return Ok(()),
        }
    }

    Err(())
}
