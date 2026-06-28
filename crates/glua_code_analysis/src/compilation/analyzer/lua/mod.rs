pub(in crate::compilation::analyzer) mod call;
mod closure;
mod for_range_stat;
mod func_body;
mod metatable;
mod module;
mod stats;

use rustc_hash::FxHashMap;
use std::sync::Arc;

use closure::analyze_closure;
pub use closure::analyze_return_point;
use for_range_stat::analyze_for_range_stat;
pub use for_range_stat::infer_for_range_iter_expr_func;
pub use func_body::LuaReturnPoint;
use glua_parser::{LuaAst, LuaAstNode, LuaExpr};
use metatable::analyze_setmetatable;
use module::analyze_chunk_return;
pub use module::compute_module_semantic_id;
use stats::{
    analyze_assign_stat, analyze_func_stat, analyze_local_func_stat, analyze_local_stat,
    analyze_table_field, flush_pending_dynamic_key_collection_widenings,
};

use log::info;
use std::time::{Duration, Instant};

use crate::{
    Emmyrc, FileId, GmodRealm, InferFailReason, LuaDeclId, LuaMemberKey, LuaMemberOwner,
    compilation::analyzer::{
        AnalysisPipeline,
        lua::call::{analyze_call, build_special_call_direct_matcher},
    },
    db_index::{DbIndex, LuaType},
    profile::Profile,
    semantic::infer_expr,
};

use super::AnalyzeContext;

pub struct LuaAnalysisPipeline;

