use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use log::info;

pub use super::checker::DiagnosticContext;
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

        let sem_start = Instant::now();
        let semantic_model = compilation.get_semantic_model(file_id)?;
        let sem_elapsed = sem_start.elapsed();
        if sem_elapsed.as_millis() > 10 {
            info!("diagnose_file: get_semantic_model cost {:?} for {:?}", sem_elapsed, file_id);
        }

        let mut context = DiagnosticContext::new(file_id, db, config, cancel_token.clone());

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
