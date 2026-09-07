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
mod inference_trust;
mod local_const_reassign;
mod missing_fields;
mod need_check_nil;
mod param_type_check;
mod property_name_candidates;
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
    BinaryOperator, LuaAssignStat, LuaAst, LuaAstNode, LuaBinaryExpr, LuaBlock, LuaCallExpr,
    LuaCallExprStat, LuaChunk, LuaClosureExpr, LuaComment, LuaDocTag, LuaDocTagAttributeUse,
    LuaDocTagClass, LuaDocTagOther, LuaExpr, LuaIfStat, LuaIndexExpr, LuaLocalStat, LuaNameExpr,
    LuaReturnStat, LuaStat, LuaSyntaxKind, LuaSyntaxNode, LuaTableExpr, LuaTableField,
    UnaryOperator,
};
use lsp_types::{Diagnostic, DiagnosticSeverity, DiagnosticTag, NumberOrString};
use rowan::{TextRange, TextSize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::{
    FileId, LuaSemanticDeclId, LuaType, LuaTypeDeclId, RenderLevel, SemanticDeclLevel, WorkspaceId,
    db_index::DbIndex, humanize_type, semantic::SemanticModel,
};

use super::{
    DiagnosticCode,
    lua_diagnostic_code::{get_default_severity, is_code_default_enable},
    lua_diagnostic_config::LuaDiagnosticConfig,
};

pub use await_in_sync::{PrecomputedAwaitCandidates, precompute_await_candidates};
pub use discard_returns::{PrecomputedNoDiscardCandidates, precompute_nodiscard_candidates};
pub use gmod_network::SortedSendFlowCache;
pub use gmod_network::precompute_sorted_send_flows;
pub use gmod_realm_misuse::GmMethodRealmMap;
pub use gmod_realm_misuse::PrecomputedCalleeRealmMap;
pub use gmod_realm_misuse::PrecomputedRealmCallCandidates;
pub use gmod_realm_misuse::precompute_callee_realm_data_for_workspace;
pub use gmod_realm_misuse::precompute_gm_method_realms;
pub use missing_fields::precompute_missing_required_fields;
pub use param_type_check::{PrecomputedParamTypeCandidates, precompute_param_type_candidates};
pub use property_name_candidates::{
    PrecomputedPropertyNameCandidates, precompute_property_name_candidates,
};

pub type PrecomputedMissingRequiredFields = HashMap<LuaTypeDeclId, Arc<HashSet<String>>>;
pub type AssignmentPrefixKey = (TextSize, TextSize, String);
pub type AssignmentPrefixEvents = HashMap<AssignmentPrefixKey, Vec<AssignmentPrefixEvent>>;

#[derive(Debug, Clone, Copy)]
pub struct AssignmentPrefixEvent {
    pub offset: TextSize,
    pub is_table_literal: bool,
}

/// Per-file syntax nodes bucketed by kind in a single preorder walk.
///
/// Every `descendants::<T>()` filter over the file root visits the whole
/// tree; with ~45 such walks per file that dominates diagnostics CPU.
/// `build_file_node_buckets` performs exactly one `descendants::<LuaAst>()`
/// walk and fans each node out to its typed buckets, all in source order, so
/// checkers iterate slices instead of re-walking.
///
/// Fidelity: each bucket holds exactly what the corresponding
/// `descendants::<T>()` walk would yield, because both go through the same
/// `T::cast` on the same nodes in the same preorder. Two node classes are
/// invisible to `LuaAst::cast` and therefore absent here: `EmptyStat` nodes
/// (no `LuaAst` variant) and `DocTagVisibility` nodes (no `LuaAst` variant).
/// Both fall into `_ => {}` arms in the only checkers walking those kinds
/// (`UnbalancedAssignments`, `DuplicateType`), so no diagnostic changes.
#[derive(Debug, Default)]
pub struct FileNodeBuckets {
    pub all_nodes: Vec<LuaAst>,
    pub call_exprs: Vec<LuaCallExpr>,
    pub assign_stats: Vec<LuaAssignStat>,
    pub local_stats: Vec<LuaLocalStat>,
    pub closure_exprs: Vec<LuaClosureExpr>,
    pub name_exprs: Vec<LuaNameExpr>,
    pub if_stats: Vec<LuaIfStat>,
    pub table_exprs: Vec<LuaTableExpr>,
    pub table_fields: Vec<LuaTableField>,
    pub binary_exprs: Vec<LuaBinaryExpr>,
    pub blocks: Vec<LuaBlock>,
    pub call_expr_stats: Vec<LuaCallExprStat>,
    pub doc_tag_classes: Vec<LuaDocTagClass>,
    pub statements: Vec<LuaStat>,
    pub exprs: Vec<LuaExpr>,
    pub doc_tags: Vec<LuaDocTag>,
    pub doc_tag_others: Vec<LuaDocTagOther>,
    pub doc_tag_attribute_uses: Vec<LuaDocTagAttributeUse>,
}

pub fn build_file_node_buckets(root: &LuaChunk) -> FileNodeBuckets {
    let mut buckets = FileNodeBuckets::default();
    for node in root.descendants::<LuaAst>() {
        push_file_node(&mut buckets, &node);
        buckets.all_nodes.push(node);
    }
    buckets
}

fn push_file_node(buckets: &mut FileNodeBuckets, node: &LuaAst) {
    match node {
        LuaAst::LuaCallExpr(call_expr) => {
            buckets.call_exprs.push(call_expr.clone());
            buckets.exprs.push(LuaExpr::CallExpr(call_expr.clone()));
        }
        LuaAst::LuaTableExpr(table_expr) => {
            buckets.table_exprs.push(table_expr.clone());
            buckets.exprs.push(LuaExpr::TableExpr(table_expr.clone()));
        }
        LuaAst::LuaLiteralExpr(literal_expr) => {
            buckets
                .exprs
                .push(LuaExpr::LiteralExpr(literal_expr.clone()));
        }
        LuaAst::LuaBinaryExpr(binary_expr) => {
            buckets.binary_exprs.push(binary_expr.clone());
            buckets.exprs.push(LuaExpr::BinaryExpr(binary_expr.clone()));
        }
        LuaAst::LuaUnaryExpr(unary_expr) => {
            buckets.exprs.push(LuaExpr::UnaryExpr(unary_expr.clone()));
        }
        LuaAst::LuaClosureExpr(closure_expr) => {
            buckets.closure_exprs.push(closure_expr.clone());
            buckets
                .exprs
                .push(LuaExpr::ClosureExpr(closure_expr.clone()));
        }
        LuaAst::LuaParenExpr(paren_expr) => {
            buckets.exprs.push(LuaExpr::ParenExpr(paren_expr.clone()));
        }
        LuaAst::LuaNameExpr(name_expr) => {
            buckets.name_exprs.push(name_expr.clone());
            buckets.exprs.push(LuaExpr::NameExpr(name_expr.clone()));
        }
        LuaAst::LuaIndexExpr(index_expr) => {
            buckets.exprs.push(LuaExpr::IndexExpr(index_expr.clone()));
        }
        LuaAst::LuaLocalStat(local_stat) => {
            buckets.local_stats.push(local_stat.clone());
            buckets
                .statements
                .push(LuaStat::LocalStat(local_stat.clone()));
        }
        LuaAst::LuaAssignStat(assign_stat) => {
            buckets.assign_stats.push(assign_stat.clone());
            buckets
                .statements
                .push(LuaStat::AssignStat(assign_stat.clone()));
        }
        LuaAst::LuaCallExprStat(call_stat) => {
            buckets.call_expr_stats.push(call_stat.clone());
            buckets
                .statements
                .push(LuaStat::CallExprStat(call_stat.clone()));
        }
        LuaAst::LuaFuncStat(func_stat) => {
            buckets
                .statements
                .push(LuaStat::FuncStat(func_stat.clone()));
        }
        LuaAst::LuaLocalFuncStat(local_func_stat) => {
            buckets
                .statements
                .push(LuaStat::LocalFuncStat(local_func_stat.clone()));
        }
        LuaAst::LuaIfStat(if_stat) => {
            buckets.if_stats.push(if_stat.clone());
            buckets.statements.push(LuaStat::IfStat(if_stat.clone()));
        }
        LuaAst::LuaWhileStat(while_stat) => {
            buckets
                .statements
                .push(LuaStat::WhileStat(while_stat.clone()));
        }
        LuaAst::LuaDoStat(do_stat) => {
            buckets.statements.push(LuaStat::DoStat(do_stat.clone()));
        }
        LuaAst::LuaForStat(for_stat) => {
            buckets.statements.push(LuaStat::ForStat(for_stat.clone()));
        }
        LuaAst::LuaForRangeStat(for_range_stat) => {
            buckets
                .statements
                .push(LuaStat::ForRangeStat(for_range_stat.clone()));
        }
        LuaAst::LuaRepeatStat(repeat_stat) => {
            buckets
                .statements
                .push(LuaStat::RepeatStat(repeat_stat.clone()));
        }
        LuaAst::LuaBreakStat(break_stat) => {
            buckets
                .statements
                .push(LuaStat::BreakStat(break_stat.clone()));
        }
        LuaAst::LuaReturnStat(return_stat) => {
            buckets
                .statements
                .push(LuaStat::ReturnStat(return_stat.clone()));
        }
        LuaAst::LuaGotoStat(goto_stat) => {
            buckets
                .statements
                .push(LuaStat::GotoStat(goto_stat.clone()));
        }
        LuaAst::LuaLabelStat(label_stat) => {
            buckets
                .statements
                .push(LuaStat::LabelStat(label_stat.clone()));
        }
        LuaAst::LuaGlobalStat(global_stat) => {
            buckets
                .statements
                .push(LuaStat::GlobalStat(global_stat.clone()));
        }
        LuaAst::LuaBlock(block) => {
            buckets.blocks.push(block.clone());
        }
        LuaAst::LuaTableField(field) => {
            buckets.table_fields.push(field.clone());
        }
        LuaAst::LuaDocTagClass(class_tag) => {
            buckets.doc_tag_classes.push(class_tag.clone());
            buckets.doc_tags.push(LuaDocTag::Class(class_tag.clone()));
        }
        LuaAst::LuaDocTagOther(other_tag) => {
            buckets.doc_tag_others.push(other_tag.clone());
            buckets.doc_tags.push(LuaDocTag::Other(other_tag.clone()));
        }
        LuaAst::LuaDocTagAttributeUse(attribute_use) => {
            buckets.doc_tag_attribute_uses.push(attribute_use.clone());
            buckets
                .doc_tags
                .push(LuaDocTag::AttributeUse(attribute_use.clone()));
        }
        LuaAst::LuaDocTagEnum(_)
        | LuaAst::LuaDocTagAlias(_)
        | LuaAst::LuaDocTagAttribute(_)
        | LuaAst::LuaDocTagType(_)
        | LuaAst::LuaDocTagParam(_)
        | LuaAst::LuaDocTagReturn(_)
        | LuaAst::LuaDocTagOverload(_)
        | LuaAst::LuaDocTagField(_)
        | LuaAst::LuaDocTagModule(_)
        | LuaAst::LuaDocTagSee(_)
        | LuaAst::LuaDocTagDiagnostic(_)
        | LuaAst::LuaDocTagDeprecated(_)
        | LuaAst::LuaDocTagVersion(_)
        | LuaAst::LuaDocTagCast(_)
        | LuaAst::LuaDocTagSource(_)
        | LuaAst::LuaDocTagSchema(_)
        | LuaAst::LuaDocTagNamespace(_)
        | LuaAst::LuaDocTagUsing(_)
        | LuaAst::LuaDocTagMeta(_)
        | LuaAst::LuaDocTagRealm(_)
        | LuaAst::LuaDocTagNodiscard(_)
        | LuaAst::LuaDocTagReadonly(_)
        | LuaAst::LuaDocTagOperator(_)
        | LuaAst::LuaDocTagGeneric(_)
        | LuaAst::LuaDocTagAsync(_)
        | LuaAst::LuaDocTagAs(_)
        | LuaAst::LuaDocTagReturnCast(_)
        | LuaAst::LuaDocTagExport(_)
        | LuaAst::LuaDocTagLanguage(_)
        | LuaAst::LuaDocTagFileparam(_)
        | LuaAst::LuaDocTagOutparam(_) => {
            // Delegate to `LuaDocTag::cast` so this bucket mirrors
            // `descendants::<LuaDocTag>()` exactly.
            if let Some(tag) = LuaDocTag::cast(node.syntax().clone()) {
                buckets.doc_tags.push(tag);
            }
        }
        _ => {}
    }
}

pub trait Checker {
    const CODES: &[DiagnosticCode];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel);
}

