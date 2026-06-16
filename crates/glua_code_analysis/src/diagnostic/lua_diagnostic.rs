use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use log::info;
use rustc_hash::FxHashMap;

pub use super::checker::DiagnosticContext;
use super::checker::SharedDiagnosticData;
use super::checker::precompute_await_candidates;
use super::checker::precompute_callee_realm_data_for_workspace;
use super::checker::precompute_gm_method_realms;
use super::checker::precompute_missing_required_fields;
use super::checker::precompute_nodiscard_candidates;
use super::checker::precompute_param_type_candidates;
use super::checker::precompute_sorted_send_flows;
use super::checker::precompute_subclass_fields;
use super::{checker::check_file, lua_diagnostic_config::LuaDiagnosticConfig};
use crate::semantic::LuaAnalysisPhase;
use crate::{DiagnosticCode, Emmyrc, FileId, LuaCompilation, WorkspaceId};
use lsp_types::Diagnostic;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub struct LuaDiagnostic {
    enable: bool,
    config: Arc<LuaDiagnosticConfig>,
    workspace_configs: HashMap<WorkspaceId, Arc<LuaDiagnosticConfig>>,
}

impl Default for LuaDiagnostic {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaDiagnostic {
    pub fn new() -> Self {
        Self {
            enable: true,
            config: Arc::new(LuaDiagnosticConfig::default()),
            workspace_configs: HashMap::new(),
        }
    }

    pub fn update_config(&mut self, emmyrc: Arc<Emmyrc>) {
        self.enable = emmyrc.diagnostics.enable;
        self.config = LuaDiagnosticConfig::new(&emmyrc).into();
        self.workspace_configs.clear();
    }

    pub fn set_workspace_configs(
        &mut self,
        configs: HashMap<WorkspaceId, Arc<LuaDiagnosticConfig>>,
    ) {
        self.workspace_configs = configs;
    }

    // 只开启指定的诊断
    pub fn enable_only(&mut self, code: DiagnosticCode) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.diagnostics.enables.push(code);
        for diagnostic_code in DiagnosticCode::all().iter() {
            if *diagnostic_code != code {
                emmyrc.diagnostics.disable.push(*diagnostic_code);
            }
        }
        self.config = LuaDiagnosticConfig::new(&emmyrc).into();
    }

    fn get_config_for_file(
        &self,
        compilation: &LuaCompilation,
        file_id: FileId,
    ) -> Arc<LuaDiagnosticConfig> {
        if !self.workspace_configs.is_empty() {
            let db = compilation.get_db();
            if let Some(workspace_id) = db.get_module_index().get_workspace_id(file_id) {
                if let Some(config) = self.workspace_configs.get(&workspace_id) {
                    return config.clone();
                }
            }
        }
        self.config.clone()
    }

    pub fn diagnose_file(
        &self,
        compilation: &LuaCompilation,
        file_id: FileId,
        cancel_token: CancellationToken,
    ) -> Option<Vec<Diagnostic>> {
        self.diagnose_file_inner(compilation, file_id, cancel_token, None)
    }

    pub fn diagnose_file_with_shared(
        &self,
        compilation: &LuaCompilation,
        file_id: FileId,
        cancel_token: CancellationToken,
        shared_data: Arc<SharedDiagnosticData>,
    ) -> Option<Vec<Diagnostic>> {
        self.diagnose_file_inner(compilation, file_id, cancel_token, Some(shared_data))
    }

