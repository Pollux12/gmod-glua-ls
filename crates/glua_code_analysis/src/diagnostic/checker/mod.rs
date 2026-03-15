mod access_invisible;
mod analyze_error;
mod assign_type_mismatch;
mod attribute_check;
mod await_in_sync;
mod call_non_callable;
mod cast_type_mismatch;
mod check_export;
mod check_field;
mod check_param_count;
mod check_return_count;
mod circle_doc_class;
mod code_style;
mod code_style_check;
mod deprecated;
mod discard_returns;
mod duplicate_field;
mod duplicate_index;
mod duplicate_require;
mod duplicate_type;
mod enum_value_mismatch;
mod generic;
mod global_non_module;
mod gmod_hook_name;
mod gmod_network;
mod gmod_realm_misuse;
mod gmod_systems;
mod incomplete_signature_doc;
mod local_const_reassign;
mod missing_fields;
mod need_check_nil;
mod param_type_check;
mod readonly_check;
mod redefined_local;
mod require_module_visibility;
mod return_type_mismatch;
mod syntax_error;
mod unbalanced_assignments;
mod undefined_doc_param;
mod undefined_global;
mod unknown_doc_tag;
mod unnecessary_assert;
mod unnecessary_if;
mod unused;

use glua_parser::{
    LuaAstNode, LuaClosureExpr, LuaComment, LuaExpr, LuaReturnStat, LuaStat, LuaSyntaxKind,
};
use lsp_types::{Diagnostic, DiagnosticSeverity, DiagnosticTag, NumberOrString};
use rowan::TextRange;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::{
    FileId, LuaSemanticDeclId, LuaType, RenderLevel, SemanticDeclLevel, db_index::DbIndex,
    humanize_type, semantic::SemanticModel,
};

use super::{
    DiagnosticCode,
    lua_diagnostic_code::{get_default_severity, is_code_default_enable},
    lua_diagnostic_config::LuaDiagnosticConfig,
};

pub trait Checker {
    const CODES: &[DiagnosticCode];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel);
}

fn run_check<T: Checker>(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    cancel_token: &CancellationToken,
) {
    if cancel_token.is_cancelled() {
        return;
    }

    if T::CODES
        .iter()
        .any(|code| context.is_checker_enable_by_code(code))
    {
        // let name = T::CODES.iter().map(|c| c.get_name()).collect::<Vec<_>>().join(",");
        // let show_name = format!("{}({})", std::any::type_name::<T>(), name);
        // let _p = Profile::new(&show_name);
        T::check(context, semantic_model);
    }
}

