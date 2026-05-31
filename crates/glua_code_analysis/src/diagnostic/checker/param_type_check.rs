use std::{collections::HashSet, sync::Arc, time::Duration};

use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaAstToken, LuaCallExpr, LuaExpr, LuaIndexExpr, LuaIndexKey,
    LuaVarExpr, PathTrait,
};
use rowan::TextRange;

use crate::{
    DbIndex, DiagnosticCode, LuaFunctionType, LuaMemberId, LuaMemberKey, LuaMemberOwner,
    LuaOperatorMetaMethod, LuaOperatorOwner, LuaSemanticDeclId, LuaSignature, LuaSignatureId,
    LuaType, LuaTypeOwner, LuaUnionType, RenderLevel, SemanticDeclLevel, SemanticModel,
    TypeCheckFailReason, TypeCheckResult, TypeVisitTrait, VariadicType,
    diagnostic::checker::assign_type_mismatch::check_table_expr, humanize_type, infer_index_expr,
};

use super::{Checker, DiagnosticContext, should_suppress_inferred_value_mismatch};

pub struct ParamTypeCheckChecker;

impl Checker for ParamTypeCheckChecker {
    const CODES: &[DiagnosticCode] = &[
        DiagnosticCode::ParamTypeMismatch,
        DiagnosticCode::AssignTypeMismatch,
    ];

    /// a simple implementation of param type check, later we will do better
    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let root = semantic_model.get_root().clone();
        let candidates = context
            .get_shared_data_arc()
            .map(|shared_data| shared_data.param_type_candidates.clone())
            .unwrap_or_else(|| Arc::new(precompute_param_type_candidates(semantic_model.get_db())));
        let profile_enabled = log::log_enabled!(log::Level::Info);
        let mut profile = profile_enabled.then(ParamTypeCheckProfile::default);

        for call_expr in root.descendants::<LuaCallExpr>() {
            if context.is_cancelled() {
                return;
            }
            if let Some(profile) = profile.as_mut() {
                profile.calls_scanned += 1;
            }
            let call_start = profile_enabled.then(std::time::Instant::now);
            check_call_expr(
                context,
                semantic_model,
                call_expr,
                &candidates,
                profile.as_mut(),
            );
            if let (Some(profile), Some(call_start)) = (profile.as_mut(), call_start) {
                profile.total_call_time += call_start.elapsed();
            }
        }

        if let Some(profile) = profile {
            profile.log(semantic_model.get_file_id());
        }
    }
}

#[derive(Default)]
struct ParamTypeCheckProfile {
    calls_scanned: usize,
    calls_without_actionable_args: usize,
    static_candidate_hits: usize,
    static_candidate_misses: usize,
    unknown_static_names: usize,
    calls_checked: usize,
    calls_without_signature: usize,
    infer_func_time: Duration,
    infer_arg_time: Duration,
    total_call_time: Duration,
}

impl ParamTypeCheckProfile {
    fn log(&self, file_id: crate::FileId) {
        log::info!(
            "param type profile: file={:?} calls_scanned={} no_args={} static_hits={} static_misses={} unknown_names={} calls_checked={} no_signature={} infer_func_time={:?} infer_arg_time={:?} total_call_time={:?}",
            file_id,
            self.calls_scanned,
            self.calls_without_actionable_args,
            self.static_candidate_hits,
            self.static_candidate_misses,
            self.unknown_static_names,
            self.calls_checked,
            self.calls_without_signature,
            self.infer_func_time,
            self.infer_arg_time,
            self.total_call_time,
        );
    }
}

#[derive(Debug, Default)]
pub struct PrecomputedParamTypeCandidates {
    typed_callee_names: HashSet<String>,
}

impl PrecomputedParamTypeCandidates {
    fn should_check_call_with_profile(
        &self,
        call_expr: &LuaCallExpr,
        mut profile: Option<&mut ParamTypeCheckProfile>,
    ) -> bool {
        if self.typed_callee_names.is_empty() {
            return true;
        }
        match static_call_name(call_expr) {
            StaticCallName::Static(name) if self.typed_callee_names.contains(name.as_str()) => {
                if let Some(profile) = profile.as_mut() {
                    profile.static_candidate_hits += 1;
                }
                true
            }
            StaticCallName::Static(_) => {
                if let Some(profile) = profile.as_mut() {
                    profile.static_candidate_misses += 1;
                }
                false
            }
            StaticCallName::Unknown => {
                if let Some(profile) = profile.as_mut() {
                    profile.unknown_static_names += 1;
                }
                true
            }
        }
    }