    /// Precompute shared diagnostic data once for use across all files.
    /// This avoids per-file recomputation of workspace-wide annotations.
    pub fn precompute_shared_data(
        &self,
        compilation: &LuaCompilation,
    ) -> Arc<SharedDiagnosticData> {
        let db = compilation.get_db();
        let module_index = db.get_module_index();
        let mut workspace_file_ids = module_index.get_main_workspace_file_ids();
        workspace_file_ids.sort_unstable();

        let mut gm_method_realms = HashMap::new();
        let mut callee_realms_by_workspace = HashMap::new();
        let mut realm_call_candidates_by_workspace = HashMap::new();
        for workspace_id in module_index.get_main_workspace_ids() {
            let realms = Arc::new(precompute_gm_method_realms(db, workspace_id));
            let mut callee_realm_data =
                precompute_callee_realm_data_for_workspace(db, workspace_id, &workspace_file_ids);
            callee_realm_data
                .realm_call_candidates
                .insert_gm_method_realms(realms.as_ref());
            gm_method_realms.insert(workspace_id, realms);
            callee_realms_by_workspace
                .insert(workspace_id, Arc::new(callee_realm_data.callee_realms));
            realm_call_candidates_by_workspace.insert(
                workspace_id,
                Arc::new(callee_realm_data.realm_call_candidates),
            );
        }

        let missing_required_fields = precompute_missing_required_fields(db);
        let subclass_fields = precompute_subclass_fields(db);
        let await_candidates = precompute_await_candidates(db);
        let param_type_candidates = precompute_param_type_candidates(db);
        let nodiscard_candidates = precompute_nodiscard_candidates(db);

        // Precompute decl annotation realms for all workspace files
        let decl_annotation_realms = precompute_decl_annotation_realms(db, &workspace_file_ids);

        // Precompute sorted send flows so each file doesn't re-sort them.
        let sorted_send_flows = Arc::new(precompute_sorted_send_flows(
            db.get_gmod_network_index(),
            db.get_vfs(),
        ));

        Arc::new(SharedDiagnosticData {
            workspace_file_ids: Arc::new(workspace_file_ids),
            gm_method_realms,
            callee_realms_by_workspace,
            realm_call_candidates_by_workspace,
            missing_required_fields: Arc::new(missing_required_fields),
            subclass_fields: Arc::new(subclass_fields),
            await_candidates: Arc::new(await_candidates),
            param_type_candidates: Arc::new(param_type_candidates),
            nodiscard_candidates: Arc::new(nodiscard_candidates),
            decl_annotation_realms: Arc::new(decl_annotation_realms),
            sorted_send_flows,
        })
    }

    fn diagnose_file_inner(
        &self,
        compilation: &LuaCompilation,
        file_id: FileId,
        cancel_token: CancellationToken,
        shared_data: Option<Arc<SharedDiagnosticData>>,
    ) -> Option<Vec<Diagnostic>> {
        if !self.enable {
            return None;
        }

        if cancel_token.is_cancelled() {
            return None;
        }

        let db = compilation.get_db();
        if let Some(workspace_id) = db.get_module_index().get_workspace_id(file_id)
            && !db.get_module_index().is_main_workspace_id(workspace_id)
        {
            return None;
        }

        let config = self.get_config_for_file(compilation, file_id);

        let slow_log_enabled = log::log_enabled!(log::Level::Info);
        let sem_start = slow_log_enabled.then(Instant::now);
        let semantic_model = compilation.get_semantic_model(file_id)?;
        // Set diagnostics phase so error results are cached (types are final,
        // no subsequent unresolve pass will change them). This prevents expensive
        // recomputation across diagnostic checkers.
        semantic_model.set_analysis_phase(LuaAnalysisPhase::Diagnostics);
        if let Some(sem_start) = sem_start {
            let sem_elapsed = sem_start.elapsed();
            if sem_elapsed.as_millis() > 10 {
                info!(
                    "diagnose_file: get_semantic_model cost {:?} for {:?}",
                    sem_elapsed, file_id
                );
            }
        }

        let mut context = if let Some(shared) = shared_data {
            DiagnosticContext::new_with_shared(file_id, db, config, cancel_token.clone(), shared)
        } else {
            DiagnosticContext::new(file_id, db, config, cancel_token.clone())
        };

        check_file(&mut context, &semantic_model, &cancel_token);

        if cancel_token.is_cancelled() {
            return None;
        }

        Some(context.get_diagnostics())
    }
}

fn precompute_decl_annotation_realms(
    db: &crate::DbIndex,
    workspace_file_ids: &[FileId],
) -> FxHashMap<FileId, Vec<super::checker::AnnotatedRealmRange>> {
    use super::checker::collect_decl_annotation_realms_for_file_precompute;
    let mut cache = FxHashMap::default();
    for &file_id in workspace_file_ids {
        let realms = collect_decl_annotation_realms_for_file_precompute(db, &file_id);
        if !realms.is_empty() {
            cache.insert(file_id, realms);
        }
    }
    cache
}
