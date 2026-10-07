#[cfg(test)]
mod test {
    use crate::VirtualWorkspace;
    use crate::db_index::derivation::DerivationKind;
    use crate::profile::work::{Work, counted};

    /// How many flow walks the file's local statements computed, and how many
    /// distinct walks those were.
    fn local_statement_walks(ws: &mut VirtualWorkspace, path: &str, text: &str) -> (u64, usize) {
        let (_, counts) = counted(|| ws.def_file(path, text));
        counts
            .get(&(Work::Flow, Some(DerivationKind::LocalStat)))
            .copied()
            .unwrap_or_default()
    }

    /// A field's walk starts from its member type, and is kept for that origin:
    /// every statement reading the field after a branch reuses the walks of the
    /// branch instead of walking them again.
    #[test]
    fn a_field_walk_from_its_member_type_is_shared_across_statements() {
        let mut ws = VirtualWorkspace::new();
        let reads = (0..8)
            .map(|read| format!("local read{read} = cfg.value\n"))
            .collect::<String>();
        let (computed, distinct) = local_statement_walks(
            &mut ws,
            "lua/autorun/field_walks.lua",
            &format!(
                "local cfg = {{}}\ncfg.value = 1\nif math.random() > 0.5 then\n    cfg.value = 2\nend\n{reads}"
            ),
        );
        assert!(distinct > 0, "the reads walked no flow");
        assert_eq!(
            computed, distinct as u64,
            "a walk from the field's member type was computed again by another statement"
        );
    }

    /// A file's diagnostics start from the memo its analysis left, so they
    /// infer less than diagnostics of the same file from scratch.
    #[test]
    fn diagnostics_start_from_the_memo_the_analysis_left() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/autorun/diagnosed.lua",
            "local cfg = { value = 1 }
local read = cfg.value
if read > 0 then
    print(read + cfg.value)
end
",
        );
        let inferred = |ws: &VirtualWorkspace| {
            let (_, counts) = counted(|| {
                ws.analysis
                    .diagnose_file(file_id, tokio_util::sync::CancellationToken::new())
            });
            counts
                .get(&(Work::Infer, Some(DerivationKind::Diagnostics)))
                .map_or(0, |(computed, _)| *computed)
        };

        let first = inferred(&ws);
        let again = inferred(&ws);

        assert!(
            first < again,
            "diagnostics inferred {first} with the analysis memo and {again} without"
        );
    }

    /// Diagnostics that take values from the analysis memo record every key
    /// those values were computed from: they read at least what diagnostics
    /// of the same file from scratch read.
    #[test]
    fn diagnostics_from_the_memo_read_what_diagnostics_from_scratch_read() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "lua/autorun/config.lua",
            "Config = { value = 1 }
",
        );
        let reader = ws.def_file(
            "lua/autorun/reader.lua",
            "local v = Config.value
local s = v + 1
print(s)
",
        );
        let read_keys = |ws: &VirtualWorkspace| {
            ws.analysis
                .diagnose_file(reader, tokio_util::sync::CancellationToken::new());
            ws.analysis
                .compilation
                .diagnostic_read_keys()
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
        };

        let from_memo = read_keys(&ws);
        let from_scratch = read_keys(&ws);

        let missing = from_scratch.difference(&from_memo).collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "reads missing with the memo: {missing:?}"
        );
    }

    /// Diagnostic reads filed a part at a time leave the same files stale
    /// after an edit as reads filed all at once.
    #[test]
    fn diagnostic_reads_filed_in_parts_leave_what_filing_them_whole_leaves() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.enable_full_diagnostic();
        ws.def_file(
            "lua/autorun/provider.lua",
            "Provider = {}
---@return string
function Provider.Describe() end
",
        );
        let readers = (0..4)
            .map(|reader| {
                ws.def_file(
                    &format!("lua/autorun/reader{reader}.lua"),
                    "local described = Provider.Describe()
print(described:upper())
",
                )
            })
            .collect::<Vec<_>>();
        for file_id in ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_all_file_ids()
        {
            ws.analysis
                .diagnose_file(file_id, tokio_util::sync::CancellationToken::new());
        }
        let stale_after_edit = |mut analysis: crate::EmmyLuaAnalysis| {
            let uri = ws.virtual_url_generator.new_uri("lua/autorun/provider.lua");
            let provider = analysis
                .update_file_text_only(
                    &uri,
                    "Provider = {}
---@return number
function Provider.Describe() end
"
                    .to_string(),
                )
                .expect("provider file id");
            analysis.reindex_files(vec![provider])
        };

        let mut in_parts = ws.analysis.fork();
        while in_parts.compilation.file_some_diagnostic_reads(1) {}
        let whole = stale_after_edit(ws.analysis.fork());
        let parts = stale_after_edit(in_parts);

        assert!(
            readers.iter().all(|reader| whole.contains(reader)),
            "an edit the readers read left {whole:?} stale"
        );
        assert_eq!(parts, whole);
    }
}
