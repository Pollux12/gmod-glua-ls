use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use log::info;

pub use super::checker::DiagnosticContext;
use super::checker::SharedDiagnosticData;
use super::checker::precompute_gm_method_realms;
use super::{checker::check_file, lua_diagnostic_config::LuaDiagnosticConfig};
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

        let mut gm_method_realms = HashMap::new();
        for workspace_id in module_index.get_main_workspace_ids() {
            let realms = precompute_gm_method_realms(db, workspace_id);
            gm_method_realms.insert(workspace_id, Arc::new(realms));
        }

        Arc::new(SharedDiagnosticData { gm_method_realms })
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

        // Use a flow analysis budget for diagnostics. This prevents
        // pathologically large files from dominating diagnostic time.
        // 10K nodes is sufficient — normal files use a few hundred at most,
        // but monster files (3000+ lines) can trigger millions of visits.
        const DIAGNOSTIC_FLOW_BUDGET: u32 = 10_000;

        let sem_start = Instant::now();
        let semantic_model =
            compilation.get_semantic_model_with_flow_budget(file_id, DIAGNOSTIC_FLOW_BUDGET)?;
        let sem_elapsed = sem_start.elapsed();
        if sem_elapsed.as_millis() > 10 {
            info!("diagnose_file: get_semantic_model cost {:?} for {:?}", sem_elapsed, file_id);
        }

        let mut context = if let Some(shared) = shared_data {
            DiagnosticContext::new_with_shared(file_id, db, config, cancel_token.clone(), shared)
        } else {
            DiagnosticContext::new(file_id, db, config, cancel_token.clone())
        };

        let check_start = Instant::now();
        check_file(&mut context, &semantic_model, &cancel_token);
        let check_elapsed = check_start.elapsed();
        if check_elapsed.as_millis() > 10 {
            info!("diagnose_file: check_file cost {:?} for {:?}", check_elapsed, file_id);
        }

        if cancel_token.is_cancelled() {
            return None;
        }

        Some(context.get_diagnostics())
    }
}
