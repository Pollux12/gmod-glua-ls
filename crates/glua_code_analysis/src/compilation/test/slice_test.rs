#[cfg(test)]
mod test {
    use crate::db_index::derivation::{self, AnalysisStage};
    use crate::{FileId, Slice, UpdateProgress, VirtualWorkspace};

    const CONFIG: &str = "lua/autorun/sh_config.lua";
    const SHOP: &str = "lua/autorun/server/sv_shop.lua";
    const LABEL: &str = "lua/autorun/client/cl_label.lua";

    /// Files that read each other's globals, members and a class, with a
    /// cycle between two of them.
    fn files() -> Vec<(&'static str, &'static str)> {
        vec![
            (
                CONFIG,
                r#"Config = Config or {}
Config.Money = 100
Config.Name = "shop"

---@class Shop
---@field price integer
Shop = {}

function Shop:Buy(count)
    return self.price * count
end
"#,
            ),
            (
                SHOP,
                r#"local shop = setmetatable({}, { __index = Shop })
shop.price = Config.Money
local total = shop:Buy(2)
Ledger = { total = total, back = Label }
"#,
            ),
            (
                LABEL,
                r#"local money = Config.Money
Label = { text = Config.Name .. money, ledger = Ledger }
local recorded = Ledger.total
"#,
            ),
        ]
    }

    /// `sh_config.lua` with its money a string and a line inserted above it,
    /// so the edit both moves positions and changes what its readers infer.
    const EDITED_CONFIG: &str = r#"Config = Config or {}
-- prices are text now
Config.Money = "100"
Config.Name = "shop"

---@class Shop
---@field price integer
Shop = {}

function Shop:Buy(count)
    return self.price * count
end
"#;

    const EDITED_LABEL: &str = r#"local money = Config.Money
Label = { text = money, ledger = Ledger }
local recorded = Ledger.back
"#;

