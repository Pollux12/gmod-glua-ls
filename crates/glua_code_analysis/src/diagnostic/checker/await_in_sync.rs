use std::{collections::HashSet, sync::Arc};

use glua_parser::{LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaIndexKey};

use crate::{
    AsyncState, DbIndex, DiagnosticCode, LuaFunctionType, LuaSemanticDeclId, LuaSignature,
    LuaSignatureId, LuaType, LuaTypeOwner, SemanticModel, TypeVisitTrait, get_real_type,
};

use super::{Checker, DiagnosticContext};

pub struct AwaitInSyncChecker;

impl Checker for AwaitInSyncChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::AwaitInSync];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let candidates = context
            .get_shared_data_arc()
            .map(|shared_data| shared_data.await_candidates.clone())
            .unwrap_or_else(|| Arc::new(precompute_await_candidates(semantic_model.get_db())));

        if !candidates.has_any_candidates() {
            return;
        }

        let root = semantic_model.get_root().clone();
        for call_expr in root.descendants::<LuaCallExpr>() {
            check_call_in_async(context, semantic_model, call_expr.clone(), &candidates);
            check_call_as_arg(context, semantic_model, call_expr, &candidates);
        }
    }
}

#[derive(Debug, Default)]
pub struct PrecomputedAwaitCandidates {
    async_callee_names: HashSet<String>,
    sync_callback_callee_names: HashSet<String>,
    has_async_callables: bool,
    has_sync_callback_params: bool,
}

impl PrecomputedAwaitCandidates {
    fn has_any_candidates(&self) -> bool {
        self.has_async_callables || self.has_sync_callback_params
    }

    fn should_check_direct_call(&self, call_expr: &LuaCallExpr) -> bool {
        if !self.has_async_callables {
            return false;
        }

        match static_call_name(call_expr) {
            StaticCallName::Static(name) => self.async_callee_names.contains(name.as_str()),
            StaticCallName::Unknown => true,
        }
    }

    fn should_check_callback_arg(&self, call_expr: &LuaCallExpr) -> bool {
        if !self.has_sync_callback_params {
            return false;
        }

        match static_call_name(call_expr) {
            StaticCallName::Static(name) => self.sync_callback_callee_names.contains(name.as_str()),
            StaticCallName::Unknown => true,
        }
    }

    fn insert_owner(&mut self, db: &DbIndex, owner: &LuaTypeOwner, info: AwaitCallableInfo) {
        if let Some(name) = owner_name(db, owner) {
            if info.async_callable {
                self.async_callee_names.insert(name.clone());
            }
            if info.sync_callback_params {
                self.sync_callback_callee_names.insert(name);
            }
        }
    }

    fn insert_signature_param_names(&mut self, db: &DbIndex, signature: &LuaSignature) {
        for param in signature.param_docs.values() {
            let info = type_await_info(db, &param.type_ref);
            if info.async_callable {
                self.async_callee_names.insert(param.name.clone());
            }
            if info.sync_callback_params {
                self.sync_callback_callee_names.insert(param.name.clone());
            }
        }
    }
}

enum StaticCallName {
    Static(String),
    Unknown,
}

fn static_call_name(call_expr: &LuaCallExpr) -> StaticCallName {
    match call_expr.get_prefix_expr() {
        Some(LuaExpr::NameExpr(name_expr)) => name_expr
            .get_name_token()
            .map(|token| StaticCallName::Static(token.get_name_text().to_string()))
            .unwrap_or(StaticCallName::Unknown),
        Some(LuaExpr::IndexExpr(index_expr)) => match index_expr.get_index_key() {
            Some(LuaIndexKey::Name(name)) => {
                StaticCallName::Static(name.get_name_text().to_string())
            }
            Some(LuaIndexKey::String(name)) => StaticCallName::Static(name.get_value()),
            _ => StaticCallName::Unknown,
        },
        _ => StaticCallName::Unknown,
    }
}

fn owner_name(db: &DbIndex, owner: &LuaTypeOwner) -> Option<String> {
    match owner {
        LuaTypeOwner::Decl(decl_id) => db
            .get_decl_index()
            .get_decl(decl_id)
            .map(|decl| decl.get_name().to_string()),
        LuaTypeOwner::Member(member_id) => db
            .get_member_index()
            .get_member(member_id)
            .and_then(|member| member.get_key().get_name())
            .map(str::to_string),
        LuaTypeOwner::SyntaxId(_) => None,
    }
}

pub fn precompute_await_candidates(db: &DbIndex) -> PrecomputedAwaitCandidates {
    let mut candidates = PrecomputedAwaitCandidates::default();
    let mut visited_signatures = HashSet::new();
    for (signature_id, signature) in db.get_signature_index().iter() {
        let info = signature_await_info(db, *signature_id, signature, &mut visited_signatures);
        candidates.has_async_callables |= info.async_callable;
        candidates.has_sync_callback_params |= info.sync_callback_params;
        candidates.insert_signature_param_names(db, signature);
    }

    for (owner, type_cache) in db.get_type_index().iter_type_caches() {
        let info = type_await_info(db, type_cache.as_type());
        candidates.has_async_callables |= info.async_callable;
        candidates.has_sync_callback_params |= info.sync_callback_params;
        if info.is_relevant() {
            candidates.insert_owner(db, owner, info);
        }
    }

    for (owner_id, _) in db.get_property_index().iter_owner_properties() {
        if let Some((owner, info)) = property_owner_await_info(db, owner_id) {
            candidates.insert_owner(db, &owner, info);
        }
    }

    candidates
}