impl AnalysisPipeline for LuaAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        let _p = Profile::cond_new("lua analyze", context.tree_list.len() > 1);
        let tree_list = context.tree_list.clone();

        let file_ids = tree_list.iter().map(|x| x.file_id).collect::<Vec<_>>();

        let tree_map = tree_list
            .iter()
            .map(|x| (x.file_id, x.value.clone()))
            .collect::<FxHashMap<_, _>>();

        let special_call_direct_matcher = build_special_call_direct_matcher(db, &tree_map);

        // Pre-compute scripted class scope for all files (compile glob patterns once)
        let gmod_enabled = db.get_emmyrc().gmod.enabled;
        let scripted_scope_files = if gmod_enabled {
            context.get_or_compute_scripted_scope_files(db)
        } else {
            Arc::new(std::collections::HashSet::new())
        };

        let file_dependency = db.get_file_dependencies_index().get_file_dependencies();
        let order = file_dependency.get_best_analysis_order(&file_ids, &context.metas);
        let stderr_profile_enabled = std::env::var_os("GLUALS_PROFILE").is_some();
        let slow_log_enabled = log::log_enabled!(log::Level::Info) || stderr_profile_enabled;
        let node_profile_enabled = stderr_profile_enabled;
        let total_start = slow_log_enabled.then(Instant::now);
        let mut workspace_profile = node_profile_enabled.then(LuaAnalyzeProfile::default);
        let mut slow_file_summary = slow_log_enabled.then(SlowLuaAnalyzeSummary::default);
        let mut file_count: usize = 0;
        for file_id in order {
            if let Some(root) = tree_map.get(&file_id) {
                let file_start = slow_log_enabled.then(Instant::now);
                let is_scripted = scripted_scope_files.contains(&file_id);
                let mut analyzer = LuaAnalyzer::new(
                    db,
                    file_id,
                    context,
                    gmod_enabled,
                    is_scripted,
                    &special_call_direct_matcher,
                );
                let mut profile = node_profile_enabled.then(LuaAnalyzeProfile::default);
                for node in root.descendants::<LuaAst>() {
                    if let Some(profile) = profile.as_mut() {
                        let kind = lua_ast_profile_kind(&node);
                        let node_start = Instant::now();
                        analyze_node(&mut analyzer, node);
                        profile.record(kind, node_start.elapsed());
                    } else {
                        analyze_node(&mut analyzer, node);
                    }
                }
                if let (Some(workspace_profile), Some(profile)) =
                    (workspace_profile.as_mut(), profile.as_ref())
                {
                    workspace_profile.merge(profile);
                }
                analyze_chunk_return(&mut analyzer, root.clone());
                flush_pending_dynamic_key_collection_widenings(&mut analyzer);
                file_count += 1;
                if let Some(file_start) = file_start {
                    let file_elapsed = file_start.elapsed();
                    if let Some(summary) = slow_file_summary.as_mut() {
                        summary.record(file_id, file_elapsed);
                    }

                    // Detailed per-file logging is intentionally reserved for explicit profiling.
                    // Info logging can be enabled in normal server sessions, and logging every
                    // >1ms file turns large workspace analysis into a log-I/O hotspot.
                    let should_log_file = if stderr_profile_enabled {
                        file_elapsed.as_millis() > 1
                    } else {
                        file_elapsed >= Duration::from_millis(50)
                    };
                    if should_log_file {
                        let path = db
                            .get_vfs()
                            .get_uri(&file_id)
                            .map(|u| u.to_string())
                            .unwrap_or_else(|| format!("{:?}", file_id));
                        info!("lua analyze slow file: {} cost {:?}", path, file_elapsed);
                        if let Some(profile) = profile.as_ref() {
                            profile.log_slow_file(&path);
                        }
                        if stderr_profile_enabled {
                            eprintln!("lua analyze slow file: {} cost {:?}", path, file_elapsed);
                            if let Some(profile) = profile.as_ref() {
                                eprintln!(
                                    "lua analyze slow file node profile: {} [{}]",
                                    path,
                                    profile.summary(8)
                                );
                            }
                        }
                    }
                }
            }
        }
        if let Some(total_start) = total_start {
            let total_elapsed = total_start.elapsed();
            info!(
                "lua analyze total: {} files in {:?}",
                file_count, total_elapsed
            );
            if let Some(workspace_profile) = workspace_profile.as_ref() {
                workspace_profile.log_workspace();
            }
            if let Some(slow_file_summary) = slow_file_summary.as_ref() {
                slow_file_summary.log(db);
            }
            if stderr_profile_enabled {
                eprintln!(
                    "lua analyze total: {} files in {:?}",
                    file_count, total_elapsed
                );
                if let Some(workspace_profile) = workspace_profile.as_ref() {
                    eprintln!(
                        "lua analyze workspace node profile: [{}]",
                        workspace_profile.summary(8)
                    );
                }
            }
        }
    }
}

#[derive(Default)]
struct SlowLuaAnalyzeSummary {
    files_over_1ms: usize,
    worst_file: Option<(FileId, Duration)>,
}

impl SlowLuaAnalyzeSummary {
    fn record(&mut self, file_id: FileId, elapsed: Duration) {
        if elapsed.as_millis() <= 1 {
            return;
        }

        self.files_over_1ms += 1;
        if self
            .worst_file
            .as_ref()
            .is_none_or(|(_, worst_elapsed)| elapsed > *worst_elapsed)
        {
            self.worst_file = Some((file_id, elapsed));
        }
    }

    fn log(&self, db: &DbIndex) {
        let Some((file_id, elapsed)) = self.worst_file else {
            return;
        };

        let path = db
            .get_vfs()
            .get_uri(&file_id)
            .map(|u| u.to_string())
            .unwrap_or_else(|| format!("{:?}", file_id));
        info!(
            "lua analyze slow file summary: {} files over 1ms, worst {} cost {:?}",
            self.files_over_1ms, path, elapsed
        );
    }
}

#[derive(Default)]
struct LuaAnalyzeProfile {
    node_stats: FxHashMap<&'static str, (usize, Duration)>,
}

impl LuaAnalyzeProfile {
    fn record(&mut self, kind: &'static str, elapsed: Duration) {
        let (count, total) = self.node_stats.entry(kind).or_default();
        *count += 1;
        *total += elapsed;
    }

