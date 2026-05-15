use std::collections::HashSet;

use glua_parser::{
    LuaAssignStat, LuaAst, LuaAstNode, LuaAstToken, LuaCallExpr, LuaClosureExpr, LuaExpr,
    LuaFuncStat, LuaGeneralToken, LuaIndexExpr, LuaLiteralToken, LuaNameExpr, LuaTableField,
};

use crate::{
    DbIndex, DiagnosticCode, LuaSemanticDeclId, LuaSignatureId, LuaType, SemanticDeclLevel,
    SemanticModel,
};

use super::{Checker, DiagnosticContext};

pub struct CheckParamCountChecker;

impl Checker for CheckParamCountChecker {
    const CODES: &[DiagnosticCode] = &[
        DiagnosticCode::MissingParameter,
        DiagnosticCode::RedundantParameter,
    ];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        for node in semantic_model.get_root().descendants::<LuaAst>() {
            match node {
                LuaAst::LuaCallExpr(call_expr) => {
                    check_call_expr(context, semantic_model, call_expr);
                }
                LuaAst::LuaClosureExpr(closure_expr) => {
                    check_closure_expr(context, semantic_model, &closure_expr);
                }
                _ => {}
            }
        }
    }
}

/// 处理左值已绑定类型但右值为匿名函数的情况
fn check_closure_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    closure_expr: &LuaClosureExpr,
) -> Option<()> {
    let _current_signature =
        context
            .db
            .get_signature_index()
            .get(&LuaSignatureId::from_closure(
                semantic_model.get_file_id(),
                closure_expr,
            ))?;

    let source_typ = semantic_model.infer_bind_value_type(closure_expr.clone().into())?;

    let source_params_len = match &source_typ {
        LuaType::DocFunction(func_type) => {
            let params = func_type.get_params();
            let base = get_params_len(params)?;
            // colon-defined methods have an implicit `self` not listed in params
            Some(if func_type.is_colon_define() {
                base + 1
            } else {
                base
            })
        }
        LuaType::Signature(signature_id) => {
            let signature = context.db.get_signature_index().get(signature_id)?;
            let params = signature.get_type_params();
            let base = get_params_len(&params)?;
            Some(if signature.is_colon_define {
                base + 1
            } else {
                base
            })
        }
        _ => return Some(()),
    }?;

    let params = closure_expr
        .get_params_list()?
        .get_params()
        .collect::<Vec<_>>();

    // 只检查右值参数多于左值参数的情况, 右值参数少于左值参数的情况是能够接受的
    if source_params_len > params.len() {
        return Some(());
    }

    for param in params.iter().skip(source_params_len) {
        context.add_diagnostic(
            DiagnosticCode::RedundantParameter,
            param.get_range(),
            t!(
                "expected %{num} parameters but found %{found_num}",
                num = source_params_len,
                found_num = params.len(),
            )
            .to_string(),
            None,
        );
    }

    Some(())
}

