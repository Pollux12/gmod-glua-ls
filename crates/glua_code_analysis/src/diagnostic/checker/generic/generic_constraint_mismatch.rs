use glua_parser::{LuaAst, LuaAstNode, LuaAstToken, LuaCallExpr, LuaDocTagType};
use lsp_types::DiagnosticSeverity;
use rowan::TextRange;

use crate::diagnostic::{checker::Checker, lua_diagnostic::DiagnosticContext};
use crate::semantic::{
    CallConstraintContext, build_call_constraint_context, normalize_constraint_type,
};
use crate::{
    DiagnosticCode, DocTypeInferContext, LuaStringTplType, LuaType, RenderLevel, SemanticModel,
    TypeCheckFailReason, TypeCheckResult, TypeSubstitutor, VariadicType, humanize_type,
    infer_doc_type, instantiate_type_generic,
};

pub struct GenericConstraintMismatchChecker;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StrTplArgClass {
    Compatible,
    Unprovable,
    Incompatible,
}

impl Checker for GenericConstraintMismatchChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::GenericConstraintMismatch];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let root = semantic_model.get_root().clone();
        for node in root.descendants::<LuaAst>() {
            match node {
                LuaAst::LuaCallExpr(call_expr) => {
                    check_call_expr(context, semantic_model, call_expr);
                }
                LuaAst::LuaDocTagType(doc_tag_type) => {
                    check_doc_tag_type(context, semantic_model, doc_tag_type);
                }
                _ => {}
            }
        }
    }
}

fn check_call_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    call_expr: LuaCallExpr,
) -> Option<()> {
    let Some((
        CallConstraintContext {
            params,
            arg_infos,
            substitutor,
        },
        doc_func,
    )) = build_call_constraint_context(semantic_model, &call_expr)
    else {
        return Some(());
    };

    let mut arg_ranges = collect_arg_ranges(semantic_model, &call_expr);
    if call_expr.is_colon_call() && !doc_func.is_colon_define() {
        let colon_range = call_expr.get_colon_token()?.get_range();
        arg_ranges.insert(0, colon_range);
    }

    for (i, (_, param_type)) in params.iter().enumerate() {
        let param_type = if let Some(param_type) = param_type {
            param_type
        } else {
            continue;
        };

        check_param(
            context,
            semantic_model,
            i,
            param_type,
            &arg_infos,
            &arg_ranges,
            false,
            &substitutor,
        );
    }

    Some(())
}

fn collect_arg_ranges(semantic_model: &SemanticModel, call_expr: &LuaCallExpr) -> Vec<TextRange> {
    let Some(arg_list) = call_expr.get_args_list() else {
        return Vec::new();
    };
    let arg_exprs = arg_list.get_args().collect::<Vec<_>>();
    let mut ranges = Vec::new();
    for expr in arg_exprs {
        let expr_type = semantic_model
            .infer_expr(expr.clone())
            .unwrap_or(LuaType::Unknown);
        match expr_type {
            LuaType::Variadic(variadic) => match variadic.as_ref() {
                VariadicType::Base(_) => ranges.push(expr.get_range()),
                VariadicType::Multi(values) => {
                    for _ in values {
                        ranges.push(expr.get_range());
                    }
                }
            },
            _ => ranges.push(expr.get_range()),
        }
    }
    ranges
}

fn check_doc_tag_type(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    doc_tag_type: LuaDocTagType,
) -> Option<()> {
    let type_list = doc_tag_type.get_type_list();
    let doc_ctx = DocTypeInferContext::new(semantic_model.get_db(), semantic_model.get_file_id());
    for doc_type in type_list {
        let type_ref = infer_doc_type(doc_ctx, &doc_type);
        let generic_type = match type_ref {
            LuaType::Generic(generic_type) => generic_type,
            _ => continue,
        };

        let generic_params = semantic_model
            .get_db()
            .get_type_index()
            .get_generic_params(&generic_type.get_base_type_id())?;
        for (i, param_type) in generic_type.get_params().iter().enumerate() {
            let extend_type = generic_params.get(i)?.type_constraint.clone()?;
            let result = semantic_model.type_check_detail(&extend_type, param_type);
            if result.is_err() {
                add_type_check_diagnostic(
                    context,
                    semantic_model,
                    doc_type.get_range(),
                    &extend_type,
                    param_type,
                    result,
                );
            }
        }
    }
    Some(())
}

