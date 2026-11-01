pub(in crate::compilation::analyzer) mod call;
mod closure;
mod for_range_stat;
pub(in crate::compilation::analyzer) mod func_body;
mod member_write_policy;
mod metatable;
mod module;
mod stats;
mod store;

use rustc_hash::FxHashMap;

use closure::analyze_closure;
pub use closure::{analyze_return_point, derive_inferred_return_with_reads};
use for_range_stat::analyze_for_range_stat;
pub use for_range_stat::{
    IterVarInference, infer_for_range_iter_expr_func, iterates_table_member_map,
};
pub use func_body::LuaReturnPoint;
use glua_parser::{LuaAst, LuaAstNode, LuaChunk, LuaExpr, LuaSyntaxId, with_syntax_resolver};
use member_write_policy::DynamicKeyCollectionWideningKey;
pub(in crate::compilation::analyzer) use member_write_policy::alias_target_global_path;
pub(in crate::compilation::analyzer) use member_write_policy::resolve_index_expr_member_owner_for_file;
use metatable::analyze_setmetatable;
use module::analyze_chunk_return;
pub use module::compute_module_semantic_id;
pub use stats::is_member_assignment_in_conditional_branch;
use stats::{
    analyze_assign_stat, analyze_func_stat, analyze_local_func_stat, analyze_local_stat,
    analyze_table_field, flush_pending_dynamic_key_collection_widenings,
};
pub(crate) use stats::{
    dominating_guarded_table_bootstrap_range, is_guarded_table_definition_site,
};
pub(crate) use stats::{expr_fills_own_default, expr_reads_out_of_decl};
pub(in crate::compilation::analyzer) use stats::{
    has_multiple_distinct_index_expr_member_owners, is_guarded_table_assignment_index_expr,
    mark_resolved_member_assignment,
};

use crate::{
    Emmyrc, FileId, InferFailReason, LuaDeclId, LuaMemberId, LuaMemberOwner,
    compilation::analyzer::lua::call::analyze_call,
    db_index::{DbIndex, LuaType},
    semantic::infer_expr,
};

use super::AnalyzeContext;
pub(in crate::compilation::analyzer) use store::{MemberHoming, SignatureChange, StatementWrite};

/// Analyses one walk derivation against `db` without writing it, and returns
/// the writes to apply with [`StatementWrite::apply`], in order. An assignment
/// is analysed in `stage`; its homing stage also returns what its values stage
/// continues from.
pub(in crate::compilation::analyzer) fn compute_node(
    db: &DbIndex,
    stage: AssignmentStage,
    context: &mut AnalyzeContext,
    special_call_direct_matcher: &call::SpecialCallDirectMatcher,
    file_id: FileId,
    syntax_id: LuaSyntaxId,
) -> (Vec<StatementWrite>, AssignmentHoming) {
    analyze_walk_node(
        db,
        stage,
        context,
        special_call_direct_matcher,
        file_id,
        syntax_id,
    )
}

fn analyze_walk_node(
    db: &DbIndex,
    stage: AssignmentStage,
    context: &mut AnalyzeContext,
    special_call_direct_matcher: &call::SpecialCallDirectMatcher,
    file_id: FileId,
    syntax_id: LuaSyntaxId,
) -> (Vec<StatementWrite>, AssignmentHoming) {
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_chunk_node())
    else {
        return Default::default();
    };
    let Some(node) = syntax_id.to_node_from_root(root.syntax()) else {
        return Default::default();
    };
    let gmod_enabled = db.get_emmyrc().gmod.enabled;
    let is_scripted_class_scope = gmod_enabled
        && context
            .get_or_compute_scripted_scope_files(db)
            .contains(&file_id);
    let mut analyzer = LuaAnalyzer::new(
        db,
        file_id,
        context,
        gmod_enabled,
        is_scripted_class_scope,
        special_call_direct_matcher,
    );
    if let AssignmentStage::Values(homing) = &stage {
        analyzer.homed_members = homing.homed.clone();
    }
    analyzer.assignment_stage = stage;
    with_syntax_resolver(root.syntax(), || {
        if let Some(chunk) = LuaChunk::cast(node.clone()) {
            analyze_chunk_return(&mut analyzer, chunk);
        } else if let Some(node) = LuaAst::cast(node) {
            analyze_node(&mut analyzer, node);
        }
        flush_pending_dynamic_key_collection_widenings(&mut analyzer);
    });
    let homing = match analyzer.assignment_stage {
        AssignmentStage::Homing(homing) => homing,
        _ => AssignmentHoming::default(),
    };
    (analyzer.log, homing)
}