fn check_call_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    call_expr: LuaCallExpr,
) -> Option<()> {
    let func = semantic_model.infer_call_expr_func(call_expr.clone(), None)?;
    let mut fake_params = func.get_params().to_vec();
    let mut fake_param_optional = func.get_optional_params().to_vec();
    if let Some(signature_id) = get_prefix_expr_signature_id(context.db, semantic_model, &call_expr)
        && let Some(signature) = context.db.get_signature_index().get(&signature_id)
        && !signature.param_docs.is_empty()
    {
        let signature_optional = signature.get_param_optional_flags();
        if fake_param_optional.len() < signature_optional.len() {
            fake_param_optional.resize(signature_optional.len(), false);
        }
        for (idx, is_optional) in signature_optional.into_iter().enumerate() {
            if is_optional {
                fake_param_optional[idx] = true;
            }
        }
    }
    let call_args = call_expr.get_args_list()?.get_args().collect::<Vec<_>>();
    let mut call_args_count = call_args.len();
    let last_arg_is_dots = call_args.last().is_some_and(is_dots_expr);
    // 根据冒号定义与冒号调用的情况来调整调用参数的数量
    let colon_call = call_expr.is_colon_call();
    let colon_define = func.is_colon_define();
    match (colon_call, colon_define) {
        (true, true) | (false, false) => {}
        (false, true) => {
            fake_params.insert(0, ("self".to_string(), Some(LuaType::SelfInfer)));
            fake_param_optional.insert(0, false);
        }
        (true, false) => {
            call_args_count += 1;
        }
    }

    // Check for missing parameters
    if call_args_count < fake_params.len() {
        // 调用参数包含 `...`
        for arg in call_args.iter() {
            if let LuaExpr::LiteralExpr(literal_expr) = arg
                && let Some(LuaLiteralToken::Dots(_)) = literal_expr.get_literal()
            {
                return Some(());
            }
        }
        // 对调用参数的最后一个参数进行特殊处理
        if let Some(last_arg) = call_args.last()
            && let Ok(LuaType::Variadic(variadic)) = semantic_model.infer_expr(last_arg.clone())
        {
            let len = match variadic.get_max_len() {
                Some(len) => len,
                None => {
                    return Some(());
                }
            };
            call_args_count = call_args_count + len - 1;
            if call_args_count >= fake_params.len() {
                return Some(());
            }
        }

        let mut miss_parameter_info = Vec::new();

        for i in call_args_count..fake_params.len() {
            let param_info = fake_params.get(i)?;
            if param_info.0 == "..." {
                break;
            }

            let typ = param_info.1.clone();
            if let Some(typ) = typ
                && !is_nullable(context.db, &typ)
                && !fake_param_optional.get(i).copied().unwrap_or(false)
            {
                miss_parameter_info.push(t!("missing parameter: %{name}", name = param_info.0,));
            }
        }

        if !miss_parameter_info.is_empty() {
            let right_paren = call_expr
                .get_args_list()?
                .tokens::<LuaGeneralToken>()
                .last()?;
            context.add_diagnostic(
                DiagnosticCode::MissingParameter,
                right_paren.get_range(),
                t!(
                    "expected %{num} parameters but found %{found_num}. %{infos}",
                    num = fake_params.len(),
                    found_num = call_args_count,
                    infos = miss_parameter_info.join(" \n ")
                )
                .to_string(),
                None,
            );
        }
    }
    // Check for redundant parameters
    else {
        let mut min_call_args_count = call_args_count;
        if last_arg_is_dots {
            min_call_args_count = min_call_args_count.saturating_sub(1);
        }

        if min_call_args_count <= fake_params.len() {
            return Some(());
        }

        if has_override_callable_accepting_call(
            context,
            semantic_model,
            &call_expr,
            min_call_args_count,
            colon_call,
        ) {
            return Some(());
        }

        // 参数定义中最后一个参数是 `...`
        if fake_params.last().is_some_and(|(name, typ)| {
            name == "..." || typ.as_ref().is_some_and(|typ| typ.is_variadic())
        }) {
            return Some(());
        }

        let mut adjusted_index = 0;
        if colon_call != colon_define {
            adjusted_index = if colon_define && !colon_call { -1 } else { 1 };
        }

        for (i, arg) in call_args.iter().enumerate() {
            if last_arg_is_dots && i + 1 == call_args.len() {
                continue;
            }

            let param_index = i as isize + adjusted_index;

            if param_index < 0 || param_index < fake_params.len() as isize {
                continue;
            }

            context.add_diagnostic(
                DiagnosticCode::RedundantParameter,
                arg.get_range(),
                t!(
                    "expected %{num} parameters but found %{found_num}",
                    num = fake_params.len(),
                    found_num = min_call_args_count,
                )
                .to_string(),
                None,
            );
        }
    }

    Some(())
}

fn is_dots_expr(expr: &LuaExpr) -> bool {
    if let LuaExpr::LiteralExpr(literal_expr) = expr
        && let Some(LuaLiteralToken::Dots(_)) = literal_expr.get_literal()
    {
        return true;
    }
    false
}

fn get_prefix_expr_signature_id(
    db: &DbIndex,
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
) -> Option<crate::LuaSignatureId> {
    let prefix_expr = call_expr.get_prefix_expr()?;
    if let LuaExpr::NameExpr(name_expr) = &prefix_expr
        && let Some(signature_id) = get_local_name_signature_id(db, semantic_model, name_expr)
    {
        return Some(signature_id);
    }

    let semantic_decl = semantic_model.find_decl(
        prefix_expr.syntax().clone().into(),
        SemanticDeclLevel::default(),
    )?;
    get_signature_id_from_semantic_decl_value_expr(db, semantic_decl)
}