#[allow(clippy::too_many_arguments)]
fn check_param(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    param_index: usize,
    param_type: &LuaType,
    arg_infos: &[LuaType],
    arg_ranges: &[TextRange],
    from_union: bool,
    substitutor: &TypeSubstitutor,
) -> Option<()> {
    // 应该先通过泛型体操约束到唯一类型再进行检查
    match param_type {
        LuaType::StrTplRef(str_tpl_ref) => {
            let extend_type = str_tpl_ref.get_constraint().cloned().map(|ty| {
                normalize_constraint_type(
                    semantic_model.get_db(),
                    instantiate_type_generic(semantic_model.get_db(), &ty, substitutor),
                )
            });
            let arg_type = arg_infos.get(param_index)?;
            let arg_range = arg_ranges.get(param_index).copied()?;

            if from_union && !arg_type.is_string() {
                return None;
            }

            validate_str_tpl_ref(
                context,
                semantic_model,
                str_tpl_ref,
                arg_type,
                arg_range,
                extend_type,
            );
        }
        LuaType::TplRef(tpl_ref) | LuaType::ConstTplRef(tpl_ref) => {
            if from_union && let Some(arg_type) = arg_infos.get(param_index) {
                if let LuaType::StringConst(type_name) | LuaType::DocStringConst(type_name) =
                    arg_type
                    && semantic_model
                        .get_db()
                        .get_type_index()
                        .find_type_decl(semantic_model.get_file_id(), &type_name)
                        .is_some_and(|decl| !decl.is_auto_generated())
                {
                    return None;
                }
            }
            let extend_type = tpl_ref.get_constraint().cloned().map(|ty| {
                normalize_constraint_type(
                    semantic_model.get_db(),
                    instantiate_type_generic(semantic_model.get_db(), &ty, substitutor),
                )
            });
            let arg_type = arg_infos.get(param_index);
            let arg_range = arg_ranges.get(param_index).copied();
            validate_tpl_ref(context, semantic_model, &extend_type, arg_type, arg_range);
        }
        LuaType::Union(union_type) => {
            // 如果不是来自 union, 才展开 union 中的每个类型进行检查
            if !from_union {
                for union_member_type in union_type.types() {
                    check_param(
                        context,
                        semantic_model,
                        param_index,
                        union_member_type,
                        arg_infos,
                        arg_ranges,
                        true,
                        substitutor,
                    );
                }
            }
        }
        _ => {}
    }
    Some(())
}

fn validate_str_tpl_ref(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    str_tpl_ref: &LuaStringTplType,
    arg_type: &LuaType,
    range: TextRange,
    extend_type: Option<LuaType>,
) -> Option<()> {
    match arg_type {
        LuaType::StringConst(str) | LuaType::DocStringConst(str) => {
            emit_str_tpl_const_diagnostic(
                context,
                semantic_model,
                str_tpl_ref,
                str,
                range,
                extend_type.as_ref(),
            );
        }
        LuaType::String | LuaType::Any | LuaType::Unknown | LuaType::StrTplRef(_) => {}
        LuaType::Union(_) => {
            // Diagnostics are pushed directly with no deduplication, so union handling emits once.
            match classify_str_tpl_arg(semantic_model, str_tpl_ref, arg_type, &extend_type) {
                StrTplArgClass::Compatible => {}
                StrTplArgClass::Unprovable => {
                    if let Some(str) = first_unprovable_str_tpl_const(
                        semantic_model,
                        str_tpl_ref,
                        arg_type,
                        &extend_type,
                    ) {
                        emit_str_tpl_const_diagnostic(
                            context,
                            semantic_model,
                            str_tpl_ref,
                            &str,
                            range,
                            extend_type.as_ref(),
                        );
                    }
                }
                StrTplArgClass::Incompatible => {
                    context.add_diagnostic(
                        DiagnosticCode::GenericConstraintMismatch,
                        range,
                        "the string template type must be a string constant".to_string(),
                        None,
                    );
                }
            }
        }
        _ => {
            context.add_diagnostic(
                DiagnosticCode::GenericConstraintMismatch,
                range,
                "the string template type must be a string constant".to_string(),
                None,
            );
        }
    }
    Some(())
}

fn classify_str_tpl_arg(
    semantic_model: &SemanticModel,
    str_tpl_ref: &LuaStringTplType,
    arg: &LuaType,
    extend_type: &Option<LuaType>,
) -> StrTplArgClass {
    match arg {
        LuaType::StringConst(str) | LuaType::DocStringConst(str) => {
            classify_str_tpl_const_arg(semantic_model, str_tpl_ref, str, extend_type.as_ref())
        }
        LuaType::String | LuaType::Any | LuaType::Unknown | LuaType::StrTplRef(_) => {
            StrTplArgClass::Compatible
        }
        LuaType::Union(union) => {
            let mut best = StrTplArgClass::Incompatible;
            for member in union.types() {
                match classify_str_tpl_arg(semantic_model, str_tpl_ref, member, extend_type) {
                    StrTplArgClass::Compatible => return StrTplArgClass::Compatible,
                    StrTplArgClass::Unprovable => best = StrTplArgClass::Unprovable,
                    StrTplArgClass::Incompatible => {}
                }
            }
            best
        }
        _ => StrTplArgClass::Incompatible,
    }
}