pub fn check_file(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    cancel_token: &CancellationToken,
) -> Option<()> {
    run_check::<syntax_error::SyntaxErrorChecker>(context, semantic_model, cancel_token);
    run_check::<analyze_error::AnalyzeErrorChecker>(context, semantic_model, cancel_token);
    run_check::<unused::UnusedChecker>(context, semantic_model, cancel_token);
    run_check::<deprecated::DeprecatedChecker>(context, semantic_model, cancel_token);
    run_check::<undefined_global::UndefinedGlobalChecker>(context, semantic_model, cancel_token);
    run_check::<unnecessary_assert::UnnecessaryAssertChecker>(
        context,
        semantic_model,
        cancel_token,
    );
    run_check::<unnecessary_if::UnnecessaryIfChecker>(context, semantic_model, cancel_token);
    run_check::<access_invisible::AccessInvisibleChecker>(context, semantic_model, cancel_token);
    run_check::<local_const_reassign::LocalConstReassignChecker>(
        context,
        semantic_model,
        cancel_token,
    );
    run_check::<discard_returns::DiscardReturnsChecker>(context, semantic_model, cancel_token);
    run_check::<await_in_sync::AwaitInSyncChecker>(context, semantic_model, cancel_token);
    run_check::<call_non_callable::CallNonCallableChecker>(context, semantic_model, cancel_token);
    run_check::<missing_fields::MissingFieldsChecker>(context, semantic_model, cancel_token);
    run_check::<param_type_check::ParamTypeCheckChecker>(context, semantic_model, cancel_token);
    run_check::<need_check_nil::NeedCheckNilChecker>(context, semantic_model, cancel_token);
    run_check::<code_style_check::CodeStyleCheckChecker>(context, semantic_model, cancel_token);
    run_check::<return_type_mismatch::ReturnTypeMismatch>(context, semantic_model, cancel_token);
    run_check::<undefined_doc_param::UndefinedDocParamChecker>(
        context,
        semantic_model,
        cancel_token,
    );
    run_check::<redefined_local::RedefinedLocalChecker>(context, semantic_model, cancel_token);
    run_check::<check_export::CheckExportChecker>(context, semantic_model, cancel_token);
    run_check::<check_field::CheckFieldChecker>(context, semantic_model, cancel_token);
    run_check::<circle_doc_class::CircleDocClassChecker>(context, semantic_model, cancel_token);
    run_check::<incomplete_signature_doc::IncompleteSignatureDocChecker>(
        context,
        semantic_model,
        cancel_token,
    );
    run_check::<assign_type_mismatch::AssignTypeMismatchChecker>(
        context,
        semantic_model,
        cancel_token,
    );
    run_check::<duplicate_require::DuplicateRequireChecker>(context, semantic_model, cancel_token);
    run_check::<duplicate_type::DuplicateTypeChecker>(context, semantic_model, cancel_token);
    run_check::<check_return_count::CheckReturnCount>(context, semantic_model, cancel_token);
    run_check::<unbalanced_assignments::UnbalancedAssignmentsChecker>(
        context,
        semantic_model,
        cancel_token,
    );
    run_check::<check_param_count::CheckParamCountChecker>(context, semantic_model, cancel_token);
    run_check::<duplicate_field::DuplicateFieldChecker>(context, semantic_model, cancel_token);
    run_check::<duplicate_index::DuplicateIndexChecker>(context, semantic_model, cancel_token);
    run_check::<generic::generic_constraint_mismatch::GenericConstraintMismatchChecker>(
        context,
        semantic_model,
        cancel_token,
    );
    run_check::<cast_type_mismatch::CastTypeMismatchChecker>(context, semantic_model, cancel_token);
    run_check::<require_module_visibility::RequireModuleVisibilityChecker>(
        context,
        semantic_model,
        cancel_token,
    );
    run_check::<unknown_doc_tag::UnknownDocTag>(context, semantic_model, cancel_token);
    run_check::<enum_value_mismatch::EnumValueMismatchChecker>(
        context,
        semantic_model,
        cancel_token,
    );
    run_check::<attribute_check::AttributeCheckChecker>(context, semantic_model, cancel_token);

    run_check::<code_style::non_literal_expressions_in_assert::NonLiteralExpressionsInAssertChecker>(
        context,
        semantic_model,
        cancel_token,
    );
    run_check::<code_style::preferred_local_alias::PreferredLocalAliasChecker>(
        context,
        semantic_model,
        cancel_token,
    );
    run_check::<code_style::invert_if::InvertIfChecker>(context, semantic_model, cancel_token);
    run_check::<readonly_check::ReadOnlyChecker>(context, semantic_model, cancel_token);
    run_check::<global_non_module::GlobalInNonModuleChecker>(context, semantic_model, cancel_token);
    if semantic_model.get_emmyrc().gmod.enabled {
        run_check::<gmod_hook_name::GmodHookNameChecker>(context, semantic_model, cancel_token);
        run_check::<gmod_network::GmodNetworkChecker>(context, semantic_model, cancel_token);
        run_check::<gmod_realm_misuse::GmodRealmMisuseChecker>(
            context,
            semantic_model,
            cancel_token,
        );
        run_check::<gmod_systems::GmodSystemsChecker>(context, semantic_model, cancel_token);
    }
    Some(())
}

pub struct DiagnosticContext<'a> {
    file_id: FileId,
    db: &'a DbIndex,
    diagnostics: Vec<Diagnostic>,
    pub config: Arc<LuaDiagnosticConfig>,
    cancel_token: CancellationToken,
}