fn get_local_name_signature_id(
    db: &DbIndex,
    semantic_model: &SemanticModel,
    name_expr: &LuaNameExpr,
) -> Option<crate::LuaSignatureId> {
    let decl_id = db
        .get_reference_index()
        .get_var_reference_decl(&semantic_model.get_file_id(), name_expr.get_range())?;
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let value_syntax_id = decl.get_value_syntax_id()?;
    let root = db.get_vfs().get_syntax_tree(&decl.get_file_id())?;
    let closure = LuaExpr::cast(value_syntax_id.to_node_from_root(&root.get_red_root())?)?;
    let LuaExpr::ClosureExpr(closure) = closure else {
        return None;
    };
    Some(crate::LuaSignatureId::from_closure(
        decl.get_file_id(),
        &closure,
    ))
}

fn get_signature_id_from_semantic_decl_value_expr(
    db: &DbIndex,
    semantic_decl: LuaSemanticDeclId,
) -> Option<crate::LuaSignatureId> {
    if let Some(signature_id) = db.get_property_index().get_signature_owner(&semantic_decl) {
        return Some(signature_id);
    }
    let file_id = match semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => decl_id.file_id,
        LuaSemanticDeclId::Member(member_id) => member_id.file_id,
        LuaSemanticDeclId::Signature(signature_id) => return Some(signature_id),
        LuaSemanticDeclId::TypeDecl(_) => return None,
    };
    let LuaExpr::ClosureExpr(closure) = get_semantic_decl_value_expr(db, semantic_decl)? else {
        return None;
    };
    Some(crate::LuaSignatureId::from_closure(file_id, &closure))
}

fn get_semantic_decl_value_expr(db: &DbIndex, semantic_decl: LuaSemanticDeclId) -> Option<LuaExpr> {
    match semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            let decl = db.get_decl_index().get_decl(&decl_id)?;
            let value_syntax_id = decl.get_value_syntax_id()?;
            let root = db.get_vfs().get_syntax_tree(&decl.get_file_id())?;
            LuaExpr::cast(value_syntax_id.to_node_from_root(&root.get_red_root())?)
        }
        LuaSemanticDeclId::Member(member_id) => get_member_value_expr(db, member_id),
        LuaSemanticDeclId::Signature(_) | LuaSemanticDeclId::TypeDecl(_) => None,
    }
}

fn get_member_value_expr(db: &DbIndex, member_id: crate::LuaMemberId) -> Option<LuaExpr> {
    let root = db
        .get_vfs()
        .get_syntax_tree(&member_id.file_id)?
        .get_red_root();
    let node = member_id.get_syntax_id().to_node_from_root(&root)?;

    if let Some(field) = LuaTableField::cast(node.clone()) {
        return field.get_value_expr();
    }

    if let Some(index_expr) = LuaIndexExpr::cast(node.clone()) {
        if let Some(assign_stat) = index_expr.get_parent::<LuaAssignStat>() {
            let (vars, value_exprs) = assign_stat.get_var_and_expr_list();
            let value_idx = vars
                .iter()
                .position(|var| var.get_syntax_id() == index_expr.get_syntax_id())?;
            return value_exprs.get(value_idx).cloned();
        }

        if let Some(func_stat) = index_expr.get_parent::<LuaFuncStat>() {
            return func_stat.get_closure().map(LuaExpr::ClosureExpr);
        }
    }

    None
}

fn get_params_len(params: &[(String, Option<LuaType>)]) -> Option<usize> {
    if let Some((name, typ)) = params.last() {
        // 如果最后一个参数是可变参数, 则直接返回, 不需要检查
        if name == "..." || typ.as_ref().is_some_and(|typ| typ.is_variadic()) {
            return None;
        }
    }
    Some(params.len())
}

