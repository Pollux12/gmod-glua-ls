use lsp_types::Diagnostic;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Identity of a computed diagnostic set, for LSP 3.17 pull diagnostics.
///
/// The client sends the previous id back as `previousResultId`; when it still
/// matches, the server answers `unchanged` and the client leaves its
/// diagnostics untouched instead of repainting them.
///
/// Derived purely from content, so no bookkeeping can go stale, and computed
/// order-insensitively so a reordered-but-equal set does not look changed.
/// Each item is hashed from its serialized form so every field counts,
/// including ones added to `Diagnostic` later.
pub fn diagnostic_result_id(diagnostics: &[Diagnostic]) -> String {
    let mut item_hashes: Vec<u64> = diagnostics
        .iter()
        .map(|diagnostic| {
            let mut hasher = DefaultHasher::new();
            serde_json::to_string(diagnostic)
                .unwrap_or_default()
                .hash(&mut hasher);
            hasher.finish()
        })
        .collect();
    item_hashes.sort_unstable();

    let mut hasher = DefaultHasher::new();
    item_hashes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::diagnostic_result_id;
    use googletest::prelude::*;
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

    fn diagnostic(line: u32, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position { line, character: 0 },
                end: Position { line, character: 4 },
            },
            severity: Some(DiagnosticSeverity::WARNING),
            message: message.to_string(),
            ..Default::default()
        }
    }

    #[gtest]
    fn equal_sets_share_an_id_regardless_of_order() -> Result<()> {
        let forward = vec![diagnostic(1, "unused"), diagnostic(7, "unreachable")];
        let reversed = vec![diagnostic(7, "unreachable"), diagnostic(1, "unused")];

        verify_that!(
            diagnostic_result_id(&forward),
            eq(diagnostic_result_id(&reversed).as_str())
        )?;
        Ok(())
    }

    #[gtest]
    fn a_changed_set_gets_a_different_id() -> Result<()> {
        let before = vec![diagnostic(1, "unused")];
        let moved = vec![diagnostic(2, "unused")];
        let retexted = vec![diagnostic(1, "unused variable")];

        verify_that!(
            diagnostic_result_id(&before),
            not(eq(diagnostic_result_id(&moved).as_str()))
        )?;
        verify_that!(
            diagnostic_result_id(&before),
            not(eq(diagnostic_result_id(&retexted).as_str()))
        )?;
        verify_that!(
            diagnostic_result_id(&before),
            not(eq(diagnostic_result_id(&[]).as_str()))
        )?;
        Ok(())
    }
}
