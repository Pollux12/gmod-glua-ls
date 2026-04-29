mod json_output_writer;
mod sarif_output_writer;
mod text_output_writer;

use std::{
    cmp::Ordering,
    path::{Path, PathBuf},
};

use glua_code_analysis::{DbIndex, FileId};
use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString};
use tokio::sync::mpsc::Receiver;

use crate::cmd_args::{OutputDestination, OutputFormat};

use crate::terminal_display::TerminalDisplay;

/// Type alias for diagnostic result channel
type DiagnosticReceiver = Receiver<(FileId, Option<Vec<Diagnostic>>)>;

#[derive(Debug, Clone)]
struct OutputDiagnostic {
    file_id: FileId,
    normalized_path: String,
    diagnostic: Diagnostic,
}

#[derive(Debug)]
struct OutputFileEntry {
    file_id: FileId,
    normalized_path: String,
    diagnostics: Vec<Diagnostic>,
}

fn normalize_path_for_sort(path: &Path) -> String {
    #[cfg(windows)]
    {
        let path_str = path.to_string_lossy();
        let stripped = path_str.strip_prefix(r"\\?\").unwrap_or(&path_str);
        stripped.replace('\\', "/").to_ascii_lowercase()
    }

    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}

fn severity_sort_key(severity: Option<DiagnosticSeverity>) -> u8 {
    match severity {
        Some(DiagnosticSeverity::ERROR) => 1,
        Some(DiagnosticSeverity::WARNING) => 2,
        Some(DiagnosticSeverity::INFORMATION) => 3,
        Some(DiagnosticSeverity::HINT) => 4,
        _ => 5,
    }
}

#[cfg(test)]
fn severity_test_key(severity: Option<DiagnosticSeverity>) -> u8 {
    severity_sort_key(severity)
}

fn code_sort_key(code: &Option<NumberOrString>) -> (u8, String) {
    match code {
        Some(NumberOrString::Number(value)) => (0, value.to_string()),
        Some(NumberOrString::String(value)) => (1, value.clone()),
        None => (2, String::new()),
    }
}

fn compare_output_diagnostics(a: &OutputDiagnostic, b: &OutputDiagnostic) -> Ordering {
    let code_a = code_sort_key(&a.diagnostic.code);
    let code_b = code_sort_key(&b.diagnostic.code);

    a.normalized_path
        .cmp(&b.normalized_path)
        .then_with(|| a.file_id.cmp(&b.file_id))
        .then_with(|| {
            a.diagnostic
                .range
                .start
                .line
                .cmp(&b.diagnostic.range.start.line)
        })
        .then_with(|| {
            a.diagnostic
                .range
                .start
                .character
                .cmp(&b.diagnostic.range.start.character)
        })
        .then_with(|| {
            a.diagnostic
                .range
                .end
                .line
                .cmp(&b.diagnostic.range.end.line)
        })
        .then_with(|| {
            a.diagnostic
                .range
                .end
                .character
                .cmp(&b.diagnostic.range.end.character)
        })
        .then_with(|| {
            severity_sort_key(a.diagnostic.severity).cmp(&severity_sort_key(b.diagnostic.severity))
        })
        .then_with(|| code_a.cmp(&code_b))
        .then_with(|| a.diagnostic.message.cmp(&b.diagnostic.message))
}

pub async fn output_result(
    total_count: usize,
    db: &DbIndex,
    workspace: PathBuf,
    mut receiver: DiagnosticReceiver,
    output_format: OutputFormat,
    output: OutputDestination,
    warnings_as_errors: bool,
) -> i32 {
    let mut writer: Box<dyn OutputWriter> = match output_format {
        OutputFormat::Json => Box::new(json_output_writer::JsonOutputWriter::new(output)),
        OutputFormat::Text => {
            Box::new(text_output_writer::TextOutputWriter::new(workspace.clone()))
        }
        OutputFormat::Sarif => Box::new(sarif_output_writer::SarifOutputWriter::new(output)),
    };

    let terminal_display = TerminalDisplay::new(workspace);
    let mut has_error = false;
    let mut count = 0;
    let mut error_count = 0;
    let mut warning_count = 0;
    let mut info_count = 0;
    let mut hint_count = 0;
    let mut collected_diagnostics = Vec::new();
    let mut json_empty_files = Vec::new();

    while let Some((file_id, diagnostics)) = receiver.recv().await {
        count += 1;
        if let Some(diagnostics) = diagnostics {
            let normalized_path = db
                .get_vfs()
                .get_file_path(&file_id)
                .map(|path| normalize_path_for_sort(path.as_path()))
                .unwrap_or_else(|| format!("{file_id:?}"));
            if diagnostics.is_empty() && output_format == OutputFormat::Json {
                json_empty_files.push((file_id, normalized_path.clone()));
            }
            for diagnostic in &diagnostics {
                match diagnostic.severity {
                    Some(lsp_types::DiagnosticSeverity::ERROR) => {
                        has_error = true;
                        error_count += 1;
                    }
                    Some(lsp_types::DiagnosticSeverity::WARNING) => {
                        if warnings_as_errors {
                            has_error = true;
                        }
                        warning_count += 1;
                    }
                    Some(lsp_types::DiagnosticSeverity::INFORMATION) => {
                        info_count += 1;
                    }
                    Some(lsp_types::DiagnosticSeverity::HINT) => {
                        hint_count += 1;
                    }
                    _ => {}
                }
            }
            for diagnostic in diagnostics {
                collected_diagnostics.push(OutputDiagnostic {
                    file_id,
                    normalized_path: normalized_path.clone(),
                    diagnostic,
                });
            }
        }

        if count == total_count {
            break;
        }
    }

    collected_diagnostics.sort_by(compare_output_diagnostics);
    let mut current_file_id = None;
    let mut current_normalized_path = None;
    let mut current_file_diagnostics = Vec::new();
    let mut ordered_diagnostic_files = Vec::new();
    for output_diagnostic in collected_diagnostics {
        if current_file_id != Some(output_diagnostic.file_id)
            && !current_file_diagnostics.is_empty()
        {
            ordered_diagnostic_files.push(OutputFileEntry {
                file_id: current_file_id
                    .expect("current file id must be set when buffered diagnostics exist"),
                normalized_path: current_normalized_path
                    .take()
                    .expect("current normalized path must be set when buffered diagnostics exist"),
                diagnostics: std::mem::take(&mut current_file_diagnostics),
            });
        }

        if current_file_id != Some(output_diagnostic.file_id) {
            current_file_id = Some(output_diagnostic.file_id);
            current_normalized_path = Some(output_diagnostic.normalized_path.clone());
        }
        current_file_diagnostics.push(output_diagnostic.diagnostic);
    }

    if let Some(file_id) = current_file_id
        && !current_file_diagnostics.is_empty()
    {
        ordered_diagnostic_files.push(OutputFileEntry {
            file_id,
            normalized_path: current_normalized_path
                .expect("current normalized path must be set for trailing buffered diagnostics"),
            diagnostics: current_file_diagnostics,
        });
    }

    if output_format == OutputFormat::Json {
        let mut ordered_json_files = ordered_diagnostic_files;
        json_empty_files.sort_unstable_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        ordered_json_files.extend(json_empty_files.into_iter().map(
            |(file_id, normalized_path)| OutputFileEntry {
                file_id,
                normalized_path,
                diagnostics: Vec::new(),
            },
        ));
        ordered_json_files.sort_unstable_by(|a, b| {
            a.normalized_path
                .cmp(&b.normalized_path)
                .then_with(|| a.file_id.cmp(&b.file_id))
        });

        for file_entry in ordered_json_files {
            writer.write(db, file_entry.file_id, file_entry.diagnostics);
        }
    } else {
        for file_entry in ordered_diagnostic_files {
            writer.write(db, file_entry.file_id, file_entry.diagnostics);
        }
    }

    writer.finish();

    // 只在 Text 格式时显示汇总
    if output_format == OutputFormat::Text {
        terminal_display.print_summary(error_count, warning_count, info_count, hint_count);
    }

    if has_error { 1 } else { 0 }
}

trait OutputWriter {
    fn write(&mut self, db: &DbIndex, file_id: FileId, diagnostics: Vec<Diagnostic>);

    fn finish(&mut self);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{OutputDiagnostic, compare_output_diagnostics, output_result, severity_test_key};
    use crate::cmd_args::{OutputDestination, OutputFormat};
    use glua_code_analysis::{DbIndex, FileId, file_path_to_uri};
    use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};
    use serde_json::Value;

    fn make_diagnostic(
        file_id: u32,
        file_path: &str,
        start_line: u32,
        start_character: u32,
        severity: Option<DiagnosticSeverity>,
        code: Option<NumberOrString>,
        message: &str,
    ) -> OutputDiagnostic {
        OutputDiagnostic {
            file_id: FileId::new(file_id),
            normalized_path: file_path.to_string(),
            diagnostic: Diagnostic {
                range: Range {
                    start: Position {
                        line: start_line,
                        character: start_character,
                    },
                    end: Position {
                        line: start_line,
                        character: start_character + 1,
                    },
                },
                severity,
                code,
                message: message.to_string(),
                ..Diagnostic::default()
            },
        }
    }

    fn tuple_key(output: &OutputDiagnostic) -> (String, u32, u32, u8, String, String) {
        (
            output.normalized_path.clone(),
            output.diagnostic.range.start.line,
            output.diagnostic.range.start.character,
            severity_test_key(output.diagnostic.severity),
            output
                .diagnostic
                .code
                .as_ref()
                .map(|value| match value {
                    NumberOrString::Number(number) => number.to_string(),
                    NumberOrString::String(string) => string.clone(),
                })
                .unwrap_or_default(),
            output.diagnostic.message.clone(),
        )
    }

    #[test]
    fn diagnostics_sort_by_canonical_tuple() {
        let mut diagnostics = vec![
            make_diagnostic(
                1,
                "b.lua",
                2,
                3,
                Some(DiagnosticSeverity::WARNING),
                Some(NumberOrString::String("W2".to_string())),
                "zzz",
            ),
            make_diagnostic(
                0,
                "a.lua",
                2,
                1,
                Some(DiagnosticSeverity::ERROR),
                Some(NumberOrString::String("E1".to_string())),
                "aaa",
            ),
            make_diagnostic(
                0,
                "a.lua",
                1,
                4,
                Some(DiagnosticSeverity::WARNING),
                Some(NumberOrString::String("W1".to_string())),
                "bbb",
            ),
        ];
        diagnostics.sort_by(compare_output_diagnostics);

        assert_eq!(
            diagnostics
                .into_iter()
                .map(|item| tuple_key(&item))
                .collect::<Vec<_>>(),
            vec![
                (
                    "a.lua".to_string(),
                    1,
                    4,
                    severity_test_key(Some(DiagnosticSeverity::WARNING)),
                    "W1".to_string(),
                    "bbb".to_string()
                ),
                (
                    "a.lua".to_string(),
                    2,
                    1,
                    severity_test_key(Some(DiagnosticSeverity::ERROR)),
                    "E1".to_string(),
                    "aaa".to_string()
                ),
                (
                    "b.lua".to_string(),
                    2,
                    3,
                    severity_test_key(Some(DiagnosticSeverity::WARNING)),
                    "W2".to_string(),
                    "zzz".to_string()
                ),
            ]
        );
    }

    #[test]
    fn diagnostics_sort_is_stable_for_reordered_inputs() {
        let mut forward = vec![
            make_diagnostic(
                5,
                "alpha.lua",
                4,
                1,
                Some(DiagnosticSeverity::WARNING),
                Some(NumberOrString::String("W".to_string())),
                "beta",
            ),
            make_diagnostic(
                5,
                "alpha.lua",
                4,
                0,
                Some(DiagnosticSeverity::WARNING),
                Some(NumberOrString::String("W".to_string())),
                "alpha",
            ),
            make_diagnostic(
                2,
                "bravo.lua",
                0,
                0,
                Some(DiagnosticSeverity::ERROR),
                Some(NumberOrString::String("E".to_string())),
                "omega",
            ),
        ];
        let mut reverse = forward.clone();
        reverse.reverse();

        forward.sort_by(compare_output_diagnostics);
        reverse.sort_by(compare_output_diagnostics);

        assert_eq!(
            forward
                .into_iter()
                .map(|item| tuple_key(&item))
                .collect::<Vec<_>>(),
            reverse
                .into_iter()
                .map(|item| tuple_key(&item))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    #[cfg(windows)]
    fn normalize_path_for_sort_removes_verbatim_prefix_and_normalizes_case() {
        let normalized =
            super::normalize_path_for_sort(std::path::Path::new(r"\\?\C:\Project\File.lua"));
        assert_eq!(normalized, "c:/project/file.lua");
    }

    #[tokio::test]
    async fn json_output_keeps_clean_file_entries_and_sorts_diagnostics() {
        let mut db = DbIndex::new();
        let output_path = PathBuf::from("target").join(format!(
            "glua_check_output_test_{}.json",
            std::process::id()
        ));

        let base =
            std::env::temp_dir().join(format!("glua_check_output_test_{}", std::process::id()));
        let file_a = base.join("a_clean.lua");
        let file_b = base.join("b_diag.lua");
        let file_c = base.join("c_diag.lua");
        let file_a_id = db
            .get_vfs_mut()
            .set_file_content(&file_path_to_uri(&file_a).expect("valid file uri"), None);
        let file_b_id = db
            .get_vfs_mut()
            .set_file_content(&file_path_to_uri(&file_b).expect("valid file uri"), None);
        let file_c_id = db
            .get_vfs_mut()
            .set_file_content(&file_path_to_uri(&file_c).expect("valid file uri"), None);

        let (sender, receiver) = tokio::sync::mpsc::channel(3);
        sender
            .send((
                file_b_id,
                Some(vec![Diagnostic {
                    range: Range {
                        start: Position {
                            line: 2,
                            character: 0,
                        },
                        end: Position {
                            line: 2,
                            character: 1,
                        },
                    },
                    severity: Some(DiagnosticSeverity::WARNING),
                    message: "b warning".to_string(),
                    ..Diagnostic::default()
                }]),
            ))
            .await
            .expect("send should succeed");
        sender
            .send((
                file_c_id,
                Some(vec![Diagnostic {
                    range: Range {
                        start: Position {
                            line: 1,
                            character: 0,
                        },
                        end: Position {
                            line: 1,
                            character: 1,
                        },
                    },
                    severity: Some(DiagnosticSeverity::ERROR),
                    message: "c error".to_string(),
                    ..Diagnostic::default()
                }]),
            ))
            .await
            .expect("send should succeed");
        sender
            .send((file_a_id, Some(Vec::new())))
            .await
            .expect("send should succeed");
        drop(sender);

        let exit_code = output_result(
            3,
            &db,
            PathBuf::from("."),
            receiver,
            OutputFormat::Json,
            OutputDestination::File(output_path.clone()),
            false,
        )
        .await;
        assert_eq!(exit_code, 1);

        let output_content = std::fs::read_to_string(&output_path).expect("output file exists");
        let output_json: Value = serde_json::from_str(&output_content).expect("valid json output");
        let files = output_json
            .as_array()
            .expect("json output should be an array");
        assert_eq!(files.len(), 3);

        let file_names = files
            .iter()
            .map(|file| {
                std::path::Path::new(
                    file.get("file")
                        .and_then(Value::as_str)
                        .expect("file path should be a string"),
                )
                .file_name()
                .and_then(|name| name.to_str())
                .expect("file path should contain file name")
                .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            file_names,
            vec![
                "a_clean.lua".to_string(),
                "b_diag.lua".to_string(),
                "c_diag.lua".to_string(),
            ]
        );

        let diagnostics_lengths = files
            .iter()
            .map(|file| {
                file.get("diagnostics")
                    .and_then(Value::as_array)
                    .map(std::vec::Vec::len)
                    .expect("diagnostics must be an array")
            })
            .collect::<Vec<_>>();
        assert_eq!(diagnostics_lengths, vec![0, 1, 1]);

        std::fs::remove_file(output_path).expect("temporary output file cleanup should succeed");
    }
}