impl<'a> DiagnosticContext<'a> {
    pub fn new(
        file_id: FileId,
        db: &'a DbIndex,
        config: Arc<LuaDiagnosticConfig>,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            file_id,
            db,
            diagnostics: Vec::new(),
            config,
            cancel_token,
        }
    }

    pub fn get_db(&self) -> &DbIndex {
        self.db
    }

    pub fn get_file_id(&self) -> FileId {
        self.file_id
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled()
    }

    pub fn add_diagnostic(
        &mut self,
        code: DiagnosticCode,
        range: TextRange,
        message: String,
        data: Option<serde_json::Value>,
    ) {
        self.add_diagnostic_with_severity(code, range, message, None, data);
    }

    pub fn add_diagnostic_with_severity(
        &mut self,
        code: DiagnosticCode,
        range: TextRange,
        message: String,
        severity: Option<DiagnosticSeverity>,
        data: Option<serde_json::Value>,
    ) {
        if self.is_cancelled() {
            return;
        }

        if !self.is_checker_enable_by_code(&code) {
            return;
        }

        if !self.should_report_diagnostic(&code, &range) {
            return;
        }

        let diagnostic = Diagnostic {
            message,
            range: self.translate_range(range).unwrap_or(lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
            }),
            severity: severity.or_else(|| self.get_severity(code)),
            code: Some(NumberOrString::String(code.get_name().to_string())),
            source: Some("GLuaLS".into()),
            tags: self.get_tags(code),
            data,
            ..Default::default()
        };

        self.diagnostics.push(diagnostic);
    }

    fn should_report_diagnostic(&self, code: &DiagnosticCode, range: &TextRange) -> bool {
        let diagnostic_index = self.get_db().get_diagnostic_index();

        !diagnostic_index.is_file_diagnostic_code_disabled(&self.get_file_id(), code, range)
    }

    fn get_severity(&self, code: DiagnosticCode) -> Option<DiagnosticSeverity> {
        if let Some(severity) = self.config.severity.get(&code) {
            return Some(*severity);
        }

        Some(get_default_severity(code))
    }

    fn get_tags(&self, code: DiagnosticCode) -> Option<Vec<DiagnosticTag>> {
        match code {
            DiagnosticCode::Unused
            | DiagnosticCode::UnusedSelf
            | DiagnosticCode::UnreachableCode => Some(vec![DiagnosticTag::UNNECESSARY]),
            DiagnosticCode::Deprecated => Some(vec![DiagnosticTag::DEPRECATED]),
            _ => None,
        }
    }

    fn translate_range(&self, range: TextRange) -> Option<lsp_types::Range> {
        let document = self.db.get_vfs().get_document(&self.file_id)?;
        let (start_line, start_character) = document.get_line_col(range.start())?;
        let (end_line, end_character) = document.get_line_col(range.end())?;

        Some(lsp_types::Range {
            start: lsp_types::Position {
                line: start_line as u32,
                character: start_character as u32,
            },
            end: lsp_types::Position {
                line: end_line as u32,
                character: end_character as u32,
            },
        })
    }

    pub fn get_diagnostics(self) -> Vec<Diagnostic> {
        self.diagnostics
    }

    pub fn is_checker_enable_by_code(&self, code: &DiagnosticCode) -> bool {
        let file_id = self.get_file_id();
        let db = self.get_db();
        let diagnostic_index = db.get_diagnostic_index();
        // force enable
        if diagnostic_index.is_file_enabled(&file_id, code) {
            return true;
        }

        // workspace force disabled
        if self.config.workspace_disabled.contains(code) {
            return false;
        }

        let module_index = db.get_module_index();
        // ignore meta file diagnostic
        if module_index.is_meta_file(&file_id) {
            return false;
        }

        // is file disabled this code
        if diagnostic_index.is_file_disabled(&file_id, code) {
            return false;
        }

        // workspace force enabled
        if self.config.workspace_enabled.contains(code) {
            return true;
        }

        // default setting
        is_code_default_enable(code, self.config.level)
    }
}

fn get_closure_expr_comment(closure_expr: &LuaClosureExpr) -> Option<LuaComment> {
    let comment = closure_expr
        .ancestors::<LuaStat>()
        .next()?
        .syntax()
        .prev_sibling()?;
    match comment.kind().into() {
        LuaSyntaxKind::Comment => {
            let comment = LuaComment::cast(comment)?;
            Some(comment)
        }
        _ => None,
    }
}

/// 获取属于自身的返回语句
pub fn get_return_stats(closure_expr: &LuaClosureExpr) -> impl Iterator<Item = LuaReturnStat> + '_ {
    closure_expr
        .descendants::<LuaReturnStat>()
        .filter(move |stat| {
            stat.ancestors::<LuaClosureExpr>()
                .next()
                .is_some_and(|expr| &expr == closure_expr)
        })
}