/// The path a checker is running against, for log lines that would otherwise
/// carry only a `FileId`.
fn checker_file_label(context: &DiagnosticContext, semantic_model: &SemanticModel) -> String {
    let file_id = context.get_file_id();
    semantic_model
        .get_db()
        .get_vfs()
        .get_file_path(&file_id)
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| format!("{file_id:?}"))
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
        // Named on entry: `checker slow` only reports on completion.
        if log::log_enabled!(log::Level::Trace) {
            log::trace!(
                "checker start: {} for {}",
                std::any::type_name::<T>(),
                checker_file_label(context, semantic_model)
            );
        }

        if !log::log_enabled!(log::Level::Info) {
            T::check(context, semantic_model);
            return;
        }

        let start = std::time::Instant::now();
        T::check(context, semantic_model);
        let elapsed = start.elapsed();
        if elapsed.as_millis() > 5 {
            let name = T::CODES
                .iter()
                .map(|c| c.get_name())
                .collect::<Vec<_>>()
                .join(",");
            let path = checker_file_label(context, semantic_model);
            log::info!(
                "checker slow: {}({}) cost {:?} for {}",
                std::any::type_name::<T>(),
                name,
                elapsed,
                path
            );
        }
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
    run_check::<call_non_callable::CallNonCallableChecker>(context, semantic_model, cancel_token);
    run_check::<inference_trust::InferenceTrustChecker>(context, semantic_model, cancel_token);
    run_check::<missing_fields::MissingFieldsChecker>(context, semantic_model, cancel_token);
    run_check::<param_type_check::ParamTypeCheckChecker>(context, semantic_model, cancel_token);
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
    // These checkers do broad expression/declaration lookups. Running them after
    // ParamTypeCheck/CheckField lets them reuse warmed per-file inference caches.
    run_check::<need_check_nil::NeedCheckNilChecker>(context, semantic_model, cancel_token);
    run_check::<discard_returns::DiscardReturnsChecker>(context, semantic_model, cancel_token);
    // AwaitInSync relies heavily on expression/call inference. Running it after
    // the broad type/field checkers lets it reuse their warmed per-file cache.
    run_check::<await_in_sync::AwaitInSyncChecker>(context, semantic_model, cancel_token);
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
    // DeprecatedChecker runs after heavy inference checkers (CheckFieldChecker,
    // ParamTypeCheckChecker, etc.) so find_decl() calls benefit from cached results.
    run_check::<deprecated::DeprecatedChecker>(context, semantic_model, cancel_token);
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