/// Which part of an assignment an analysis makes.
///
/// An assignment homes the members it writes before it infers the values it
/// assigns them, and the inference reads those members, so the homing runs as
/// a stage of its own, which applies before the values stage infers.
#[derive(Debug)]
pub(in crate::compilation::analyzer) enum AssignmentStage {
    /// The whole analysis in one go.
    Whole,
    /// Only the homing of the members the values are inferred after.
    Homing(AssignmentHoming),
    /// The rest, after the homing stage applied.
    Values(AssignmentHoming),
}

/// What an assignment's homing stage read before it homed its members, for
/// the values stage to decide from.
#[derive(Debug, Default)]
pub(in crate::compilation::analyzer) struct AssignmentHoming {
    /// Per assigned value: whether the assignment skips it.
    skipped: Vec<bool>,
    /// Per assigned value on the general path: the declared type of the empty
    /// table it assigns, as read before its member was homed.
    declared_empty_table_types: Vec<Option<Option<LuaType>>>,
    /// The members homed.
    homed: rustc_hash::FxHashSet<LuaMemberId>,
}

fn analyze_node(analyzer: &mut LuaAnalyzer, node: LuaAst) {
    match node {
        LuaAst::LuaLocalStat(local_stat) => {
            analyze_local_stat(analyzer, local_stat);
        }
        LuaAst::LuaAssignStat(assign_stat) => {
            analyze_assign_stat(analyzer, assign_stat);
        }
        LuaAst::LuaForRangeStat(for_range_stat) => {
            analyze_for_range_stat(analyzer, for_range_stat);
        }
        LuaAst::LuaFuncStat(func_stat) => {
            analyze_func_stat(analyzer, func_stat);
        }
        LuaAst::LuaLocalFuncStat(local_func_stat) => {
            analyze_local_func_stat(analyzer, local_func_stat);
        }
        LuaAst::LuaTableField(field) => {
            analyze_table_field(analyzer, field);
        }
        LuaAst::LuaClosureExpr(closure) => {
            analyze_closure(analyzer, closure);
        }
        LuaAst::LuaCallExpr(call_expr) => {
            if call_expr.is_setmetatable() {
                analyze_setmetatable(analyzer, call_expr);
            } else {
                analyze_call(analyzer, call_expr);
            }
        }
        _ => {}
    }
}

#[derive(Debug)]
struct LuaAnalyzer<'a> {
    file_id: FileId,
    db: &'a DbIndex,
    /// The writes made through [`LuaAnalyzer::write`], in order.
    log: Vec<StatementWrite>,
    /// The part of an assignment this analysis makes.
    assignment_stage: AssignmentStage,
    /// The members this analysis homed, each homed once.
    homed_members: rustc_hash::FxHashSet<LuaMemberId>,
    context: &'a mut AnalyzeContext,
    gmod_enabled: bool,
    is_scripted_class_scope: bool,
    special_call_direct_matcher: &'a call::SpecialCallDirectMatcher,
    pending_dynamic_key_collection_widenings: FxHashMap<DynamicKeyCollectionWideningKey, LuaType>,
    direct_local_table_member_owner_cache: FxHashMap<LuaDeclId, Option<LuaMemberOwner>>,
    /// A closure's own `return` statements (excluding nested closures').
    closure_own_returns_cache:
        FxHashMap<glua_parser::LuaSyntaxId, std::rc::Rc<Vec<glua_parser::LuaReturnStat>>>,
}

impl LuaAnalyzer<'_> {
    pub fn new<'a>(
        db: &'a DbIndex,
        file_id: FileId,
        context: &'a mut AnalyzeContext,
        gmod_enabled: bool,
        is_scripted_class_scope: bool,
        special_call_direct_matcher: &'a call::SpecialCallDirectMatcher,
    ) -> LuaAnalyzer<'a> {
        LuaAnalyzer {
            file_id,
            db,
            log: Vec::new(),
            assignment_stage: AssignmentStage::Whole,
            homed_members: rustc_hash::FxHashSet::default(),
            context,
            gmod_enabled,
            is_scripted_class_scope,
            special_call_direct_matcher,
            pending_dynamic_key_collection_widenings: FxHashMap::default(),
            direct_local_table_member_owner_cache: FxHashMap::default(),
            closure_own_returns_cache: FxHashMap::default(),
        }
    }

    #[allow(unused)]
    pub fn get_emmyrc(&self) -> &Emmyrc {
        self.db.get_emmyrc()
    }
}

impl LuaAnalyzer<'_> {
    pub fn infer_expr(&mut self, expr: &LuaExpr) -> Result<LuaType, InferFailReason> {
        let cache = self.context.infer_manager.get_infer_cache(self.file_id);
        infer_expr(self.db, cache, expr.clone())
    }

    /// Logs `write`, which applies after the analysis.
    fn write(&mut self, write: StatementWrite) {
        self.log.push(write);
    }
}
