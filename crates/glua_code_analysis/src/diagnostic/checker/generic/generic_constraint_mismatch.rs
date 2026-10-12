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

struct Finding {
    range: TextRange,
    message: String,
    severity: Option<DiagnosticSeverity>,
}

#[derive(Default)]
struct Findings(Vec<Finding>);

impl Findings {
    fn add(&mut self, range: TextRange, message: String) {
        self.add_with_severity(range, message, None);
    }

    fn add_with_severity(
        &mut self,
        range: TextRange,
        message: String,
        severity: Option<DiagnosticSeverity>,
    ) {
        self.0.push(Finding {
            range,
            message,
            severity,
        });
    }

    fn report(self, context: &mut DiagnosticContext) {
        for finding in self.0 {
            context.add_diagnostic_with_severity(
                DiagnosticCode::GenericConstraintMismatch,
                finding.range,
                finding.message,
                finding.severity,
                None,
            );
        }
    }
}

enum ParamVerdict {
    NotApplicable,
    Accepted,
    Rejected(Findings),
}

impl From<Findings> for ParamVerdict {
    fn from(findings: Findings) -> Self {
        if findings.0.is_empty() {
            ParamVerdict::Accepted
        } else {
            ParamVerdict::Rejected(findings)
        }
    }
}

impl Checker for GenericConstraintMismatchChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::GenericConstraintMismatch];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let Some(buckets) = context.file_buckets() else {
            return;
        };
        for node in buckets.all_nodes.iter().cloned() {
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

    // A forwarded `...` fills its slot with one of the pack's values.
    let arg_infos: Vec<LuaType> = arg_infos
        .into_iter()
        .map(|arg_type| match &arg_type {
            LuaType::Variadic(variadic) => match variadic.as_ref() {
                VariadicType::Base(base) => base.clone(),
                VariadicType::Multi(_) => arg_type,
            },
            _ => arg_type,
        })
        .collect();
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

        if let ParamVerdict::Rejected(findings) = check_param(
            semantic_model,
            i,
            param_type,
            &arg_infos,
            &arg_ranges,
            false,
            &substitutor,
        ) {
            findings.report(context);
        }
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
                let mut findings = Findings::default();
                add_type_check_diagnostic(
                    &mut findings,
                    semantic_model,
                    doc_type.get_range(),
                    &extend_type,
                    param_type,
                    result,
                );
                findings.report(context);
            }
        }
    }
    Some(())
}

fn check_param(
    semantic_model: &SemanticModel,
    param_index: usize,
    param_type: &LuaType,
    arg_infos: &[LuaType],
    arg_ranges: &[TextRange],
    from_union: bool,
    substitutor: &TypeSubstitutor,
) -> ParamVerdict {
    let mut findings = Findings::default();
    // Should first be constrained to a single type through generic gymnastics before checking
    match param_type {
        LuaType::StrTplRef(str_tpl_ref) => {
            let extend_type = str_tpl_ref.get_constraint().cloned().map(|ty| {
                normalize_constraint_type(
                    semantic_model.get_db(),
                    instantiate_type_generic(semantic_model.get_db(), &ty, substitutor),
                )
            });
            let (Some(arg_type), Some(arg_range)) =
                (arg_infos.get(param_index), arg_ranges.get(param_index))
            else {
                return ParamVerdict::NotApplicable;
            };

            if from_union && !arg_type.is_string() {
                return ParamVerdict::NotApplicable;
            }

            validate_str_tpl_ref(
                &mut findings,
                semantic_model,
                str_tpl_ref,
                arg_type,
                *arg_range,
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
                    return ParamVerdict::NotApplicable;
                }
            }
            let extend_type = tpl_ref.get_constraint().cloned().map(|ty| {
                normalize_constraint_type(
                    semantic_model.get_db(),
                    instantiate_type_generic(semantic_model.get_db(), &ty, substitutor),
                )
            });
            let (Some(extend_type), Some(arg_type), Some(arg_range)) = (
                extend_type,
                arg_infos.get(param_index),
                arg_ranges.get(param_index),
            ) else {
                return ParamVerdict::NotApplicable;
            };
            validate_tpl_ref(
                &mut findings,
                semantic_model,
                &extend_type,
                arg_type,
                *arg_range,
            );
        }
        LuaType::Union(union_type) if !from_union => {
            // An argument is valid when any arm accepts it; otherwise the first rejecting arm speaks.
            let mut first_rejection = None;
            for union_member_type in union_type.types() {
                match check_param(
                    semantic_model,
                    param_index,
                    union_member_type,
                    arg_infos,
                    arg_ranges,
                    true,
                    substitutor,
                ) {
                    ParamVerdict::Accepted => return ParamVerdict::Accepted,
                    ParamVerdict::Rejected(rejection) => {
                        first_rejection.get_or_insert(rejection);
                    }
                    ParamVerdict::NotApplicable => {}
                }
            }
            return first_rejection.map_or(ParamVerdict::NotApplicable, ParamVerdict::Rejected);
        }
        _ => return ParamVerdict::NotApplicable,
    }
    findings.into()
}