/// Precomputed data shared across all diagnostic files in a batch run.
/// Computing this once instead of per-file saves ~60s on large workspaces.
pub struct SharedDiagnosticData {
    /// Main-workspace file IDs sorted by stable FileId order.
    pub workspace_file_ids: Arc<Vec<FileId>>,
    /// Maps workspace_id -> precomputed GM method realm annotations.
    /// GmodRealmMisuseChecker uses this to avoid re-scanning all annotations per file.
    pub gm_method_realms: HashMap<WorkspaceId, Arc<GmMethodRealmMap>>,
    /// Maps workspace_id -> immutable, precomputed callee-declaration realm data.
    pub callee_realms_by_workspace: HashMap<WorkspaceId, Arc<PrecomputedCalleeRealmMap>>,
    /// Maps workspace_id -> static callee names that may produce realm diagnostics.
    pub realm_call_candidates_by_workspace:
        HashMap<WorkspaceId, Arc<PrecomputedRealmCallCandidates>>,
    /// Required non-method fields for each type declaration, including inherited fields.
    pub missing_required_fields: Arc<PrecomputedMissingRequiredFields>,
    /// Whether any indexed signature/type cache can produce an async callable.
    pub await_candidates: Arc<PrecomputedAwaitCandidates>,
    /// Static callee names whose callable metadata has parameter types worth checking.
    pub param_type_candidates: Arc<PrecomputedParamTypeCandidates>,
    /// Static callee names whose signatures are marked @nodiscard.
    pub nodiscard_candidates: Arc<PrecomputedNoDiscardCandidates>,
    /// Names the deprecated, readonly and visibility checkers could report on.
    pub property_name_candidates: Arc<PrecomputedPropertyNameCandidates>,
    /// Precomputed sorted send flows by message name. Built once per batch
    /// run instead of per-file, avoiding repeated VFS path lookups and sorts.
    pub sorted_send_flows: Arc<SortedSendFlowCache>,
}