fn is_nullable(db: &DbIndex, typ: &LuaType) -> bool {
    let mut stack: Vec<LuaType> = Vec::new();
    stack.push(typ.clone());
    let mut visited = HashSet::new();
    while let Some(typ) = stack.pop() {
        if visited.contains(&typ) {
            continue;
        }
        visited.insert(typ.clone());
        match typ {
            LuaType::Any | LuaType::Unknown | LuaType::Nil => return true,
            LuaType::Ref(decl_id) => {
                if let Some(decl) = db.get_type_index().get_type_decl(&decl_id)
                    && decl.is_alias()
                    && let Some(alias_origin) = decl.get_alias_ref()
                {
                    stack.push(alias_origin.clone());
                }
            }
            LuaType::Union(u) => {
                for t in u.into_vec() {
                    stack.push(t);
                }
            }
            LuaType::MultiLineUnion(m) => {
                for (t, _) in m.get_unions() {
                    stack.push(t.clone());
                }
            }
            _ => {}
        }
    }
    false
}

fn has_override_callable_accepting_call(
    context: &DiagnosticContext,
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    min_call_args_count: usize,
    colon_call: bool,
) -> bool {
    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return false;
    };
    let Some(LuaSemanticDeclId::Member(member_id)) = semantic_model.find_decl(
        prefix_expr.syntax().clone().into(),
        SemanticDeclLevel::default(),
    ) else {
        return false;
    };

    let Some(member) = context.db.get_member_index().get_member(&member_id) else {
        return false;
    };
    let Some(owner) = context.db.get_member_index().get_current_owner(&member_id) else {
        return false;
    };
    let Some(member_item) = context
        .db
        .get_member_index()
        .get_member_item(owner, member.get_key())
    else {
        return false;
    };

    let visible_member_ids = member_item.visible_member_ids_with_realm_at_offset(
        context.db,
        &semantic_model.get_file_id(),
        call_expr.get_position(),
    );
    if visible_member_ids.is_empty() {
        return false;
    }

    let has_meta_decl = visible_member_ids.iter().any(|visible_member_id| {
        context
            .db
            .get_member_index()
            .get_member(visible_member_id)
            .is_some_and(|visible_member| visible_member.get_feature().is_meta_decl())
    });
    if !has_meta_decl {
        return false;
    }

    visible_member_ids.iter().any(|visible_member_id| {
        let Some(visible_member) = context.db.get_member_index().get_member(visible_member_id)
        else {
            return false;
        };
        let feature = visible_member.get_feature();
        if !feature.is_file_decl() && !feature.is_file_define() {
            return false;
        }

        context
            .db
            .get_type_index()
            .get_type_cache(&visible_member.get_id().into())
            .is_some_and(|cache| {
                callable_accepts_call_args(
                    context.db,
                    cache.as_type(),
                    min_call_args_count,
                    colon_call,
                )
            })
    })
}

fn callable_accepts_call_args(
    db: &DbIndex,
    typ: &LuaType,
    min_call_args_count: usize,
    colon_call: bool,
) -> bool {
    match typ {
        LuaType::DocFunction(function_type) => {
            let Some(mut expected_params_len) = get_params_len(function_type.get_params()) else {
                return true;
            };
            if function_type.is_colon_define() && !colon_call {
                expected_params_len += 1;
            }
            let mut effective_min_call_args_count = min_call_args_count;
            if colon_call && !function_type.is_colon_define() {
                effective_min_call_args_count += 1;
            }
            effective_min_call_args_count <= expected_params_len
        }
        LuaType::Signature(signature_id) => {
            let Some(signature) = db.get_signature_index().get(signature_id) else {
                return false;
            };
            let Some(mut expected_params_len) = get_params_len(&signature.get_type_params()) else {
                return true;
            };
            if signature.is_colon_define && !colon_call {
                expected_params_len += 1;
            }
            let mut effective_min_call_args_count = min_call_args_count;
            if colon_call && !signature.is_colon_define {
                effective_min_call_args_count += 1;
            }
            effective_min_call_args_count <= expected_params_len
        }
        LuaType::Union(union_type) => union_type.into_vec().iter().any(|union_member| {
            callable_accepts_call_args(db, union_member, min_call_args_count, colon_call)
        }),
        _ => false,
    }
}