fn classify_str_tpl_const_arg(
    semantic_model: &SemanticModel,
    str_tpl_ref: &LuaStringTplType,
    str: &str,
    extend_type: Option<&LuaType>,
) -> StrTplArgClass {
    let full_type_name = str_tpl_full_type_name(str_tpl_ref, str);
    let Some(type_decl) = semantic_model
        .get_db()
        .get_type_index()
        .find_type_decl(semantic_model.get_file_id(), &full_type_name)
    else {
        return StrTplArgClass::Unprovable;
    };

    if type_decl.is_auto_generated() {
        return StrTplArgClass::Unprovable;
    }

    if let Some(extend_type) = extend_type {
        let ref_type = LuaType::Ref(type_decl.get_id());
        if semantic_model
            .type_check_detail(extend_type, &ref_type)
            .is_err()
        {
            return StrTplArgClass::Incompatible;
        }
    }

    StrTplArgClass::Compatible
}

fn first_unprovable_str_tpl_const(
    semantic_model: &SemanticModel,
    str_tpl_ref: &LuaStringTplType,
    arg: &LuaType,
    extend_type: &Option<LuaType>,
) -> Option<String> {
    match arg {
        LuaType::StringConst(str) | LuaType::DocStringConst(str)
            if classify_str_tpl_const_arg(
                semantic_model,
                str_tpl_ref,
                str,
                extend_type.as_ref(),
            ) == StrTplArgClass::Unprovable =>
        {
            Some(str.to_string())
        }
        LuaType::Union(union) => {
            for member in union.types() {
                if let Some(str) = first_unprovable_str_tpl_const(
                    semantic_model,
                    str_tpl_ref,
                    member,
                    extend_type,
                ) {
                    return Some(str);
                }
            }
            None
        }
        _ => None,
    }
}

fn emit_str_tpl_const_diagnostic(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    str_tpl_ref: &LuaStringTplType,
    str: &str,
    range: TextRange,
    extend_type: Option<&LuaType>,
) {
    let full_type_name = str_tpl_full_type_name(str_tpl_ref, str);
    let founded_type_decl = semantic_model
        .get_db()
        .get_type_index()
        .find_type_decl(semantic_model.get_file_id(), &full_type_name);
    let is_auto_generated_decl = founded_type_decl
        .as_ref()
        .is_some_and(|type_decl| type_decl.is_auto_generated());
    if is_auto_generated_decl && let Some(extend_type) = extend_type {
        context.add_diagnostic_with_severity(
            DiagnosticCode::GenericConstraintMismatch,
            range,
            format!(
                "Type `{}` is not explicitly defined; auto-created inheriting `{}`",
                full_type_name,
                humanize_type(semantic_model.get_db(), extend_type, RenderLevel::Simple)
            ),
            Some(DiagnosticSeverity::HINT),
            None,
        );
    }

    if founded_type_decl.is_none() {
        if let Some(extend_type) = extend_type {
            context.add_diagnostic_with_severity(
                DiagnosticCode::GenericConstraintMismatch,
                range,
                format!(
                    "type `{}` is not defined in the codebase, using constraint type `{}`",
                    full_type_name,
                    humanize_type(semantic_model.get_db(), extend_type, RenderLevel::Simple)
                ),
                Some(DiagnosticSeverity::HINT),
                None,
            );
        } else {
            context.add_diagnostic(
                DiagnosticCode::GenericConstraintMismatch,
                range,
                "the string template type does not match any type declaration".to_string(),
                None,
            );
        }

        return;
    }

    if let Some(extend_type) = extend_type
        && let Some(type_decl) = founded_type_decl
    {
        let type_id = type_decl.get_id();
        let ref_type = LuaType::Ref(type_id);
        let result = semantic_model.type_check_detail(extend_type, &ref_type);
        if result.is_err() {
            add_type_check_diagnostic(
                context,
                semantic_model,
                range,
                extend_type,
                &ref_type,
                result,
            );
        }
    }
}

fn str_tpl_full_type_name(str_tpl_ref: &LuaStringTplType, str: &str) -> String {
    format!(
        "{}{}{}",
        str_tpl_ref.get_prefix(),
        str,
        str_tpl_ref.get_suffix()
    )
}

fn validate_tpl_ref(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    extend_type: &Option<LuaType>,
    arg_type: Option<&LuaType>,
    range: Option<TextRange>,
) -> Option<()> {
    let extend_type = extend_type.clone()?;
    let arg_type = arg_type?;
    let range = range?;
    let result = semantic_model.type_check_detail(&extend_type, arg_type);
    if result.is_err() {
        add_type_check_diagnostic(
            context,
            semantic_model,
            range,
            &extend_type,
            arg_type,
            result,
        );
    }
    Some(())
}

fn add_type_check_diagnostic(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    range: TextRange,
    extend_type: &LuaType,
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
                DiagnosticCode::GenericConstraintMismatch,
                range,
                format!(
                    "type `{found}` does not satisfy the constraint `{source}`. {reason}",
                    source = humanize_type(db, extend_type, RenderLevel::Simple),
                    found = humanize_type(db, expr_type, RenderLevel::Simple),
                    reason = reason_message
                )
                .to_string(),
                None,
            );
        }
    }
}