fn property_owner_await_info(
    db: &DbIndex,
    owner_id: &LuaSemanticDeclId,
) -> Option<(LuaTypeOwner, AwaitCallableInfo)> {
    match owner_id {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            let owner = LuaTypeOwner::Decl(*decl_id);
            let info = db
                .get_type_index()
                .get_type_cache(&owner)
                .map(|type_cache| type_await_info(db, type_cache.as_type()))
                .unwrap_or_default();
            Some((owner, info))
        }
        LuaSemanticDeclId::Member(member_id) => {
            let owner = LuaTypeOwner::Member(*member_id);
            let info = db
                .get_type_index()
                .get_type_cache(&owner)
                .map(|type_cache| type_await_info(db, type_cache.as_type()))
                .unwrap_or_default();
            Some((owner, info))
        }
        LuaSemanticDeclId::Signature(_) => None,
        LuaSemanticDeclId::TypeDecl(_) => None,
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct AwaitCallableInfo {
    async_callable: bool,
    sync_callback_params: bool,
}

impl AwaitCallableInfo {
    fn is_relevant(self) -> bool {
        self.async_callable || self.sync_callback_params
    }

    fn include(&mut self, other: AwaitCallableInfo) {
        self.async_callable |= other.async_callable;
        self.sync_callback_params |= other.sync_callback_params;
    }
}

fn signature_await_info(
    db: &DbIndex,
    signature_id: LuaSignatureId,
    signature: &LuaSignature,
    visited_signatures: &mut HashSet<LuaSignatureId>,
) -> AwaitCallableInfo {
    if !visited_signatures.insert(signature_id) {
        return AwaitCallableInfo::default();
    }

    let mut info = AwaitCallableInfo {
        async_callable: signature.async_state == AsyncState::Async,
        sync_callback_params: false,
    };

    for overload in &signature.overloads {
        info.include(function_await_info(db, overload, visited_signatures));
    }

    for param in signature.param_docs.values() {
        if type_contains_sync_callable_with(db, &param.type_ref, visited_signatures) {
            info.sync_callback_params = true;
        }
    }

    info
}

fn type_await_info(db: &DbIndex, typ: &LuaType) -> AwaitCallableInfo {
    let mut visited_signatures = HashSet::new();
    type_await_info_with(db, typ, &mut visited_signatures)
}

fn type_await_info_with(
    db: &DbIndex,
    typ: &LuaType,
    visited_signatures: &mut HashSet<LuaSignatureId>,
) -> AwaitCallableInfo {
    let mut info = AwaitCallableInfo::default();
    typ.visit_type(&mut |inner_type| {
        if info.async_callable && info.sync_callback_params {
            return;
        }

        match inner_type {
            LuaType::DocFunction(func) => {
                info.include(function_await_info(db, func, visited_signatures));
            }
            LuaType::Signature(signature_id) => {
                if let Some(signature) = db.get_signature_index().get(signature_id) {
                    info.include(signature_await_info(
                        db,
                        *signature_id,
                        signature,
                        visited_signatures,
                    ));
                }
            }
            _ => {}
        }
    });
    info
}

fn function_await_info(
    db: &DbIndex,
    func: &LuaFunctionType,
    visited_signatures: &mut HashSet<LuaSignatureId>,
) -> AwaitCallableInfo {
    AwaitCallableInfo {
        async_callable: func.get_async_state() == AsyncState::Async,
        sync_callback_params: function_has_sync_callback_param(db, func, visited_signatures),
    }
}

fn function_has_sync_callback_param(
    db: &DbIndex,
    func: &LuaFunctionType,
    visited_signatures: &mut HashSet<LuaSignatureId>,
) -> bool {
    func.get_params().iter().any(|(_, param_type)| {
        param_type.as_ref().is_some_and(|param_type| {
            type_contains_sync_callable_with(db, param_type, visited_signatures)
        })
    })
}

fn type_contains_sync_callable_with(
    db: &DbIndex,
    typ: &LuaType,
    visited_signatures: &mut HashSet<LuaSignatureId>,
) -> bool {
    let mut has_sync_callable = false;
    typ.visit_type(&mut |inner_type| {
        if has_sync_callable {
            return;
        }

        match inner_type {
            LuaType::DocFunction(func) => {
                has_sync_callable = func.get_async_state() == AsyncState::Sync;
            }
            LuaType::Signature(signature_id) => {
                has_sync_callable =
                    db.get_signature_index()
                        .get(signature_id)
                        .is_some_and(|signature| {
                            signature.async_state == AsyncState::Sync
                                || signature
                                    .overloads
                                    .iter()
                                    .any(|overload| overload.get_async_state() == AsyncState::Sync)
                                || {
                                    let nested = signature_await_info(
                                        db,
                                        *signature_id,
                                        signature,
                                        visited_signatures,
                                    );
                                    nested.sync_callback_params
                                }
                        });
            }
            _ => {}
        }
    });
    has_sync_callable
}

fn check_call_in_async(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    call_expr: LuaCallExpr,
    candidates: &PrecomputedAwaitCandidates,
) -> Option<()> {
    if !candidates.should_check_direct_call(&call_expr) {
        return Some(());
    }

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
            for member_type in union.into_vec().iter() {
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
    candidates: &PrecomputedAwaitCandidates,
) -> Option<()> {
    if !candidates.should_check_callback_arg(&call_expr) {
        return Some(());
    }

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