    fn log_slow_file(&self, path: &str) {
        let summary = self.summary(8);
        info!("lua analyze slow file node profile: {} [{}]", path, summary);
    }

    fn summary(&self, take: usize) -> String {
        let mut stats = self
            .node_stats
            .iter()
            .map(|(kind, (count, total))| (*kind, *count, *total))
            .collect::<Vec<_>>();
        stats.sort_by_key(|(_, _, total)| std::cmp::Reverse(*total));
        stats
            .into_iter()
            .take(take)
            .map(|(kind, count, total)| format!("{kind}: {count} in {total:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn merge(&mut self, other: &LuaAnalyzeProfile) {
        for (kind, (count, total)) in &other.node_stats {
            let (existing_count, existing_total) = self.node_stats.entry(kind).or_default();
            *existing_count += count;
            *existing_total += *total;
        }
    }

    fn log_workspace(&self) {
        let summary = self.summary(10);
        info!("lua analyze workspace node profile: [{}]", summary);
    }
}

fn lua_ast_profile_kind(node: &LuaAst) -> &'static str {
    match node {
        LuaAst::LuaLocalStat(_) => "local_stat",
        LuaAst::LuaAssignStat(_) => "assign_stat",
        LuaAst::LuaForRangeStat(_) => "for_range_stat",
        LuaAst::LuaFuncStat(_) => "func_stat",
        LuaAst::LuaLocalFuncStat(_) => "local_func_stat",
        LuaAst::LuaTableField(_) => "table_field",
        LuaAst::LuaClosureExpr(_) => "closure_expr",
        LuaAst::LuaCallExpr(call_expr) if call_expr.is_setmetatable() => "setmetatable_call",
        LuaAst::LuaCallExpr(_) => "call_expr",
        _ => "other",
    }
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
    member_assignment_widening_cache:
        FxHashMap<MemberAssignmentWideningCacheKey, MemberAssignmentWideningCache>,
    member_collection_assignment_widening_cache:
        FxHashMap<MemberAssignmentWideningCacheKey, MemberCollectionAssignmentWideningCache>,
    pending_dynamic_key_collection_widenings: FxHashMap<DynamicKeyCollectionWideningKey, LuaType>,
    guarded_table_assignment_type_cache: FxHashMap<MemberAssignmentWideningCacheKey, LuaType>,
    direct_local_table_member_owner_cache: FxHashMap<LuaDeclId, Option<LuaMemberOwner>>,
    literal_index_member_owner_cache: FxHashMap<String, LuaMemberOwner>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MemberAssignmentWideningCacheKey {
    owner: LuaMemberOwner,
    key: LuaMemberKey,
}

#[derive(Debug, Default)]
struct MemberAssignmentWideningCache {
    seen_count: usize,
    by_realm: FxHashMap<GmodRealm, MemberAssignmentWideningState>,
    disabled: bool,
}

#[derive(Debug, Clone)]
struct MemberAssignmentWideningState {
    no_table_literal_widen_type: LuaType,
    table_literal_widen_type: LuaType,
    all_table_assignment_merge_types: bool,
    class_bootstrap_type: Option<LuaType>,
    class_bootstrap_compatible: bool,
}

#[derive(Debug, Default)]
struct MemberCollectionAssignmentWideningCache {
    seen_count: usize,
    by_realm: FxHashMap<GmodRealm, LuaType>,
    disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DynamicKeyCollectionWideningKey {
    owner: LuaMemberOwner,
    key: LuaMemberKey,
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
            member_assignment_widening_cache: FxHashMap::default(),
            member_collection_assignment_widening_cache: FxHashMap::default(),
            pending_dynamic_key_collection_widenings: FxHashMap::default(),
            guarded_table_assignment_type_cache: FxHashMap::default(),
            direct_local_table_member_owner_cache: FxHashMap::default(),
            literal_index_member_owner_cache: FxHashMap::default(),
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
}