pub fn humanize_lint_type(db: &DbIndex, typ: &LuaType) -> String {
    match typ {
        // TODO: 应该仅去掉命名空间
        // LuaType::Ref(type_decl_id) => type_decl_id.get_simple_name().to_string(),
        // LuaType::Generic(generic_type) => generic_type
        //     .get_base_type_id()
        //     .get_simple_name()
        //     .to_string(),
        LuaType::IntegerConst(_) => "integer".to_string(),
        LuaType::FloatConst(_) => "number".to_string(),
        LuaType::BooleanConst(_) => "boolean".to_string(),
        LuaType::StringConst(_) => "string".to_string(),
        LuaType::DocStringConst(_) => "string".to_string(),
        LuaType::DocIntegerConst(_) => "integer".to_string(),
        LuaType::DocBooleanConst(_) => "boolean".to_string(),
        _ => humanize_type(db, typ, RenderLevel::Simple),
    }
}

fn decl_has_inferred_type(semantic_model: &SemanticModel, decl: LuaSemanticDeclId) -> bool {
    let type_owner = match decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => decl_id.into(),
        LuaSemanticDeclId::Member(member_id) => member_id.into(),
        _ => return false,
    };

    semantic_model
        .get_db()
        .get_type_index()
        .get_type_cache(&type_owner)
        .is_some_and(|cache| cache.is_infer())
}

pub fn expr_has_inferred_type(semantic_model: &SemanticModel, expr: &LuaExpr) -> bool {
    match expr {
        LuaExpr::ParenExpr(paren_expr) => paren_expr
            .get_expr()
            .is_some_and(|inner_expr| expr_has_inferred_type(semantic_model, &inner_expr)),
        LuaExpr::UnaryExpr(unary_expr) => unary_expr
            .get_expr()
            .is_some_and(|inner_expr| expr_has_inferred_type(semantic_model, &inner_expr)),
        LuaExpr::BinaryExpr(binary_expr) => binary_expr.get_exprs().is_some_and(|(left, right)| {
            expr_has_inferred_type(semantic_model, &left)
                || expr_has_inferred_type(semantic_model, &right)
        }),
        LuaExpr::NameExpr(name_expr) => semantic_model
            .find_decl(
                rowan::NodeOrToken::Node(name_expr.syntax().clone()),
                SemanticDeclLevel::default(),
            )
            .is_some_and(|decl| decl_has_inferred_type(semantic_model, decl)),
        LuaExpr::IndexExpr(index_expr) => semantic_model
            .find_decl(
                rowan::NodeOrToken::Node(index_expr.syntax().clone()),
                SemanticDeclLevel::default(),
            )
            .map(|decl| decl_has_inferred_type(semantic_model, decl))
            .unwrap_or(true),
        LuaExpr::CallExpr(_) => false,
        LuaExpr::TableExpr(_) | LuaExpr::LiteralExpr(_) | LuaExpr::ClosureExpr(_) => false,
    }
}

fn strip_inferred_uncertainty(typ: &LuaType) -> LuaType {
    match typ {
        LuaType::Union(union) => {
            let stripped = union
                .into_vec()
                .into_iter()
                .filter_map(|member| match member {
                    LuaType::Nil | LuaType::Never | LuaType::Unknown | LuaType::SelfInfer => None,
                    other => Some(strip_inferred_uncertainty(&other)),
                })
                .collect::<Vec<_>>();
            if stripped.is_empty() {
                LuaType::Any
            } else {
                LuaType::from_vec(stripped)
            }
        }
        LuaType::MultiLineUnion(multi_union) => {
            let stripped = multi_union
                .get_unions()
                .iter()
                .filter_map(|(member, _)| match member {
                    LuaType::Nil | LuaType::Never | LuaType::Unknown | LuaType::SelfInfer => None,
                    other => Some(strip_inferred_uncertainty(other)),
                })
                .collect::<Vec<_>>();
            if stripped.is_empty() {
                LuaType::Any
            } else {
                LuaType::from_vec(stripped)
            }
        }
        LuaType::Nil | LuaType::Never | LuaType::Unknown | LuaType::SelfInfer => LuaType::Any,
        LuaType::TableOf(inner) => LuaType::TableOf(Box::new(strip_inferred_uncertainty(inner))),
        _ => typ.clone(),
    }
}

pub fn should_suppress_inferred_value_mismatch(
    semantic_model: &SemanticModel,
    expected_type: &LuaType,
    actual_type: &LuaType,
    actual_expr: &LuaExpr,
) -> bool {
    if semantic_model.get_emmyrc().strict.inferred_type_mismatch
        || !expr_has_inferred_type(semantic_model, actual_expr)
    {
        return false;
    }

    let stripped_type = strip_inferred_uncertainty(actual_type);
    stripped_type != *actual_type
        && semantic_model
            .type_check_detail(expected_type, &stripped_type)
            .is_ok()
}