fn validate_str_tpl_ref(
    findings: &mut Findings,
    semantic_model: &SemanticModel,
    str_tpl_ref: &LuaStringTplType,
    arg_type: &LuaType,
    range: TextRange,
    extend_type: Option<LuaType>,
) -> Option<()> {
    match arg_type {
        LuaType::StringConst(str) | LuaType::DocStringConst(str) => {
            emit_str_tpl_const_diagnostic(
                findings,
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
                            findings,
                            semantic_model,
                            str_tpl_ref,
                            &str,
                            range,
                            extend_type.as_ref(),
                        );
                    }
                }
                StrTplArgClass::Incompatible => {
                    findings.add(
                        range,
                        "the string template type must be a string constant".to_string(),
                    );
                }
            }
        }
        _ => {
            findings.add(
                range,
                "the string template type must be a string constant".to_string(),
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
                if let Some(str) =
                    first_unprovable_str_tpl_const(semantic_model, str_tpl_ref, member, extend_type)
                {
                    return Some(str);
                }
            }
            None
        }
        _ => None,
    }
}

fn emit_str_tpl_const_diagnostic(
    findings: &mut Findings,
    semantic_model: &SemanticModel,
    str_tpl_ref: &LuaStringTplType,
    str: &str,
    range: TextRange,
    extend_type: Option<&LuaType>,
) {
    if !crate::semantic::str_tpl_value_names_type(str) {
        return;
    }
    let full_type_name = str_tpl_full_type_name(str_tpl_ref, str);
    let founded_type_decl = semantic_model
        .get_db()
        .get_type_index()
        .find_type_decl(semantic_model.get_file_id(), &full_type_name);
    let is_auto_generated_decl = founded_type_decl
        .as_ref()
        .is_some_and(|type_decl| type_decl.is_auto_generated());
    if is_auto_generated_decl && let Some(extend_type) = extend_type {
        findings.add_with_severity(
            range,
            format!(
                "Type `{}` is not explicitly defined; auto-created inheriting `{}`",
                full_type_name,
                humanize_type(semantic_model.get_db(), extend_type, RenderLevel::Simple)
            ),
            Some(DiagnosticSeverity::HINT),
        );
    }

    if founded_type_decl.is_none() {
        if let Some(extend_type) = extend_type {
            findings.add_with_severity(
                range,
                format!(
                    "type `{}` is not defined in the codebase, using constraint type `{}`",
                    full_type_name,
                    humanize_type(semantic_model.get_db(), extend_type, RenderLevel::Simple)
                ),
                Some(DiagnosticSeverity::HINT),
            );
        } else {
            findings.add(
                range,
                "the string template type does not match any type declaration".to_string(),
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
                findings,
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
    findings: &mut Findings,
    semantic_model: &SemanticModel,
    extend_type: &LuaType,
    arg_type: &LuaType,
    range: TextRange,
) {
    let result = semantic_model.type_check_detail(extend_type, arg_type);
    if result.is_err() {
        add_type_check_diagnostic(
            findings,
            semantic_model,
            range,
            extend_type,
            arg_type,
            result,
        );
    }
}

fn add_type_check_diagnostic(
    findings: &mut Findings,
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
            findings.add(
                range,
                format!(
                    "type `{found}` does not satisfy the constraint `{source}`. {reason}",
                    source = humanize_type(db, extend_type, RenderLevel::Simple),
                    found = humanize_type(db, expr_type, RenderLevel::Simple),
                    reason = reason_message
                )
                .to_string(),
            );
        }
    }
}
