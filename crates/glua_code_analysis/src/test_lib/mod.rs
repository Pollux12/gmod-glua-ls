#[cfg(test)]
use std::{collections::BTreeSet, path::Path};
use std::{ops::Deref, sync::Arc};

use glua_parser::{LuaAstNode, LuaAstToken, LuaLocalName};
use lsp_types::NumberOrString;
#[cfg(test)]
use lsp_types::{Diagnostic, DiagnosticSeverity};
use tokio_util::sync::CancellationToken;

use crate::{
    DbIndex, DiagnosticCode, EmmyLuaAnalysis, Emmyrc, FileId, LuaType, RenderLevel,
    VirtualUrlGenerator, check_type_compact, humanize_type,
};

/// A virtual workspace for testing.
#[allow(unused)]
#[derive(Debug)]
pub struct VirtualWorkspace {
    pub virtual_url_generator: VirtualUrlGenerator,
    pub analysis: EmmyLuaAnalysis,
    id_counter: u32,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DiagnosticSnapshot {
    pub file: String,
    pub range_start_line: u32,
    pub range_start_character: u32,
    pub range_end_line: u32,
    pub range_end_character: u32,
    pub severity: Option<i32>,
    pub code: Option<String>,
    pub message: String,
}

#[cfg(test)]
pub fn diagnostics_to_snapshot_set(
    file: impl Into<String>,
    diagnostics: Vec<Diagnostic>,
) -> BTreeSet<DiagnosticSnapshot> {
    let file = file.into();
    diagnostics
        .into_iter()
        .map(|diagnostic| DiagnosticSnapshot {
            file: file.clone(),
            range_start_line: diagnostic.range.start.line,
            range_start_character: diagnostic.range.start.character,
            range_end_line: diagnostic.range.end.line,
            range_end_character: diagnostic.range.end.character,
            severity: diagnostic.severity.map(diagnostic_severity_to_i32),
            code: diagnostic.code.map(number_or_string_to_string),
            message: diagnostic.message,
        })
        .collect()
}

#[cfg(test)]
fn diagnostic_severity_to_i32(severity: DiagnosticSeverity) -> i32 {
    match severity {
        DiagnosticSeverity::ERROR => 1,
        DiagnosticSeverity::WARNING => 2,
        DiagnosticSeverity::INFORMATION => 3,
        DiagnosticSeverity::HINT => 4,
        _ => 0,
    }
}

#[cfg(test)]
fn number_or_string_to_string(code: NumberOrString) -> String {
    match code {
        NumberOrString::Number(number) => number.to_string(),
        NumberOrString::String(text) => text,
    }
}

#[cfg(test)]
fn normalize_snapshot_file(base: &Path, file_path: &Path) -> String {
    let normalized = file_path
        .strip_prefix(base)
        .unwrap_or(file_path)
        .to_string_lossy()
        .replace('\\', "/");
    if normalized.is_empty() {
        file_path.to_string_lossy().replace('\\', "/")
    } else {
        normalized
    }
}

#[allow(unused, clippy::unwrap_used)]
impl Default for VirtualWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualWorkspace {
    pub fn new() -> Self {
        let generator = VirtualUrlGenerator::new();
        let mut analysis = EmmyLuaAnalysis::new();
        let base = &generator.base;
        analysis.add_main_workspace(base.clone());
        VirtualWorkspace {
            virtual_url_generator: generator,
            analysis,
            id_counter: 0,
        }
    }

    pub fn new_with_init_std_lib() -> Self {
        let generator = VirtualUrlGenerator::new();
        let mut analysis = EmmyLuaAnalysis::new();
        analysis.init_std_lib(None);
        let base = &generator.base;
        analysis.add_main_workspace(base.clone());
        VirtualWorkspace {
            virtual_url_generator: generator,
            analysis,
            id_counter: 0,
        }
    }

    pub fn def(&mut self, content: &str) -> FileId {
        let id = self.id_counter;
        self.id_counter += 1;
        let uri = self
            .virtual_url_generator
            .new_uri(&format!("virtual_{}.lua", id));

        self.analysis
            .update_file_by_uri(&uri, Some(content.to_string()))
            .expect("File ID must be present")
    }

    pub fn def_file(&mut self, file_name: &str, content: &str) -> FileId {
        let uri = self.virtual_url_generator.new_uri(file_name);

        self.analysis
            .update_file_by_uri(&uri, Some(content.to_string()))
            .expect("File ID must be present")
    }

    pub fn def_files(&mut self, files: Vec<(&str, &str)>) -> Vec<FileId> {
        let file_infos = files
            .iter()
            .map(|(file_name, content)| {
                let uri = self.virtual_url_generator.new_uri(file_name);
                (uri, Some(content.to_string()))
            })
            .collect();

        let mut file_ids = self.analysis.update_files_by_uri_sorted(file_infos);
        file_ids.sort();

        file_ids
    }

    pub fn get_emmyrc(&self) -> Emmyrc {
        self.analysis.emmyrc.deref().clone()
    }

    pub fn update_emmyrc(&mut self, emmyrc: Emmyrc) {
        self.analysis.update_config(Arc::new(emmyrc));
    }

    pub fn get_node<Ast: LuaAstNode>(&self, file_id: FileId) -> Ast {
        let tree = self
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("Tree must exist");
        tree.get_chunk_node()
            .descendants::<Ast>()
            .next()
            .expect("Node must exist")
    }

    pub fn ty(&mut self, type_repr: &str) -> LuaType {
        let virtual_content = format!("---@type {}\nlocal t", type_repr);
        let file_id = self.def(&virtual_content);
        let local_name = self.get_node::<LuaLocalName>(file_id);
        let semantic_model = self
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let token = local_name.get_name_token().expect("Name token must exist");
        let info = semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .expect("Semantic info must exist");
        info.typ
    }

