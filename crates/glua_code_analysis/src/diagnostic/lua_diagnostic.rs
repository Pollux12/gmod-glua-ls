use rustc_hash::FxHashMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use log::info;

pub use super::checker::DiagnosticContext;
use super::checker::SharedDiagnosticData;
use super::shared_data_cache::SharedDiagnosticComponentCache;
use super::{checker::check_file, lua_diagnostic_config::LuaDiagnosticConfig};
use crate::semantic::LuaAnalysisPhase;
use crate::{DiagnosticCode, Emmyrc, FileId, LuaCompilation, WorkspaceId};
use lsp_types::Diagnostic;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct LuaDiagnostic {
    enable: bool,
    config: Arc<LuaDiagnosticConfig>,
    workspace_configs: FxHashMap<WorkspaceId, Arc<LuaDiagnosticConfig>>,
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
            workspace_configs: FxHashMap::default(),
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
        self.workspace_configs = configs.into_iter().collect();
    }

    // Only enable the given diagnostic
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
        self.precompute_shared_data_with_cache(compilation, &SharedDiagnosticComponentCache::new())
    }

    /// [`precompute_shared_data`](Self::precompute_shared_data) reusing what
    /// `cache` holds from earlier runs wherever the facts it read still hold;
    /// see [`SharedDiagnosticComponentCache`]. The data equals a cold build's.
    pub fn precompute_shared_data_with_cache(
        &self,
        compilation: &LuaCompilation,
        cache: &SharedDiagnosticComponentCache,
    ) -> Arc<SharedDiagnosticData> {
        cache.shared_data(compilation)
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
        let profile_start = crate::profile::phase_enabled()
            .then(|| (Instant::now(), crate::profile::cpu::thread_cpu_time()));

        let config = self.get_config_for_file(compilation, file_id);

        let slow_log_enabled = log::log_enabled!(log::Level::Info);
        let sem_start = slow_log_enabled.then(Instant::now);
        let semantic_model = compilation.get_diagnostic_semantic_model(file_id)?;
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

        let mut diagnostics = context.get_diagnostics();
        diagnostics.sort_by(|a, b| {
            (a.range.start, a.range.end, code_order(&a.code), &a.message).cmp(&(
                b.range.start,
                b.range.end,
                code_order(&b.code),
                &b.message,
            ))
        });
        if let Some((wall_start, cpu_start)) = profile_start {
            drop(semantic_model);
            let path = db
                .get_vfs()
                .get_file_path(&file_id)
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("{file_id:?}"));
            let cpu = cpu_start
                .zip(crate::profile::cpu::thread_cpu_time())
                .map(|(start, end)| end.saturating_sub(start));
            super::checker::report_file_profile(&path, wall_start.elapsed(), cpu);
        }
        Some(diagnostics)
    }
}

fn code_order(code: &Option<lsp_types::NumberOrString>) -> (u8, i32, &str) {
    match code {
        None => (0, 0, ""),
        Some(lsp_types::NumberOrString::Number(number)) => (1, *number, ""),
        Some(lsp_types::NumberOrString::String(name)) => (2, 0, name),
    }
}