    fn insert_owner(&mut self, db: &DbIndex, owner: &LuaTypeOwner, is_candidate: bool) {
        if !is_candidate {
            return;
        }

        if let Some(name) = owner_name(db, owner) {
            self.typed_callee_names.insert(name);
        }
    }

    fn insert_signature_param_names(&mut self, db: &DbIndex, signature: &LuaSignature) {
        for param in signature.param_docs.values() {
            let mut visited_signatures = HashSet::new();
            if type_is_callable_with_actionable_params(db, &param.type_ref, &mut visited_signatures)
            {
                self.typed_callee_names.insert(param.name.clone());
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

pub fn precompute_param_type_candidates(db: &DbIndex) -> PrecomputedParamTypeCandidates {
    let mut candidates = PrecomputedParamTypeCandidates::default();

    for (_, signature) in db.get_signature_index().iter() {
        candidates.insert_signature_param_names(db, signature);
    }

    for (owner, type_cache) in db.get_type_index().iter_type_caches() {
        let mut visited_signatures = HashSet::new();
        let is_candidate = type_is_callable_with_actionable_params(
            db,
            type_cache.as_type(),
            &mut visited_signatures,
        );
        candidates.insert_owner(db, owner, is_candidate);
    }

    for (owner_id, _) in db.get_property_index().iter_owner_properties() {
        let Some(owner) = property_owner_type_owner(owner_id) else {
            continue;
        };
        let mut visited_signatures = HashSet::new();
        let is_candidate = db
            .get_type_index()
            .get_type_cache(&owner)
            .is_some_and(|type_cache| {
                type_is_callable_with_actionable_params(
                    db,
                    type_cache.as_type(),
                    &mut visited_signatures,
                )
            });
        candidates.insert_owner(db, &owner, is_candidate);
    }

    candidates
}

fn property_owner_type_owner(owner_id: &LuaSemanticDeclId) -> Option<LuaTypeOwner> {
    match owner_id {
        LuaSemanticDeclId::LuaDecl(decl_id) => Some(LuaTypeOwner::Decl(*decl_id)),
        LuaSemanticDeclId::Member(member_id) => Some(LuaTypeOwner::Member(*member_id)),
        LuaSemanticDeclId::TypeDecl(_) | LuaSemanticDeclId::Signature(_) => None,
    }
}

fn type_is_callable_with_actionable_params(
    db: &DbIndex,
    typ: &LuaType,
    visited_signatures: &mut HashSet<LuaSignatureId>,
) -> bool {
    let mut is_candidate = false;
    typ.visit_type(&mut |inner_type| {
        if is_candidate {
            return;
        }

        match inner_type {
            LuaType::DocFunction(func) => {
                is_candidate = function_has_actionable_params(func);
            }
            LuaType::Ref(type_id) | LuaType::Def(type_id) => {
                is_candidate = db
                    .get_type_index()
                    .get_type_decl(type_id)
                    .and_then(|type_decl| type_decl.get_alias_ref())
                    .is_some_and(|origin| {
                        type_is_callable_with_actionable_params(db, origin, visited_signatures)
                    });
            }
            LuaType::Signature(signature_id) => {
                is_candidate =
                    db.get_signature_index()
                        .get(signature_id)
                        .is_some_and(|signature| {
                            signature_has_actionable_params(
                                db,
                                *signature_id,
                                signature,
                                visited_signatures,
                            )
                        });
            }
            LuaType::TableConst(table) => {
                is_candidate = table_call_operator_has_actionable_params(db, table);
            }
            LuaType::Instance(instance) => {
                is_candidate = type_is_callable_with_actionable_params(
                    db,
                    instance.get_base(),
                    visited_signatures,
                );
            }
            _ => {}
        }
    });
    is_candidate
}

fn table_call_operator_has_actionable_params(
    db: &DbIndex,
    table: &crate::InFiled<TextRange>,
) -> bool {
    let Some(meta_table) = db.get_metatable_index().get(table) else {
        return false;
    };
    let owner = LuaOperatorOwner::Table(meta_table.clone());
    let Some(call_operators) = db
        .get_operator_index()
        .get_operators(&owner, LuaOperatorMetaMethod::Call)
    else {
        return false;
    };

    call_operators.iter().any(|operator_id| {
        let Some(operator) = db.get_operator_index().get_operator(operator_id) else {
            return false;
        };
        match operator.get_operator_func(db) {
            LuaType::DocFunction(func) => {
                call_operator_function_has_actionable_params(func.as_ref())
            }
            LuaType::Signature(signature_id) => {
                let mut visited_signatures = HashSet::new();
                db.get_signature_index()
                    .get(&signature_id)
                    .is_some_and(|signature| {
                        signature_has_actionable_params(
                            db,
                            signature_id,
                            signature,
                            &mut visited_signatures,
                        )
                    })
            }
            _ => false,
        }
    })
}

fn call_operator_function_has_actionable_params(func: &LuaFunctionType) -> bool {
    func.get_params()
        .iter()
        .enumerate()
        .any(|(idx, (name, typ))| {
            let is_hidden_self = idx == 0 && !func.is_colon_define();
            !is_hidden_self && param_type_is_actionable(name, typ.as_ref())
        })
}

fn signature_has_actionable_params(
    db: &DbIndex,
    signature_id: LuaSignatureId,
    signature: &LuaSignature,
    visited_signatures: &mut HashSet<LuaSignatureId>,
) -> bool {
    if !visited_signatures.insert(signature_id) {
        return false;
    }

    signature
        .get_type_params()
        .iter()
        .any(|(name, typ)| param_type_is_actionable(name, typ.as_ref()))
        || signature
            .overloads
            .iter()
            .any(|overload| function_has_actionable_params(overload))
        || signature.param_docs.values().any(|param| {
            type_is_callable_with_actionable_params(db, &param.type_ref, visited_signatures)
        })
}

fn function_has_actionable_params(func: &LuaFunctionType) -> bool {
    func.get_params()
        .iter()
        .any(|(name, typ)| param_type_is_actionable(name, typ.as_ref()))
}

fn param_type_is_actionable(name: &str, typ: Option<&LuaType>) -> bool {
    let Some(typ) = typ else {
        return false;
    };

    !matches!(
        typ,
        LuaType::Any | LuaType::Unknown | LuaType::Never | LuaType::SelfInfer
    ) || name == "..."
}

fn check_call_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    call_expr: LuaCallExpr,
    candidates: &PrecomputedParamTypeCandidates,
    mut profile: Option<&mut ParamTypeCheckProfile>,
) -> Option<()> {
    if context.is_cancelled() {
        return Some(());
    }

    if !call_expr.is_colon_call()
        && call_expr
            .get_args_list()
            .is_none_or(|args| args.get_args().next().is_none())
    {
        if let Some(profile) = profile.as_mut() {
            profile.calls_without_actionable_args += 1;
        }
        return Some(());
    }

    if !candidates.should_check_call_with_profile(&call_expr, profile.as_deref_mut()) {
        return Some(());
    }

    if let Some(profile) = profile.as_mut() {
        profile.calls_checked += 1;
    }
    let infer_func_start = profile.is_some().then(std::time::Instant::now);
    let Some(func) = semantic_model.infer_call_expr_func(call_expr.clone(), None) else {
        if let Some(profile) = profile.as_mut() {
            profile.calls_without_signature += 1;
            if let Some(infer_func_start) = infer_func_start {
                profile.infer_func_time += infer_func_start.elapsed();
            }
        }
        return None;
    };
    if let (Some(profile), Some(infer_func_start)) = (profile.as_mut(), infer_func_start) {
        profile.infer_func_time += infer_func_start.elapsed();
    }
    let mut params = func.get_params().to_vec();
    let colon_call = call_expr.is_colon_call();
    let colon_define = func.is_colon_define();
    if matches!((colon_call, colon_define), (false, true)) {
        // 插入 self 参数
        params.insert(0, ("self".into(), Some(LuaType::SelfInfer)));
    }

    let Some(arg_type_count) = required_arg_type_count(&params, colon_call, colon_define) else {
        return Some(());
    };

    let arg_exprs = call_expr.get_args_list()?.get_args().collect::<Vec<_>>();
    let infer_arg_start = profile.is_some().then(std::time::Instant::now);
    let arg_infos =
        semantic_model.infer_call_arg_expr_list_types(call_expr.clone(), arg_type_count);
    if let (Some(profile), Some(infer_arg_start)) = (profile.as_mut(), infer_arg_start) {
        profile.infer_arg_time += infer_arg_start.elapsed();
    }
    let (mut arg_types, mut arg_ranges): (Vec<LuaType>, Vec<TextRange>) =
        arg_infos.into_iter().unzip();

    match (colon_call, colon_define) {
        (true, true) | (false, false) => {}
        (false, true) => {}
        (true, false) => {
            // 往调用参数插入插入调用者类型
            let source_type = get_call_source_type(semantic_model, &call_expr)?;
            arg_types.insert(0, source_type);
            arg_ranges.insert(0, call_expr.get_colon_token()?.get_range());
        }
    }

    for (idx, param) in params.iter().enumerate() {
        if context.is_cancelled() {
            return Some(());
        }
        if param.0 == "..." {
            if arg_types.len() < idx {
                break;
            }

            if let Some(variadic_type) = param.1.clone() {
                check_variadic_param_match_args(
                    context,
                    semantic_model,
                    &variadic_type,
                    &arg_types[idx..],
                    &arg_ranges[idx..],
                );
            }

            break;
        }

        if let Some(param_type) = param.1.clone() {
            if !param_type_needs_arg_inference(&param.0, &param_type, idx) {
                continue;
            }
            let arg_type = arg_types.get(idx).unwrap_or(&LuaType::Any);
            let mut check_type = param_type.clone();
            let was_self_infer = param_type.is_self_infer();
            // SelfInfer parameters represent `self` — they accept any class type.
            // At idx==0, try to resolve the actual self type from the call context.
            // At any other index, just skip the check since SelfInfer is always compatible.
            if was_self_infer {
                if idx == 0 {
                    let result = get_call_source_type(semantic_model, &call_expr);
                    if let Some(result) = result {
                        check_type = result;
                    } else {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            let result = semantic_model.type_check_detail(&check_type, arg_type);
            if result.is_err() {
                // `never` and `SelfInfer` indicate type inference limitations, skip the diagnostic.
                // When the original param was SelfInfer and the arg is a class/ref/def type,
                // skip the diagnostic — get_call_source_type may resolve to the storage owner
                // instead of the method's original class when a method is captured and stored
                // on a different object. We only skip for class types, not primitives like string,
                // to preserve genuine error detection (e.g., passing a string where entity is expected).
                if matches!(check_type, LuaType::Never | LuaType::SelfInfer)
                    || matches!(arg_type, LuaType::Never | LuaType::SelfInfer)
                    || (was_self_infer
                        && matches!(
                            arg_type,
                            LuaType::Ref(_) | LuaType::Def(_) | LuaType::SelfInfer
                        ))
                {
                    continue;
                }
                // 这里执行了`AssignTypeMismatch`的检查
                if arg_type.is_table() {
                    let arg_expr_idx = match (colon_call, colon_define) {
                        (true, false) => {
                            if idx == 0 {
                                continue;
                            } else {
                                idx - 1
                            }
                        }
                        _ => idx,
                    };

                    // 表字段已经报错了, 则不添加参数不匹配的诊断避免干扰
                    if let Some(arg_expr) = arg_exprs.get(arg_expr_idx)
                        && let Some(add_diagnostic) = check_table_expr(
                            context,
                            semantic_model,
                            rowan::NodeOrToken::Node(arg_expr.syntax().clone()),
                            arg_expr,
                            Some(&param_type),
                        )
                        && add_diagnostic
                    {
                        continue;
                    }
                }

                let arg_expr = match (colon_call, colon_define) {
                    (true, false) if idx == 0 => None,
                    (true, false) => arg_exprs.get(idx - 1),
                    _ => arg_exprs.get(idx),
                };

                if let Some(LuaExpr::IndexExpr(index_expr)) = arg_expr
                    && let Ok(flow_arg_type) = infer_index_expr(
                        semantic_model.get_db(),
                        &mut semantic_model.get_cache().borrow_mut(),
                        index_expr.clone(),
                        true,
                    )
                    && semantic_model
                        .type_check_detail(&check_type, &flow_arg_type)
                        .is_ok()
                {
                    continue;
                }

                if let Some(LuaExpr::IndexExpr(index_expr)) = arg_expr
                    && rewritten_collection_element_matches_param(
                        semantic_model,
                        index_expr,
                        &check_type,
                    )
                {
                    continue;
                }

                try_add_diagnostic(
                    context,
                    semantic_model,
                    *arg_ranges.get(idx)?,
                    &param_type,
                    arg_type,
                    arg_expr,
                    result,
                );
            }
        }
    }

    Some(())
}

fn required_arg_type_count(
    params: &[(String, Option<LuaType>)],
    colon_call: bool,
    colon_define: bool,
) -> Option<Option<usize>> {
    let mut max_required_arg_index: Option<usize> = None;
    for (idx, param) in params.iter().enumerate() {
        let Some(param_type) = &param.1 else {
            continue;
        };

        if param.0 == "..." {
            return param_type_is_actionable(&param.0, Some(param_type)).then_some(None);
        }

        if !param_type_needs_arg_inference(&param.0, param_type, idx) {
            continue;
        }

        let required_arg_index = match (colon_call, colon_define) {
            (true, false) if idx == 0 => None,
            (true, false) => Some(idx - 1),
            _ => Some(idx),
        };

        if let Some(required_arg_index) = required_arg_index {
            max_required_arg_index = Some(
                max_required_arg_index
                    .map_or(required_arg_index, |max| max.max(required_arg_index)),
            );
        }
    }

    max_required_arg_index
        .map(|idx| Some(Some(idx + 1)))
        .unwrap_or_else(|| {
            if params.iter().enumerate().any(|(idx, (name, typ))| {
                typ.as_ref()
                    .is_some_and(|typ| param_type_needs_arg_inference(name, typ, idx))
            }) {
                Some(Some(0))
            } else {
                None
            }
        })
}

fn param_type_needs_arg_inference(name: &str, typ: &LuaType, idx: usize) -> bool {
    if typ.is_self_infer() {
        return idx == 0;
    }

    param_type_is_actionable(name, Some(typ))
}

fn check_variadic_param_match_args(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    variadic_type: &LuaType,
    arg_types: &[LuaType],
    arg_ranges: &[TextRange],
) {
    if let LuaType::Variadic(variadic) = variadic_type
        && let VariadicType::Multi(types) = variadic.as_ref()
    {
        for (idx, (arg_type, arg_range)) in arg_types.iter().zip(arg_ranges.iter()).enumerate() {
            if context.is_cancelled() {
                return;
            }
            let Some(expected_type) = types.get(idx) else {
                break;
            };
            let result = semantic_model.type_check_detail(expected_type, arg_type);
            if result.is_err() {
                try_add_diagnostic(
                    context,
                    semantic_model,
                    *arg_range,
                    expected_type,
                    arg_type,
                    None,
                    result,
                );
            }
        }
        return;
    }

    for (arg_type, arg_range) in arg_types.iter().zip(arg_ranges.iter()) {
        if context.is_cancelled() {
            return;
        }
        let result = semantic_model.type_check_detail(variadic_type, arg_type);
        if result.is_err() {
            try_add_diagnostic(
                context,
                semantic_model,
                *arg_range,
                variadic_type,
                arg_type,
                None,
                result,
            );
        }
    }
}

fn try_add_diagnostic(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    range: TextRange,
    param_type: &LuaType,
    expr_type: &LuaType,
    expr: Option<&LuaExpr>,
    result: TypeCheckResult,
) {
    if let (LuaType::Integer, LuaType::FloatConst(f)) = (param_type, expr_type)
        && f.fract() == 0.0
    {
        return;
    }

    if let Some(expr) = expr
        && should_suppress_inferred_value_mismatch(semantic_model, param_type, expr_type, expr)
    {
        return;
    }

    if is_any_only_uninformative_or_nil(expr_type) {
        return;
    }

    let strict_coercion = semantic_model.get_emmyrc().strict.strict_type_coercion;
    if !strict_coercion && should_suppress_lua_primitive_coercion(param_type, expr_type) {
        return;
    }

    add_type_check_diagnostic(
        context,
        semantic_model,
        range,
        param_type,
        expr_type,
        result,
    );
}

fn add_type_check_diagnostic(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    range: TextRange,
    param_type: &LuaType,
    expr_type: &LuaType,
    result: TypeCheckResult,
) {
    let db = semantic_model.get_db();
    match result {
        Ok(_) => (),
        Err(reason) => {
            let reason_message = match reason {
                TypeCheckFailReason::TypeNotMatchWithReason(reason) => reason,
                TypeCheckFailReason::TypeNotMatch | TypeCheckFailReason::DonotCheck => {
                    "".to_string()
                }
                TypeCheckFailReason::TypeRecursion => "type recursion".to_string(),
            };
            context.add_diagnostic(
                DiagnosticCode::ParamTypeMismatch,
                range,
                t!(
                    "expected `%{source}` but found `%{found}`. %{reason}",
                    source = humanize_type(db, param_type, RenderLevel::Simple),
                    found = humanize_type(db, expr_type, RenderLevel::Simple),
                    reason = reason_message
                )
                .to_string(),
                None,
            );
        }
    }
}

fn rewritten_collection_element_matches_param(
    semantic_model: &SemanticModel,
    arg_index_expr: &LuaIndexExpr,
    param_type: &LuaType,
) -> bool {
    let Some(arg_prefix_expr) = arg_index_expr.get_prefix_expr() else {
        return false;
    };
    let Some(arg_prefix_path) = expr_access_path(&arg_prefix_expr) else {
        return false;
    };
    let Some(arg_index_key) = arg_index_expr.get_index_key() else {
        return false;
    };
    let arg_member_key = LuaMemberKey::from_index_key(
        semantic_model.get_db(),
        &mut semantic_model.get_cache().borrow_mut(),
        &arg_index_key,
    )
    .ok();

    let arg_start = arg_index_expr.get_range().start();
    let mut last_matching_assignment_is_compatible = None;

    for assign_stat in semantic_model.get_root().descendants::<LuaAssignStat>() {
        if assign_stat.get_range().end() >= arg_start {
            continue;
        }

        let (vars, exprs) = assign_stat.get_var_and_expr_list();
        for (idx, var) in vars.iter().enumerate() {
            let LuaVarExpr::IndexExpr(var_index_expr) = var else {
                continue;
            };
            let Some(var_prefix_expr) = var_index_expr.get_prefix_expr() else {
                continue;
            };
            if expr_access_path(&var_prefix_expr).as_deref() != Some(arg_prefix_path.as_str()) {
                continue;
            }
            let Some(expr) = exprs.get(idx).or_else(|| exprs.last()) else {
                last_matching_assignment_is_compatible = Some(false);
                continue;
            };
            let Some(var_index_key) = var_index_expr.get_index_key() else {
                last_matching_assignment_is_compatible = Some(false);
                continue;
            };
            let is_exact_arg_key = arg_member_key.as_ref().is_some_and(|arg_member_key| {
                LuaMemberKey::from_index_key(
                    semantic_model.get_db(),
                    &mut semantic_model.get_cache().borrow_mut(),
                    &var_index_key,
                )
                .is_ok_and(|var_member_key| var_member_key == *arg_member_key)
            });
            let is_iter_rewrite = matches!(&var_index_key, LuaIndexKey::Expr(_)) && {
                let LuaIndexKey::Expr(index_key_expr) = var_index_key.clone() else {
                    unreachable!();
                };
                crate::semantic::check_iter_var_range(
                    semantic_model.get_db(),
                    &mut semantic_model.get_cache().borrow_mut(),
                    &index_key_expr,
                    var_prefix_expr,
                )
                .unwrap_or(false)
            };

            if !is_exact_arg_key && !is_iter_rewrite {
                if LuaMemberKey::from_index_key(
                    semantic_model.get_db(),
                    &mut semantic_model.get_cache().borrow_mut(),
                    &var_index_key,
                )
                .is_err()
                {
                    last_matching_assignment_is_compatible = Some(false);
                }
                continue;
            }

            let Ok(expr_type) = semantic_model.infer_expr(expr.clone()) else {
                last_matching_assignment_is_compatible = Some(false);
                continue;
            };
            last_matching_assignment_is_compatible = Some(
                semantic_model
                    .type_check_detail(param_type, &expr_type)
                    .is_ok(),
            );
        }
    }

    last_matching_assignment_is_compatible == Some(true)
}

fn expr_access_path(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::NameExpr(name_expr) => name_expr.get_access_path(),
        LuaExpr::IndexExpr(index_expr) => index_expr.get_access_path(),
        _ => None,
    }
}

pub fn get_call_source_type(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
) -> Option<LuaType> {
    match call_expr.get_prefix_expr()? {
        LuaExpr::IndexExpr(index_expr) => {
            let decl = semantic_model.find_decl(
                index_expr.syntax().clone().into(),
                SemanticDeclLevel::default(),
            )?;

            if let LuaSemanticDeclId::Member(member_id) = decl
                && let Some(LuaSemanticDeclId::Member(member_id)) =
                    semantic_model.get_member_origin_owner(member_id)
            {
                // First try to resolve the self type from the member owner stored in the index.
                // This is reliable even when the member is defined in a different file.
                if let Some(owner_type) = get_self_type_from_member(semantic_model, member_id) {
                    return Some(owner_type);
                }

                let root = semantic_model
                    .get_db()
                    .get_vfs()
                    .get_syntax_tree(&member_id.file_id)?
                    .get_red_root();
                let cur_node = member_id.get_syntax_id().to_node_from_root(&root)?;
                let index_expr = LuaIndexExpr::cast(cur_node)?;

                return index_expr.get_prefix_expr().map(|prefix_expr| {
                    semantic_model
                        .infer_expr(prefix_expr.clone())
                        .unwrap_or(LuaType::SelfInfer)
                });
            }

            return if let Some(prefix_expr) = index_expr.get_prefix_expr() {
                let expr_type = semantic_model
                    .infer_expr(prefix_expr.clone())
                    .unwrap_or(LuaType::SelfInfer);
                Some(expr_type)
            } else {
                None
            };
        }
        LuaExpr::NameExpr(name_expr) => {
            let decl = semantic_model.find_decl(
                name_expr.syntax().clone().into(),
                SemanticDeclLevel::default(),
            )?;
            if let LuaSemanticDeclId::Member(member_id) = decl {
                // First try to resolve the self type from the member owner stored in the index.
                // This avoids cross-file inference failures when the member is defined in a
                // different file (e.g. GLua API annotations), where inferring the prefix
                // expression would fail and incorrectly fall back to SelfInfer.
                if let Some(owner_type) = get_self_type_from_member(semantic_model, member_id) {
                    return Some(owner_type);
                }

                let root = semantic_model
                    .get_db()
                    .get_vfs()
                    .get_syntax_tree(&member_id.file_id)?
                    .get_red_root();
                let cur_node = member_id.get_syntax_id().to_node_from_root(&root)?;
                let index_expr = LuaIndexExpr::cast(cur_node)?;

                return index_expr.get_prefix_expr().map(|prefix_expr| {
                    semantic_model
                        .infer_expr(prefix_expr.clone())
                        .unwrap_or(LuaType::SelfInfer)
                });
            }

            return None;
        }
        _ => {}
    }

    None
}

/// Returns the owner type of a member as a `LuaType::Ref` if the member belongs to a named class.
/// Returns `None` for table/element/global-scoped members where the owner type cannot be
/// expressed as a simple class reference.
fn get_self_type_from_member(
    semantic_model: &SemanticModel,
    member_id: LuaMemberId,
) -> Option<LuaType> {
    match semantic_model
        .get_db()
        .get_member_index()
        .get_current_owner(&member_id)?
    {
        LuaMemberOwner::Type(type_id) => Some(LuaType::Ref(type_id.clone())),
        _ => None,
    }
}

/// Suppresses `param-type-mismatch` for implicit coercions that are safe in GLua:
/// - `number`, `integer`, `boolean` (and their const variants) passed to a `string` param
/// - Numeric string literals (e.g. `"2"`) passed to a `number` or `integer` param
///
/// Tables, userdata, and named class instances are deliberately NOT suppressed because passing
/// them to a string/number param almost always indicates a programmer error.
fn should_suppress_lua_primitive_coercion(param_type: &LuaType, expr_type: &LuaType) -> bool {
    // Single call; derive nullability from whether stripping changed the pointer.
    let core_param = strip_nullable_param(param_type);
    let param_is_nullable = !std::ptr::eq(core_param, param_type);

    match expr_type {
        LuaType::Union(union) => match union.as_ref() {
            // `T|nil` — T must be coercible and param must accept nil.
            LuaUnionType::Nullable(inner) => {
                param_is_nullable && is_primitive_coercible_to(core_param, inner)
            }
            // General union — all members must be coercible; nil passes only if param is nullable.
            // Iterates the inner Vec directly — no allocation.
            LuaUnionType::Multi(types) => {
                !types.is_empty()
                    && types.iter().all(|t| {
                        if matches!(t, LuaType::Nil) {
                            param_is_nullable
                        } else {
                            is_primitive_coercible_to(core_param, t)
                        }
                    })
            }
        },
        _ => is_primitive_coercible_to(core_param, expr_type),
    }
}

/// Unwraps a nullable param type (`T?` → `T`), returning the inner type.
fn strip_nullable_param(ty: &LuaType) -> &LuaType {
    if let LuaType::Union(union) = ty {
        if let LuaUnionType::Nullable(inner) = union.as_ref() {
            return inner;
        }
    }
    ty
}

fn is_any_only_uninformative_or_nil(ty: &LuaType) -> bool {
    contains_any(ty) && is_only_any_unknown_or_nil(ty)
}

fn contains_any(ty: &LuaType) -> bool {
    match ty {
        LuaType::Any => true,
        LuaType::Union(union) => union.into_vec().iter().any(contains_any),
        LuaType::MultiLineUnion(union) => union.get_unions().iter().any(|(ty, _)| contains_any(ty)),
        _ => false,
    }
}

fn is_only_any_unknown_or_nil(ty: &LuaType) -> bool {
    match ty {
        LuaType::Any | LuaType::Unknown | LuaType::Nil => true,
        LuaType::Union(union) => union.into_vec().iter().all(is_only_any_unknown_or_nil),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .all(|(ty, _)| is_only_any_unknown_or_nil(ty)),
        _ => false,
    }
}

/// Returns `true` if `expr_type` can be safely passed where `param_type` is expected,
/// under GLua's implicit coercion rules.
fn is_primitive_coercible_to(param_type: &LuaType, expr_type: &LuaType) -> bool {
    match param_type {
        // `string` params accept any primitive that `tostring()` handles cleanly,
        // plus `any`/`unknown` which are too unspecific to warrant a diagnostic.
        LuaType::String => {
            is_tostring_coercible(expr_type) || matches!(expr_type, LuaType::Any | LuaType::Unknown)
        }
        // `number`/`integer` params accept string literals that are valid numbers,
        // e.g. the elements of `string.Explode(" ", "10 20 30")`.
        // `any`/`unknown` are too unspecific to warrant a diagnostic.
        LuaType::Number | LuaType::Integer => {
            is_number_coercible(expr_type) || matches!(expr_type, LuaType::Any | LuaType::Unknown)
        }
        _ => false,
    }
}

fn is_number_coercible(ty: &LuaType) -> bool {
    matches!(
        ty,
        LuaType::Number
            | LuaType::Integer
            | LuaType::FloatConst(_)
            | LuaType::IntegerConst(_)
            | LuaType::DocIntegerConst(_)
    ) || is_numeric_string_literal(ty)
}

/// Returns `true` if `ty` is a string constant that is a valid number (integer or float).
fn is_numeric_string_literal(ty: &LuaType) -> bool {
    match ty {
        LuaType::StringConst(s) | LuaType::DocStringConst(s) => s.parse::<f64>().is_ok(),
        _ => false,
    }
}

/// Returns `true` if `ty` is a primitive whose `tostring()` result is well-defined:
/// booleans, numbers (all numeric kinds), and string types.
fn is_tostring_coercible(ty: &LuaType) -> bool {
    matches!(
        ty,
        LuaType::Boolean
            | LuaType::BooleanConst(_)
            | LuaType::DocBooleanConst(_)
            | LuaType::Number
            | LuaType::Integer
            | LuaType::FloatConst(_)
            | LuaType::IntegerConst(_)
            | LuaType::DocIntegerConst(_)
            | LuaType::String
            | LuaType::StringConst(_)
            | LuaType::DocStringConst(_)
            | LuaType::StrTplRef(_)
    )
}