    pub fn expr_ty(&mut self, expr: &str) -> LuaType {
        let virtual_content = format!("local t = {}", expr);
        let file_id = self.def(&virtual_content);
        let local_name = self.get_node::<LuaLocalName>(file_id);
        let semantic_model = self
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Model must exist");
        let token = local_name.get_name_token().expect("Name token must exist");
        let info = semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .expect("Semantic info must exist");
        info.typ
    }

    pub fn check_type(&self, source: &LuaType, compact_type: &LuaType) -> bool {
        let db = &self.analysis.compilation.get_db();
        check_type_compact(db, source, compact_type).is_ok()
    }

    pub fn enable_check(&mut self, diagnostic_code: DiagnosticCode) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.diagnostics.enables.push(diagnostic_code);
        self.analysis.diagnostic.update_config(Arc::new(emmyrc));
    }

    /// 只执行对应诊断代码的检查, 必须要在对应的`Checker`中为`const CODES`添加对应的诊断代码
    pub fn check_code_for(&mut self, diagnostic_code: DiagnosticCode, block_str: &str) -> bool {
        // 只启用对应的诊断
        self.analysis.diagnostic.enable_only(diagnostic_code);
        let file_id = self.def(block_str);
        let result = self
            .analysis
            .diagnose_file(file_id, CancellationToken::new());
        if let Some(diagnostics) = result {
            let code_string = Some(NumberOrString::String(
                diagnostic_code.get_name().to_string(),
            ));
            for diagnostic in diagnostics {
                if diagnostic.code == code_string {
                    return false;
                }
            }
        }

        true
    }

    /// Like `check_code_for` but registers the content under a specific file path.
    /// Useful for testing diagnostics that depend on path-based detection (e.g. scripted classes).
    /// Returns `true` if no diagnostic of the given code is emitted.
    pub fn check_file_for(
        &mut self,
        diagnostic_code: DiagnosticCode,
        file_name: &str,
        content: &str,
    ) -> bool {
        self.analysis.diagnostic.enable_only(diagnostic_code);
        let file_id = self.def_file(file_name, content);
        let result = self
            .analysis
            .diagnose_file(file_id, CancellationToken::new());
        if let Some(diagnostics) = result {
            let code_string = Some(NumberOrString::String(
                diagnostic_code.get_name().to_string(),
            ));
            for diagnostic in diagnostics {
                if diagnostic.code == code_string {
                    return false;
                }
            }
        }
        true
    }

    pub fn check_code_for_namespace(
        &mut self,
        diagnostic_code: DiagnosticCode,
        block_str: &str,
    ) -> bool {
        self.check_code_for(
            diagnostic_code,
            &format!(
                "---@namespace TestNamespace{}\n{}",
                self.id_counter, block_str
            ),
        )
    }

    pub fn enable_full_diagnostic(&mut self) {
        let mut emmyrc = Emmyrc::default();
        let mut enables = emmyrc.diagnostics.enables;
        enables.push(DiagnosticCode::IncompleteSignatureDoc);
        enables.push(DiagnosticCode::MissingGlobalDoc);
        emmyrc.diagnostics.enables = enables;
        self.analysis.diagnostic.update_config(Arc::new(emmyrc));
    }

    pub fn humanize_type(&self, ty: LuaType) -> String {
        let db = &self.analysis.compilation.get_db();
        humanize_type(db, &ty, RenderLevel::Brief)
    }

    pub fn humanize_type_detailed(&self, ty: LuaType) -> String {
        let db = &self.analysis.compilation.get_db();
        humanize_type(db, &ty, RenderLevel::Detailed)
    }

    pub fn get_db_mut(&mut self) -> &mut DbIndex {
        (self.analysis.compilation.get_db_mut()) as _
    }

    #[cfg(test)]
    pub fn run_diagnostics_with_shared_snapshots(
        &self,
        file_ids: &[FileId],
    ) -> BTreeSet<DiagnosticSnapshot> {
        let shared_snapshot = self.analysis.precompute_diagnostic_shared_data();
        let mut combined = BTreeSet::new();

        for &file_id in file_ids {
            let diagnostics = self
                .analysis
                .diagnose_file_with_shared(file_id, CancellationToken::new(), shared_snapshot.clone())
                .unwrap_or_else(|| {
                    let file = self
                        .analysis
                        .compilation
                        .get_db()
                        .get_vfs()
                        .get_file_path(&file_id)
                        .map(|path| normalize_snapshot_file(&self.virtual_url_generator.base, path))
                        .unwrap_or_else(|| format!("file-id:{}", file_id.id));
                    panic!(
                        "expected diagnostics vector for selected file while collecting shared snapshots: {}",
                        file
                    );
                });
            combined.extend(self.diagnostic_snapshots_for_file(file_id, diagnostics));
        }

        combined
    }

    #[cfg(test)]
    pub fn diagnostic_snapshots_for_file(
        &self,
        file_id: FileId,
        diagnostics: Vec<Diagnostic>,
    ) -> BTreeSet<DiagnosticSnapshot> {
        let normalized_file = self
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_path(&file_id)
            .map(|path| normalize_snapshot_file(&self.virtual_url_generator.base, path))
            .unwrap_or_else(|| format!("file-id:{}", file_id.id));

        diagnostics_to_snapshot_set(normalized_file, diagnostics)
    }
}

#[cfg(test)]
mod tests {
    use crate::LuaType;

    use super::VirtualWorkspace;

    #[test]
    fn test_basic() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@class a
        "#,
        );

        let ty = ws.ty("a");
        match ty {
            LuaType::Ref(i) => {
                assert_eq!(i.get_name(), "a");
            }
            _ => unreachable!(),
        }
    }
}
