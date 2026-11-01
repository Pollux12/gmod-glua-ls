mod analyze_error;
mod diagnostic_action;

use crate::db_index::cow_shards::CowCell;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

pub use analyze_error::AnalyzeError;
pub use diagnostic_action::{DiagnosticAction, DiagnosticActionKind};
use rowan::TextRange;

use crate::{
    DiagnosticCode, FileId,
    db_index::derivation::{self, FactKey, Fingerprint},
};

use super::traits::LuaIndex;

#[derive(Debug, Clone)]
pub struct DiagnosticIndex {
    diagnostic_actions: CowCell<HashMap<FileId, Vec<DiagnosticAction>>>,
    diagnostics: CowCell<HashMap<FileId, Vec<AnalyzeError>>>,
    file_diagnostic_disabled: CowCell<HashMap<FileId, HashSet<DiagnosticCode>>>,
    file_diagnostic_enabled: CowCell<HashMap<FileId, HashSet<DiagnosticCode>>>,
}

impl Default for DiagnosticIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl DiagnosticIndex {
    pub fn new() -> Self {
        Self {
            diagnostic_actions: Default::default(),
            diagnostics: Default::default(),
            file_diagnostic_disabled: Default::default(),
            file_diagnostic_enabled: Default::default(),
        }
    }

    pub fn add_diagnostic_action(&mut self, file_id: FileId, diagnostic: DiagnosticAction) {
        derivation::note_write(|| FactKey::DiagnosticFile(file_id));
        self.diagnostic_actions
            .entry(file_id)
            .or_default()
            .push(diagnostic);
    }

    pub fn add_file_diagnostic_disabled(&mut self, file_id: FileId, code: DiagnosticCode) {
        derivation::note_write(|| FactKey::DiagnosticFile(file_id));
        self.file_diagnostic_disabled
            .entry(file_id)
            .or_default()
            .insert(code);
    }

    pub fn add_file_diagnostic_enabled(&mut self, file_id: FileId, code: DiagnosticCode) {
        derivation::note_write(|| FactKey::DiagnosticFile(file_id));
        self.file_diagnostic_enabled
            .entry(file_id)
            .or_default()
            .insert(code);
    }

    pub fn get_diagnostics_actions(&self, file_id: FileId) -> Option<&Vec<DiagnosticAction>> {
        derivation::note_read(|| FactKey::DiagnosticFile(file_id));
        self.diagnostic_actions.get(&file_id)
    }

    pub fn add_diagnostic(&mut self, file_id: FileId, diagnostic: AnalyzeError) {
        derivation::note_write(|| FactKey::DiagnosticFile(file_id));
        self.diagnostics
            .entry(file_id)
            .or_default()
            .push(diagnostic);
    }

    pub fn get_diagnostics(&self, file_id: &FileId) -> Option<&Vec<AnalyzeError>> {
        derivation::note_read(|| FactKey::DiagnosticFile(*file_id));
        self.diagnostics.get(file_id)
    }

    pub fn is_file_diagnostic_code_disabled(
        &self,
        file_id: &FileId,
        code: &DiagnosticCode,
        range: &TextRange,
    ) -> bool {
        derivation::note_read(|| FactKey::DiagnosticFile(*file_id));
        if let Some(disabled) = self.diagnostic_actions.get(file_id) {
            for action in disabled {
                if action.is_match(true, range, code) {
                    return true;
                }
            }
        }
        false
    }

    pub fn is_file_disabled(&self, file_id: &FileId, code: &DiagnosticCode) -> bool {
        derivation::note_read(|| FactKey::DiagnosticFile(*file_id));
        if let Some(disabled) = self.file_diagnostic_disabled.get(file_id) {
            disabled.contains(code)
        } else {
            false
        }
    }

    pub fn is_file_enabled(&self, file_id: &FileId, code: &DiagnosticCode) -> bool {
        derivation::note_read(|| FactKey::DiagnosticFile(*file_id));
        if let Some(enabled) = self.file_diagnostic_enabled.get(file_id) {
            enabled.contains(code)
        } else {
            false
        }
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        let FactKey::DiagnosticFile(file_id) = key else {
            debug_assert!(false, "{key:?} is not a diagnostic fact");
            return derivation::ABSENT;
        };
        let actions = self.diagnostic_actions.get(file_id);
        let diagnostics = self.diagnostics.get(file_id);
        let disabled = sorted_codes(self.file_diagnostic_disabled.get(file_id));
        let enabled = sorted_codes(self.file_diagnostic_enabled.get(file_id));
        if actions.is_none() && diagnostics.is_none() && disabled.is_none() && enabled.is_none() {
            return derivation::ABSENT;
        }
        derivation::fingerprint_of(&(actions, diagnostics, disabled, enabled))
    }
}

fn sorted_codes(codes: Option<&HashSet<DiagnosticCode>>) -> Option<Vec<DiagnosticCode>> {
    codes.map(|codes| {
        let mut codes = codes.iter().copied().collect::<Vec<_>>();
        codes.sort_unstable_by_key(|code| *code as u32);
        codes
    })
}

impl LuaIndex for DiagnosticIndex {
    fn remove(&mut self, file_id: FileId) {
        derivation::note_write(|| FactKey::DiagnosticFile(file_id));
        self.diagnostic_actions.remove(&file_id);
        self.diagnostics.remove(&file_id);
        self.file_diagnostic_disabled.remove(&file_id);
        self.file_diagnostic_enabled.remove(&file_id);
    }

    fn clear(&mut self) {
        if derivation::recording() {
            for file_id in self
                .diagnostic_actions
                .keys()
                .chain(self.diagnostics.keys())
                .chain(self.file_diagnostic_disabled.keys())
                .chain(self.file_diagnostic_enabled.keys())
            {
                derivation::note_write(|| FactKey::DiagnosticFile(*file_id));
            }
        }
        self.diagnostic_actions.clear();
        self.diagnostics.clear();
        self.file_diagnostic_disabled.clear();
        self.file_diagnostic_enabled.clear();
    }
}

crate::db_index::cow_shards::task_fork_fields!(DiagnosticIndex {
    diagnostic_actions,
    diagnostics,
    file_diagnostic_disabled,
    file_diagnostic_enabled,
});