pub struct DiagnosticContext<'a> {
    file_id: FileId,
    db: &'a DbIndex,
    diagnostics: Vec<Diagnostic>,
    pub config: Arc<LuaDiagnosticConfig>,
    cancel_token: CancellationToken,
    shared_data: Option<Arc<SharedDiagnosticData>>,
    assignment_prefix_events: Option<Arc<AssignmentPrefixEvents>>,
    file_buckets: Option<Arc<FileNodeBuckets>>,
    enable_cache: RefCell<HashMap<DiagnosticCode, bool>>,
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
            shared_data: None,
            assignment_prefix_events: None,
            file_buckets: None,
            enable_cache: RefCell::new(HashMap::new()),
        }
    }

    pub fn new_with_shared(
        file_id: FileId,
        db: &'a DbIndex,
        config: Arc<LuaDiagnosticConfig>,
        cancel_token: CancellationToken,
        shared_data: Arc<SharedDiagnosticData>,
    ) -> Self {
        Self {
            file_id,
            db,
            diagnostics: Vec::new(),
            config,
            cancel_token,
            shared_data: Some(shared_data),
            assignment_prefix_events: None,
            file_buckets: None,
            enable_cache: RefCell::new(HashMap::new()),
        }
    }

    pub fn get_shared_data_arc(&self) -> Option<Arc<SharedDiagnosticData>> {
        self.shared_data.clone()
    }

    pub fn get_db(&self) -> &DbIndex {
        self.db
    }

    pub fn get_file_id(&self) -> FileId {
        self.file_id
    }

    pub fn get_assignment_prefix_events(&mut self, root: &LuaChunk) -> Arc<AssignmentPrefixEvents> {
        if let Some(events) = &self.assignment_prefix_events {
            return events.clone();
        }

        let events = if let Some(buckets) = self.file_buckets() {
            collect_assignment_prefix_events_from_assigns(&buckets.assign_stats)
        } else {
            collect_assignment_prefix_events(root)
        };
        let events = Arc::new(events);
        self.assignment_prefix_events = Some(events.clone());
        events
    }

    /// Per-file syntax nodes bucketed by kind, built with a single preorder
    /// walk on first use and shared by all checkers. Returns `None` only when
    /// the file has no syntax tree, in which case diagnostics cannot run.
    // Rowan syntax nodes are `!Send`/`!Sync`, but this `Arc` never crosses
    // threads: buckets are built and iterated synchronously within one file's
    // check pass, bridging only the `&mut` borrow between checkers.
    #[expect(clippy::arc_with_non_send_sync)]
    pub fn file_buckets(&mut self) -> Option<Arc<FileNodeBuckets>> {
        if let Some(buckets) = &self.file_buckets {
            return Some(buckets.clone());
        }

        let tree = self.db.get_vfs().get_syntax_tree(&self.file_id)?;
        let buckets = Arc::new(build_file_node_buckets(&tree.get_chunk_node()));
        self.file_buckets = Some(buckets.clone());
        Some(buckets)
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
        // Evaluated once per emitted diagnostic plus once per checker dispatch,
        // so hundreds of repeated workspace-set/index lookups per file. The
        // inputs (config, per-file diagnostic index, module index) are fixed
        // for the lifetime of this context, so memoize per code.
        if let Some(cached) = self.enable_cache.borrow().get(code) {
            return *cached;
        }

        let enabled = self.is_checker_enable_by_code_uncached(code);
        self.enable_cache.borrow_mut().insert(*code, enabled);
        enabled
    }

    fn is_checker_enable_by_code_uncached(&self, code: &DiagnosticCode) -> bool {
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

fn collect_assignment_prefix_events(root: &LuaChunk) -> AssignmentPrefixEvents {
    let assigns: Vec<LuaAssignStat> = root.descendants().collect();
    collect_assignment_prefix_events_from_assigns(&assigns)
}

fn collect_assignment_prefix_events_from_assigns(
    assigns: &[LuaAssignStat],
) -> AssignmentPrefixEvents {
    let mut events: AssignmentPrefixEvents = HashMap::default();
    for assign_stat in assigns {
        let Some((block_start, block_end)) = assignment_block_range(assign_stat.syntax()) else {
            continue;
        };

        let offset = assign_stat.syntax().text_range().start();
        let (vars, exprs) = assign_stat.get_var_and_expr_list();
        for (idx, var) in vars.into_iter().enumerate() {
            let prefix_text = normalized_syntax_text(var.syntax());
            if prefix_text.is_empty() {
                continue;
            }

            let is_table_literal = exprs
                .get(idx)
                .is_some_and(|expr| assignment_guarantees_table(var.syntax(), expr));
            if is_table_literal
                && let Some((outer_start, outer_end, after)) =
                    absence_guard_seed_scope(assign_stat.syntax(), &prefix_text)
            {
                // `if not t.k then t.k = {} end` runs exactly when `t.k` is
                // absent, so `t.k` is a table on every path out of the `if`.
                // The event otherwise stays keyed to the branch's own block and
                // the statements after the `if` never see it, so writes through
                // `t.k` there are checked as if it had never been seeded.
                events
                    .entry((outer_start, outer_end, prefix_text.clone()))
                    .or_default()
                    .push(AssignmentPrefixEvent {
                        offset: after,
                        is_table_literal,
                    });
            }
            events
                .entry((block_start, block_end, prefix_text))
                .or_default()
                .push(AssignmentPrefixEvent {
                    offset,
                    is_table_literal,
                });
        }
    }

    for entries in events.values_mut() {
        entries.sort_by_key(|event| event.offset);
    }

    events
}

/// The block an `if not <prefix> then <prefix> = {} end` seed reaches, and the
/// offset it is in force from.
///
/// Only an absence guard qualifies: its branch runs exactly when the target is
/// missing, so the target is a table afterwards whichever way the test went. A
/// plain `if cond then t.k = {} end` guarantees nothing after the `if`.
fn absence_guard_seed_scope(
    assign_syntax: &LuaSyntaxNode,
    prefix_text: &str,
) -> Option<(TextSize, TextSize, TextSize)> {
    let branch_block = assign_syntax.parent()?;
    let if_stat = LuaIfStat::cast(branch_block.parent()?)?;
    // Only the `then` block: an `else` runs when the target is present.
    if if_stat.get_block()?.syntax() != &branch_block {
        return None;
    }
    if !condition_tests_absence(&if_stat.get_condition_expr()?, prefix_text) {
        return None;
    }

    let (outer_start, outer_end) = assignment_block_range(if_stat.syntax())?;
    Some((outer_start, outer_end, if_stat.syntax().text_range().end()))
}

fn condition_tests_absence(condition: &LuaExpr, prefix_text: &str) -> bool {
    match condition {
        LuaExpr::ParenExpr(paren) => paren
            .get_expr()
            .is_some_and(|inner| condition_tests_absence(&inner, prefix_text)),
        LuaExpr::UnaryExpr(unary) => {
            unary.get_op_token().map(|op| op.get_op()) == Some(UnaryOperator::OpNot)
                && unary
                    .get_expr()
                    .is_some_and(|inner| syntax_text_eq_normalized(inner.syntax(), prefix_text))
        }
        LuaExpr::BinaryExpr(binary) => {
            binary.get_op_token().map(|op| op.get_op()) == Some(BinaryOperator::OpEq)
                && binary.get_exprs().is_some_and(|(left, right)| {
                    (matches!(right, LuaExpr::LiteralExpr(_))
                        && syntax_text_eq_normalized(right.syntax(), "nil")
                        && syntax_text_eq_normalized(left.syntax(), prefix_text))
                        || (matches!(left, LuaExpr::LiteralExpr(_))
                            && syntax_text_eq_normalized(left.syntax(), "nil")
                            && syntax_text_eq_normalized(right.syntax(), prefix_text))
                })
        }
        _ => false,
    }
}

fn assignment_guarantees_table(var: &LuaSyntaxNode, expr: &LuaExpr) -> bool {
    if matches!(expr, LuaExpr::TableExpr(_)) {
        return true;
    }

    let LuaExpr::BinaryExpr(binary) = expr else {
        return false;
    };
    if binary.get_op_token().map(|token| token.get_op()) != Some(BinaryOperator::OpOr) {
        return false;
    }
    let Some((left, right)) = binary.get_exprs() else {
        return false;
    };

    matches!(right, LuaExpr::TableExpr(_))
        && syntax_text_eq_normalized(left.syntax(), &normalized_syntax_text(var))
}

pub fn is_initialized_assignment_prefix(
    index_expr: &LuaIndexExpr,
    assign_stat: &LuaAssignStat,
    assignment_prefixes: &AssignmentPrefixEvents,
) -> bool {
    let Some(prefix) = index_expr.get_prefix_expr() else {
        return false;
    };

    let prefix_text = normalized_syntax_text(prefix.syntax());
    if prefix_text.is_empty() {
        return false;
    }

    let current_offset = assign_stat.syntax().text_range().start();
    // A seed in an enclosing block still reaches here: `t.k = {}` before an
    // `if` initialises `t.k` for the writes inside it just as much as for the
    // ones after it. Only a closure breaks the chain, since its body runs
    // somewhere else entirely.
    //
    // The key reuses one `String` across blocks instead of cloning the prefix
    // per enclosing block.
    let mut key: AssignmentPrefixKey = (TextSize::from(0), TextSize::from(0), prefix_text);
    for (block_start, block_end) in enclosing_assignment_block_ranges(assign_stat.syntax()) {
        key.0 = block_start;
        key.1 = block_end;
        let Some(events) = assignment_prefixes.get(&key) else {
            continue;
        };
        let last_event_idx = events.partition_point(|event| event.offset < current_offset);
        if last_event_idx > 0 {
            return events[last_event_idx - 1].is_table_literal;
        }
    }

    false
}

/// Every block that encloses `node` within its own function, innermost first.
fn enclosing_assignment_block_ranges(node: &LuaSyntaxNode) -> Vec<(TextSize, TextSize)> {
    let mut ranges = Vec::new();
    let mut current = node.parent();
    while let Some(block) = current {
        if LuaClosureExpr::can_cast(block.kind().into()) {
            break;
        }
        if LuaSyntaxKind::Block == block.kind().into() {
            let range = block.text_range();
            ranges.push((range.start(), range.end()));
        }
        current = block.parent();
    }
    ranges
}

pub fn assignment_prefix_key_for_syntax(
    stat_syntax: &LuaSyntaxNode,
    syntax: &LuaSyntaxNode,
) -> Option<AssignmentPrefixKey> {
    let (block_start, block_end) = assignment_block_range(stat_syntax)?;

    let access_text = normalized_syntax_text(syntax);
    if access_text.is_empty() {
        return None;
    }

    Some((block_start, block_end, access_text))
}

pub fn is_initialized_assignment_access(
    index_expr: &LuaIndexExpr,
    assignment_prefixes: &AssignmentPrefixEvents,
) -> bool {
    let Some(stat) = index_expr.syntax().ancestors().find_map(LuaStat::cast) else {
        return false;
    };

    let Some(key) = assignment_prefix_key_for_syntax(stat.syntax(), index_expr.syntax()) else {
        return false;
    };

    let Some(events) = assignment_prefixes.get(&key) else {
        return false;
    };

    let current_offset = stat.syntax().text_range().start();
    let last_event_idx = events.partition_point(|event| event.offset < current_offset);
    last_event_idx > 0 && events[last_event_idx - 1].is_table_literal
}

fn assignment_block_range(node: &LuaSyntaxNode) -> Option<(TextSize, TextSize)> {
    let range = node.parent()?.text_range();
    Some((range.start(), range.end()))
}

fn normalized_syntax_text(syntax: &LuaSyntaxNode) -> String {
    syntax
        .text()
        .to_string()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// Whitespace-insensitive comparison without materialising the syntax text.
/// Hot query paths (`condition_tests_absence`, `assignment_guarantees_table`)
/// compare single nodes against short literals or an already-normalized
/// prefix; building an intermediate `String` per query showed up per file.
fn syntax_text_eq_normalized(syntax: &LuaSyntaxNode, expected: &str) -> bool {
    let mut expected_chars = expected.chars().filter(|c| !c.is_whitespace());
    for element in syntax.descendants_with_tokens() {
        let Some(token) = element.into_token() else {
            continue;
        };
        for c in token.text().chars() {
            if c.is_whitespace() {
                continue;
            }
            if expected_chars.next() != Some(c) {
                return false;
            }
        }
    }
    expected_chars.next().is_none()
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

/// The certain part of an inferred type, with uncertain positions widened to
/// `any` so a mismatch probe ignores them.
///
/// The `any` here is a throwaway: the result feeds one `type_check_detail`
/// comparison and is never stored, returned or cached. It must stay that way —
/// this is not an inference site, and treating it as one would put a
/// failure-shaped `any` back into the type lattice.
fn strip_inferred_uncertainty(typ: &LuaType) -> LuaType {
    match typ {
        LuaType::Union(union) => {
            let stripped = union
                .into_vec()
                .into_iter()
                .filter_map(|member| match member {
                    LuaType::Nil
                    | LuaType::Never
                    | LuaType::Unknown
                    | LuaType::SelfInfer
                    | LuaType::BooleanConst(false)
                    | LuaType::DocBooleanConst(false) => None,
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
                    LuaType::Nil
                    | LuaType::Never
                    | LuaType::Unknown
                    | LuaType::SelfInfer
                    | LuaType::BooleanConst(false)
                    | LuaType::DocBooleanConst(false) => None,
                    other => Some(strip_inferred_uncertainty(other)),
                })
                .collect::<Vec<_>>();
            if stripped.is_empty() {
                LuaType::Any
            } else {
                LuaType::from_vec(stripped)
            }
        }
        LuaType::Nil
        | LuaType::Never
        | LuaType::Unknown
        | LuaType::SelfInfer
        | LuaType::BooleanConst(false)
        | LuaType::DocBooleanConst(false) => LuaType::Any,
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
