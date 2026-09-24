pub(in crate::compilation::analyzer) mod call;
mod closure;
mod for_range_stat;
pub(in crate::compilation::analyzer) mod func_body;
mod member_write_policy;
mod metatable;
mod module;
mod stats;

use rustc_hash::FxHashMap;

use closure::analyze_closure;
pub use closure::{analyze_return_point, derive_inferred_return_with_reads};
use for_range_stat::analyze_for_range_stat;
pub use for_range_stat::{
    IterVarInference, infer_for_range_iter_expr_func, iterates_table_member_map,
};
pub use func_body::LuaReturnPoint;
use glua_parser::{LuaAst, LuaAstNode, LuaChunk, LuaExpr, LuaSyntaxId, with_syntax_resolver};
pub(in crate::compilation::analyzer) use member_write_policy::alias_target_global_path;
pub(in crate::compilation::analyzer) use member_write_policy::resolve_index_expr_member_owner_for_file;
use member_write_policy::{
    DynamicKeyCollectionWideningKey, MemberAssignmentWideningCacheKey, MemberWideningCache,
};
use metatable::analyze_setmetatable;
use module::analyze_chunk_return;
pub use module::compute_module_semantic_id;
pub use stats::is_member_assignment_in_conditional_branch;
use stats::{
    GuardedSlotMemo, analyze_assign_stat, analyze_func_stat, analyze_local_func_stat,
    analyze_local_stat, analyze_table_field, flush_pending_dynamic_key_collection_widenings,
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
    Emmyrc, FileId, InferFailReason, LuaDeclId, LuaMemberId, LuaMemberKey, LuaMemberOwner,
    compilation::analyzer::lua::call::analyze_call,
    db_index::{DbIndex, LuaType},
    semantic::infer_expr,
};

use super::AnalyzeContext;

/// Evaluates one walk derivation of `file_id`: the statement or expression at
/// `syntax_id`, or the file's module export when `syntax_id` is the chunk.
pub(in crate::compilation::analyzer) fn evaluate_node(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    special_call_direct_matcher: &call::SpecialCallDirectMatcher,
    file_id: FileId,
    syntax_id: LuaSyntaxId,
) {
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_chunk_node())
    else {
        return;
    };
    let Some(node) = syntax_id.to_node_from_root(root.syntax()) else {
        return;
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
    with_syntax_resolver(root.syntax(), || {
        if let Some(chunk) = LuaChunk::cast(node.clone()) {
            analyze_chunk_return(&mut analyzer, chunk);
        } else if let Some(node) = LuaAst::cast(node) {
            analyze_node(&mut analyzer, node);
        }
        flush_pending_dynamic_key_collection_widenings(&mut analyzer);
    });
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
    db: &'a mut DbIndex,
    context: &'a mut AnalyzeContext,
    gmod_enabled: bool,
    is_scripted_class_scope: bool,
    special_call_direct_matcher: &'a call::SpecialCallDirectMatcher,
    /// Whether the last [`Self::infer_expr`] merged sibling members under a
    /// computed key.
    sibling_merge_read: bool,
    member_collection_assignment_widening_cache:
        FxHashMap<MemberAssignmentWideningCacheKey, MemberWideningCache<LuaType>>,
    pending_dynamic_key_collection_widenings: FxHashMap<DynamicKeyCollectionWideningKey, LuaType>,
    guarded_table_assignment_type_cache: FxHashMap<MemberAssignmentWideningCacheKey, LuaType>,
    /// Whether each member is a guarded `x.y = x.y or {}` self-assignment.
    ///
    /// The answer derives from the member's own statement syntax alone, which
    /// cannot change while the member is indexed (an edit forgets and
    /// recreates it), and a `LuaAnalyzer` lives for one file pass over a
    /// stable VFS — so each member's syntax is walked once per pass instead
    /// of once per writer sharing its slot.
    guarded_table_assignment_member_check_cache: FxHashMap<LuaMemberId, bool>,
    /// Per-`(owner, key)` guarded-bootstrap slot memo for this file pass.
    ///
    /// The canonical bootstrap used to walk the whole slot on every write; N
    /// same-key appends were O(N^2). The memo re-walks only appended entries,
    /// so the pass costs O(N) total for identical answers.
    guarded_table_assignment_slot_cache: FxHashMap<(LuaMemberOwner, LuaMemberKey), GuardedSlotMemo>,
    direct_local_table_member_owner_cache: FxHashMap<LuaDeclId, Option<LuaMemberOwner>>,
    literal_index_member_owner_cache: FxHashMap<smol_str::SmolStr, LuaMemberOwner>,
    /// A closure's own `return` statements (excluding nested closures').
    closure_own_returns_cache:
        FxHashMap<glua_parser::LuaSyntaxId, std::rc::Rc<Vec<glua_parser::LuaReturnStat>>>,
}

impl LuaAnalyzer<'_> {
    pub fn new<'a>(
        db: &'a mut DbIndex,
        file_id: FileId,
        context: &'a mut AnalyzeContext,
        gmod_enabled: bool,
        is_scripted_class_scope: bool,
        special_call_direct_matcher: &'a call::SpecialCallDirectMatcher,
    ) -> LuaAnalyzer<'a> {
        LuaAnalyzer {
            file_id,
            db,
            context,
            gmod_enabled,
            is_scripted_class_scope,
            special_call_direct_matcher,
            sibling_merge_read: false,
            member_collection_assignment_widening_cache: FxHashMap::default(),
            pending_dynamic_key_collection_widenings: FxHashMap::default(),
            guarded_table_assignment_type_cache: FxHashMap::default(),
            guarded_table_assignment_member_check_cache: FxHashMap::default(),
            guarded_table_assignment_slot_cache: FxHashMap::default(),
            direct_local_table_member_owner_cache: FxHashMap::default(),
            literal_index_member_owner_cache: FxHashMap::default(),
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
        let merges_before = cache.sibling_merge_reads;
        let result = infer_expr(self.db, cache, expr.clone());
        self.sibling_merge_read = cache.sibling_merge_reads > merges_before;
        result
    }
}
