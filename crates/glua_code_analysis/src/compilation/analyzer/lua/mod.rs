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
    analyze_table_field,
};

use log::info;
use std::time::{Duration, Instant};

use crate::{
    Emmyrc, FileId, InferFailReason,
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
        let slow_log_enabled = log::log_enabled!(log::Level::Info);
        let total_start = slow_log_enabled.then(Instant::now);
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
                let mut profile = slow_log_enabled.then(LuaAnalyzeProfile::default);
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
                analyze_chunk_return(&mut analyzer, root.clone());
                file_count += 1;
                if let Some(file_start) = file_start {
                    let file_elapsed = file_start.elapsed();
                    if file_elapsed.as_millis() > 1 {
                        let path = db
                            .get_vfs()
                            .get_uri(&file_id)
                            .map(|u| u.to_string())
                            .unwrap_or_else(|| format!("{:?}", file_id));
                        info!("lua analyze slow file: {} cost {:?}", path, file_elapsed);
                        if let Some(profile) = profile.as_ref() {
                            profile.log_slow_file(&path);
                        }
                        // Log infer_expr sub-type timing for this file
                        let cache = context.infer_manager.get_infer_cache(file_id);
                        let idx_ns = cache.prof_infer_index_time_ns;
                        let call_ns = cache.prof_infer_call_time_ns;
                        let name_ns = cache.prof_infer_name_time_ns;
                        let tbl_ns = cache.prof_infer_table_time_ns;
                        let other_ns = cache.prof_infer_other_time_ns;
                        let idx_calls = cache.prof_infer_index_calls;
                        let call_calls = cache.prof_infer_call_calls;
                        let name_calls = cache.prof_infer_name_calls;
                        let tbl_calls = cache.prof_infer_table_calls;
                        let total_miss = cache.prof_infer_expr_calls - cache.prof_infer_expr_hits;
                        let flow_calls = cache.prof_flow_calls;
                        let flow_hits = cache.prof_flow_hits;
                        let flow_hit_pct = flow_hits
                            .checked_mul(100)
                            .and_then(|h| h.checked_div(flow_calls))
                            .unwrap_or(0);
                        let flow_nodes_walked = cache.prof_flow_nodes_walked;
                        let flow_cache_entries = cache.flow_cache_entry_count();
                        let expr_cache_entries = cache.expr_cache.len();
                        let cache_removals = cache.prof_expr_cache_removals;
                        let unique_inferred = cache.prof_unique_inferred;
                        let recursive_calls = cache.prof_infer_expr_recursive_calls;
                        let max_depth = cache.prof_infer_expr_max_depth;
                        let flow_walk_avg = if cache.prof_flow_calls > 0 {
                            cache.prof_flow_walk_depth_sum / cache.prof_flow_calls as u64
                        } else {
                            0
                        };
                        let flow_walk_max = cache.prof_flow_walk_max_depth;
                        let err_fnf = cache.prof_err_field_not_found;

                        let err_ue = cache.prof_err_unresolve_expr;
                        let err_udt = cache.prof_err_unresolve_decl_type;
                        let err_umt = cache.prof_err_unresolve_member_type;
                        let err_utd = cache.prof_err_unresolve_type_decl;
                        let err_uo = cache.prof_err_unresolve_operator;
                        let err_um = cache.prof_err_unresolve_module;
                        let err_usr = cache.prof_err_unresolve_sig_return;
                        // Log top UnResolveDeclType decl_ids
                        let mut decl_ids: Vec<_> = cache.prof_unresolve_decl_ids.iter().collect();
                        decl_ids.sort_by(|a, b| b.1.cmp(a.1));
                        let top_ids: Vec<String> = decl_ids
                            .iter()
                            .take(10)
                            .map(|(pos, count)| format!("{}:{}", pos, count))
                            .collect();
                        let unique_decls = decl_ids.len();
                        info!(
                            "lua infer profile: {} [misses={}/{} unique={} removals={} recursive={} max_depth={} err: fnf={} ue={} udt={} umt={} utd={} uo={} um={} usr={} index={} in {}ms call={} in {}ms name={} in {}ms table={} in {}ms other={}ms flow: calls={} hits={} ({}%) nodes_walked={} walk_avg={} walk_max={} flow_cache={} expr_cache={} udt_unique={} udt_decls={} udt_top={}]",
                            path,
                            total_miss,
                            cache.prof_infer_expr_calls,
                            unique_inferred,
                            cache_removals,
                            recursive_calls,
                            max_depth,
                            err_fnf,
                            err_ue,
                            err_udt,
                            err_umt,
                            err_utd,
                            err_uo,
                            err_um,
                            err_usr,
                            idx_calls,
                            idx_ns / 1_000_000,
                            call_calls,
                            call_ns / 1_000_000,
                            name_calls,
                            name_ns / 1_000_000,
                            tbl_calls,
                            tbl_ns / 1_000_000,
                            other_ns / 1_000_000,
                            flow_calls,
                            flow_hits,
                            flow_hit_pct,
                            flow_nodes_walked,
                            flow_walk_avg,
                            flow_walk_max,
                            flow_cache_entries,
                            expr_cache_entries,
                            unique_decls,
                            cache.prof_unresolve_decl_names.join(","),
                            top_ids.join(","),
                        );
                    }
                }
            }
        }
        // Workspace-level infer_expr sub-type timing aggregation
        if slow_log_enabled {
            let mut total_idx_ns: u64 = 0;
            let mut total_call_ns: u64 = 0;
            let mut total_name_ns: u64 = 0;
            let mut total_tbl_ns: u64 = 0;
            let mut total_other_ns: u64 = 0;
            let mut total_idx_calls: u32 = 0;
            let mut total_call_calls: u32 = 0;
            let mut total_name_calls: u32 = 0;
            let mut total_tbl_calls: u32 = 0;
            let mut total_misses: u32 = 0;
            let mut total_calls: u32 = 0;
            let mut total_name_local: u32 = 0;
            let mut total_name_narrow: u32 = 0;
            let mut total_name_global: u32 = 0;
            let mut total_name_self: u32 = 0;
            let mut total_name_narrow_ns: u64 = 0;
            for fid in &file_ids {
                let cache = context.infer_manager.get_infer_cache(*fid);
                total_idx_ns += cache.prof_infer_index_time_ns;
                total_call_ns += cache.prof_infer_call_time_ns;
                total_name_ns += cache.prof_infer_name_time_ns;
                total_tbl_ns += cache.prof_infer_table_time_ns;
                total_other_ns += cache.prof_infer_other_time_ns;
                total_idx_calls += cache.prof_infer_index_calls;
                total_call_calls += cache.prof_infer_call_calls;
                total_name_calls += cache.prof_infer_name_calls;
                total_tbl_calls += cache.prof_infer_table_calls;
                total_misses += cache.prof_infer_expr_calls - cache.prof_infer_expr_hits;
                total_calls += cache.prof_infer_expr_calls;
                total_name_local += cache.prof_name_local_calls;
                total_name_narrow += cache.prof_name_narrow_calls;
                total_name_global += cache.prof_name_global_calls;
                total_name_self += cache.prof_name_self_calls;
                total_name_narrow_ns += cache.prof_name_narrow_time_ns;
            }
            info!(
                "lua infer workspace total: [misses={}/{} index={} in {}ms call={} in {}ms name={} in {}ms (local={} narrow={} in {}ms global={} self={}) table={} in {}ms other={}ms]",
                total_misses,
                total_calls,
                total_idx_calls,
                total_idx_ns / 1_000_000,
                total_call_calls,
                total_call_ns / 1_000_000,
                total_name_calls,
                total_name_ns / 1_000_000,
                total_name_local,
                total_name_narrow,
                total_name_narrow_ns / 1_000_000,
                total_name_global,
                total_name_self,
                total_tbl_calls,
                total_tbl_ns / 1_000_000,
                total_other_ns / 1_000_000,
            );
        }
        if let Some(total_start) = total_start {
            info!(
                "lua analyze total: {} files in {:?}",
                file_count,
                total_start.elapsed()
            );
        }
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
        let mut stats = self
            .node_stats
            .iter()
            .map(|(kind, (count, total))| (*kind, *count, *total))
            .collect::<Vec<_>>();
        stats.sort_by_key(|(_, _, total)| std::cmp::Reverse(*total));
        let summary = stats
            .into_iter()
            .take(8)
            .map(|(kind, count, total)| format!("{kind}: {count} in {total:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        info!("lua analyze slow file node profile: {} [{}]", path, summary);
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