    fn workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        ws.def_files(files());
        ws
    }

    /// A cold build of `files` with `edits` applied.
    fn cold(edits: &[(&str, &str)]) -> Vec<String> {
        let mut files = files();
        for (path, text) in edits {
            files
                .iter_mut()
                .find(|(existing, _)| existing == path)
                .expect("an edited file is in the workspace")
                .1 = text;
        }
        let mut ws = VirtualWorkspace::new();
        ws.def_files(files);
        ws.settled_digest()
    }

    /// Asserts `ws` settled to `expected` and left no derivation stale or
    /// naming a node its file no longer has.
    fn assert_settled(ws: &VirtualWorkspace, expected: &[String], context: &str) {
        assert_eq!(ws.settled_digest(), expected, "{context}");
        assert_eq!(ws.stale_derivations(), [], "{context}");
        assert_eq!(ws.phantom_derivations(), [], "{context}");
    }

    /// Sets `path`'s text without re-analysing it.
    fn stage_text(ws: &mut VirtualWorkspace, path: &str, text: &str) -> FileId {
        let uri = ws.virtual_url_generator.new_uri(path);
        ws.analysis
            .update_file_text_only(&uri, text.to_string())
            .expect("a workspace file")
    }

    /// Advances the re-analysis in progress in slices of `evaluations`,
    /// calling `between` after each slice that left work. Returns how many
    /// slices left work.
    fn settle(
        ws: &mut VirtualWorkspace,
        evaluations: usize,
        mut between: impl FnMut(&mut VirtualWorkspace),
    ) -> usize {
        let mut paused = 0;
        loop {
            match ws.analysis.advance_reindex(Slice::Evaluations(evaluations)) {
                UpdateProgress::Settled(_) => return paused,
                UpdateProgress::Settling => {
                    paused += 1;
                    between(ws);
                }
                UpdateProgress::TextChanged => panic!("no text changed"),
            }
        }
    }

    #[test]
    fn an_edit_settled_one_evaluation_at_a_time_matches_a_cold_build() {
        let mut ws = workspace();
        let config = stage_text(&mut ws, CONFIG, EDITED_CONFIG);
        ws.analysis.begin_reindex(vec![config]);

        let paused = settle(&mut ws, 1, |_| {});

        assert!(paused > 0, "the edit settled in one slice");
        assert_settled(&ws, &cold(&[(CONFIG, EDITED_CONFIG)]), "one at a time");
    }

    #[test]
    fn between_slices_no_evaluation_is_open_and_the_index_reads_as_settled() {
        let mut ws = workspace();
        let config = stage_text(&mut ws, CONFIG, EDITED_CONFIG);
        ws.analysis.begin_reindex(vec![config]);
        let mut boundaries = Vec::new();

        settle(&mut ws, 1, |ws| {
            let db = ws.analysis.compilation.get_db();
            boundaries.push((
                db.analysis_stage(),
                derivation::recording(),
                derivation::keys_lent(),
                db.get_type_index().inference_derived_state_is_stale(),
            ));
        });

        assert!(!boundaries.is_empty());
        assert!(
            boundaries
                .iter()
                .all(|boundary| *boundary == (AnalysisStage::Closed, false, false, false)),
            "{boundaries:?}"
        );
    }

    #[test]
    fn a_second_edit_of_the_same_file_taken_mid_reanalysis_matches_a_cold_build() {
        let expected = cold(&[(CONFIG, EDITED_CONFIG)]);
        for taken_after in [0, 1, 2, 5, 10, 20, 40] {
            let mut ws = workspace();
            let config = stage_text(&mut ws, CONFIG, &EDITED_CONFIG.replace("\"100\"", "true"));
            ws.analysis.begin_reindex(vec![config]);
            ws.analysis.advance_reindex(Slice::Evaluations(taken_after));
            stage_text(&mut ws, CONFIG, EDITED_CONFIG);
            ws.analysis.begin_reindex(vec![config]);
            settle(&mut ws, 3, |_| {});

            assert_settled(&ws, &expected, &format!("taken after {taken_after}"));
        }
    }

    #[test]
    fn an_edit_of_another_file_taken_mid_reanalysis_matches_a_cold_build() {
        let expected = cold(&[(CONFIG, EDITED_CONFIG), (LABEL, EDITED_LABEL)]);
        for taken_after in [0, 1, 3, 8, 16, 32] {
            let mut ws = workspace();
            let config = stage_text(&mut ws, CONFIG, EDITED_CONFIG);
            ws.analysis.begin_reindex(vec![config]);
            ws.analysis.advance_reindex(Slice::Evaluations(taken_after));
            let label = stage_text(&mut ws, LABEL, EDITED_LABEL);
            ws.analysis.begin_reindex(vec![label]);
            settle(&mut ws, 2, |_| {});

            assert_settled(&ws, &expected, &format!("taken after {taken_after}"));
        }
    }

    const ADAPTER: &str = "lua/autorun/sh_adapter.lua";
    const USER: &str = "lua/autorun/client/cl_user.lua";

    /// A class whose method returns a shape a reader in another file indexes
    /// past, so the reader's diagnostics follow the class file.
    fn adapter_files() -> Vec<(&'static str, &'static str)> {
        vec![
            (
                ADAPTER,
                r#"---@class AdapterState
---@field warning string

---@class Adapter
Adapter = {}

---@return AdapterState
function Adapter.GetState()
    return { warning = "off" }
end
"#,
            ),
            (
                USER,
                r#"local state = Adapter.GetState()
local warning = state.warning
local directional = state.directional
"#,
            ),
        ]
    }

    /// The diagnostics of each file at `paths` that exists, by path, as a
    /// client shows them.
    fn diagnose_paths(ws: &VirtualWorkspace, paths: &[&str]) -> Vec<(String, Vec<String>)> {
        paths
            .iter()
            .filter_map(|path| {
                let uri = ws.virtual_url_generator.new_uri(path);
                let file_id = ws.analysis.get_file_id(&uri)?;
                ws.analysis
                    .compilation
                    .get_db()
                    .get_vfs()
                    .get_syntax_tree(&file_id)?;
                Some((path.to_string(), diagnose(ws, file_id)))
            })
            .collect()
    }

    fn diagnose(ws: &VirtualWorkspace, file_id: FileId) -> Vec<String> {
        let mut shown = ws
            .analysis
            .diagnose_file(file_id, tokio_util::sync::CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .map(|diagnostic| {
                format!(
                    "{:?} {:?} {}",
                    diagnostic.code, diagnostic.range, diagnostic.message
                )
            })
            .collect::<Vec<_>>();
        shown.sort();
        shown
    }

    /// A class file deleted and created again through watched-file events,
    /// settled in slices while a client diagnoses every file once the first
    /// slice is in and each reported one once it settles, leaves the client
    /// showing what a cold build reports.
    #[test]
    fn diagnostics_pulled_between_slices_of_a_delete_and_recreate_match_a_cold_build() {
        let paths = [ADAPTER, USER];
        let slices = [
            (Slice::Evaluations(1), Slice::Whole),
            (Slice::Evaluations(2), Slice::Whole),
            (Slice::Whole, Slice::Evaluations(1)),
            (Slice::Evaluations(1), Slice::Evaluations(1)),
            (Slice::Evaluations(3), Slice::Evaluations(2)),
        ];
        for (delete_slice, recreate_slice) in slices {
            let mut ws = VirtualWorkspace::new();
            ws.def_files(adapter_files());
            let mut expected = diagnose_paths(&ws, &paths);
            expected.sort();
            let mut shown = expected
                .iter()
                .cloned()
                .collect::<std::collections::BTreeMap<_, _>>();
            let uri = ws.virtual_url_generator.new_uri(ADAPTER);
            for (text, slice) in [
                (None, delete_slice),
                (Some(adapter_files()[0].1.to_string()), recreate_slice),
            ] {
                let staged = ws
                    .analysis
                    .stage_file_system_changes(vec![(uri.clone(), text)]);
                ws.analysis.begin_reindex(staged.clone());
                let mut pulled = false;
                loop {
                    match ws.analysis.advance_reindex(slice) {
                        UpdateProgress::Settled(affected) => {
                            for file_id in affected.into_iter().chain(staged.iter().copied()) {
                                let Some(path) = paths.iter().find(|path| {
                                    ws.analysis
                                        .get_file_id(&ws.virtual_url_generator.new_uri(path))
                                        == Some(file_id)
                                }) else {
                                    continue;
                                };
                                shown.remove(*path);
                                shown.extend(diagnose_paths(&ws, &[path]));
                            }
                            break;
                        }
                        UpdateProgress::Settling if !pulled => {
                            pulled = true;
                            shown = diagnose_paths(&ws, &paths).into_iter().collect();
                        }
                        UpdateProgress::Settling => {}
                        UpdateProgress::TextChanged => panic!("no text changed"),
                    }
                }
            }

            assert_eq!(
                shown.into_iter().collect::<Vec<_>>(),
                expected,
                "deleted in {delete_slice:?}, recreated in {recreate_slice:?}"
            );
        }
    }

    /// Diagnostics taken between two slices read the index as it stood then:
    /// a fact they read that a later slice moves reports the file.
    #[test]
    fn a_file_diagnosed_between_slices_is_reported_when_a_later_slice_moves_its_reads() {
        let mut ws = workspace();
        let label = {
            let uri = ws.virtual_url_generator.new_uri(LABEL);
            ws.analysis.get_file_id(&uri).expect("a workspace file")
        };
        let config = stage_text(&mut ws, CONFIG, EDITED_CONFIG);
        ws.analysis.begin_reindex(vec![config]);
        ws.analysis.advance_reindex(Slice::Evaluations(1));

        ws.analysis
            .diagnose_file(label, tokio_util::sync::CancellationToken::new());
        let affected = loop {
            if let UpdateProgress::Settled(affected) =
                ws.analysis.advance_reindex(Slice::Evaluations(1))
            {
                break affected;
            }
        };

        assert!(affected.contains(&label), "{affected:?}");
    }

    /// A file still settling ahead of the rest keeps doing so when another
    /// file's edit is taken in: its document must not answer half evaluated.
    #[test]
    fn a_file_settling_first_keeps_settling_first_when_another_edit_is_taken_in() {
        let mut ws = workspace();
        let config = stage_text(&mut ws, CONFIG, EDITED_CONFIG);
        ws.analysis.begin_reindex(vec![config]);
        ws.analysis.advance_reindex(Slice::Evaluations(1));
        assert!(ws.analysis.compilation.settles_first(config));

        let label = stage_text(&mut ws, LABEL, EDITED_LABEL);
        ws.analysis.begin_reindex(vec![label]);

        assert!(ws.analysis.compilation.settles_first(config));
    }

    /// A slice ends as soon as the edited file settles, so its document
    /// answers before the work waiting on it is checked.
    #[test]
    fn a_slice_hands_back_once_the_edited_file_settles() {
        let mut ws = workspace();
        let config = stage_text(&mut ws, CONFIG, EDITED_CONFIG);
        ws.analysis.begin_reindex(vec![config]);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
        let progress = ws.analysis.advance_reindex(Slice::Until(deadline));

        assert!(matches!(progress, UpdateProgress::Settling));
        assert!(!ws.analysis.compilation.settles_first(config));
        settle(&mut ws, 1, |_| {});
        assert_settled(&ws, &cold(&[(CONFIG, EDITED_CONFIG)]), "handed back");
    }

    #[test]
    fn taking_no_files_mid_reanalysis_still_settles_the_work_queued() {
        let mut ws = workspace();
        let config = stage_text(&mut ws, CONFIG, EDITED_CONFIG);
        ws.analysis.begin_reindex(vec![config]);
        ws.analysis.advance_reindex(Slice::Evaluations(2));

        ws.analysis.begin_reindex(Vec::new());
        settle(&mut ws, 2, |_| {});

        assert_settled(&ws, &cold(&[(CONFIG, EDITED_CONFIG)]), "no files taken");
    }

    #[test]
    fn a_comment_typed_mid_reanalysis_re_analyses_no_other_file() {
        let config_text = files()[0].1;
        for taken_after in [0, 1, 3, 8] {
            let mut ws = workspace();
            let config = stage_text(&mut ws, CONFIG, &format!("-- one\n{config_text}"));
            ws.analysis.begin_reindex(vec![config]);
            ws.analysis.advance_reindex(Slice::Evaluations(taken_after));
            stage_text(&mut ws, CONFIG, &format!("-- one\n-- two\n{config_text}"));
            ws.analysis.begin_reindex(vec![config]);
            settle(&mut ws, 1, |_| {});

            assert_eq!(
                ws.analysis.compilation.last_evaluated_files(),
                [config],
                "taken after {taken_after}"
            );
        }
    }

    #[test]
    fn prioritising_a_different_file_each_slice_matches_a_cold_build() {
        let mut ws = workspace();
        let files = [CONFIG, SHOP, LABEL].map(|path| {
            let uri = ws.virtual_url_generator.new_uri(path);
            ws.analysis.get_file_id(&uri).expect("a workspace file")
        });
        let config = stage_text(&mut ws, CONFIG, EDITED_CONFIG);
        ws.analysis.begin_reindex(vec![config]);
        let mut next = 0;

        settle(&mut ws, 1, |ws| {
            ws.analysis
                .compilation
                .prioritise_update(&[files[next % files.len()]]);
            next += 1;
        });

        assert_settled(&ws, &cold(&[(CONFIG, EDITED_CONFIG)]), "rotating priority");
    }

    #[test]
    fn slices_on_fresh_threads_match_a_cold_build() {
        let mut ws = workspace();
        let config = stage_text(&mut ws, CONFIG, EDITED_CONFIG);
        ws.analysis.begin_reindex(vec![config]);

        loop {
            let progress = std::thread::scope(|scope| {
                std::thread::Builder::new()
                    .stack_size(crate::ANALYSIS_STACK_SIZE)
                    .spawn_scoped(scope, || ws.analysis.advance_reindex(Slice::Evaluations(2)))
                    .expect("slice thread")
                    .join()
                    .expect("a slice on a fresh thread")
            });
            if matches!(progress, UpdateProgress::Settled(_)) {
                break;
            }
        }

        assert_settled(&ws, &cold(&[(CONFIG, EDITED_CONFIG)]), "fresh threads");
    }

    #[test]
    fn a_reanalysis_evaluates_nothing_while_the_vfs_holds_text_it_has_not_taken() {
        let mut ws = workspace();
        let config = stage_text(&mut ws, CONFIG, EDITED_CONFIG);
        ws.analysis.begin_reindex(vec![config]);
        stage_text(&mut ws, LABEL, EDITED_LABEL);

        let progress = ws.analysis.advance_reindex(Slice::Whole);

        assert_eq!(progress, UpdateProgress::TextChanged);
        assert!(ws.analysis.compilation.is_updating());
    }
}
