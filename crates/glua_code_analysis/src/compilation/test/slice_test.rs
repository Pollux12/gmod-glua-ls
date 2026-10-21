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

---@generic T
---@param name `T`
---@return T
function MakeLabel(name)
    return nil
end
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

    /// `cl_label.lua` cut to far fewer bytes than its positions reach.
    const SHORT_LABEL: &str = "Label = {}\n";

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

    /// A file a request settles first after the edited file handed back
    /// answers before the rest its settling releases is checked.
    #[test]
    fn a_slice_stops_once_a_requested_file_settles_and_releases_the_rest() {
        let mut ws = workspace();
        let config = stage_text(&mut ws, CONFIG, EDITED_CONFIG);
        ws.analysis.begin_reindex(vec![config]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
        ws.analysis.advance_reindex(Slice::Until(deadline));
        let shop = ws
            .analysis
            .get_file_id(&ws.virtual_url_generator.new_uri(SHOP))
            .expect("a workspace file");
        assert!(ws.analysis.compilation.is_settling(shop));
        ws.analysis.compilation.prioritise_update(&[shop]);

        let progress = ws.analysis.advance_reindex(Slice::Until(deadline));

        assert!(matches!(progress, UpdateProgress::Settling));
        assert!(!ws.analysis.compilation.settles_first(shop));
        settle(&mut ws, 1, |_| {});
        assert_settled(
            &ws,
            &cold(&[(CONFIG, EDITED_CONFIG)]),
            "requested, then released",
        );
    }

    /// An edit waiting to be taken in stops the slice in progress; the run
    /// goes on once it is taken.
    #[test]
    fn a_waiting_edit_stops_the_slice_in_progress() {
        let mut ws = workspace();
        let config = stage_text(&mut ws, CONFIG, EDITED_CONFIG);
        ws.analysis.begin_reindex(vec![config]);
        let interrupt = ws.analysis.compilation.interrupt_handle();
        interrupt.store(true, std::sync::atomic::Ordering::Release);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
        let progress = ws.analysis.advance_reindex(Slice::Until(deadline));

        assert!(matches!(progress, UpdateProgress::Settling));
        assert!(ws.analysis.compilation.settles_first(config));
        interrupt.store(false, std::sync::atomic::Ordering::Release);
        settle(&mut ws, 1, |_| {});
        assert_settled(&ws, &cold(&[(CONFIG, EDITED_CONFIG)]), "interrupted");
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

    /// Taking no file mid-run changes nothing: the run enters the same
    /// stages and evaluates as much as it does without the take.
    #[test]
    fn taking_no_files_mid_reanalysis_leaves_the_run_as_it_was() {
        let run = |taken_after: usize, take_nothing: bool| {
            let mut ws = workspace();
            let config = stage_text(&mut ws, CONFIG, EDITED_CONFIG);
            ws.analysis.begin_reindex(vec![config]);
            ws.analysis.advance_reindex(Slice::Evaluations(taken_after));
            if take_nothing {
                ws.analysis.begin_reindex(Vec::new());
            }
            settle(&mut ws, 1, |_| {});
            let stats = ws.analysis.compilation.last_stats();
            (stats.stages().to_vec(), stats.total_evaluations())
        };
        for taken_after in [0, 1, 2, 5, 10, 20, 40, 80] {
            assert_eq!(
                run(taken_after, true),
                run(taken_after, false),
                "no files taken after {taken_after}"
            );
        }
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

    /// A take also takes text staged before it that it was not given, so no
    /// slice reads the index against text it was not built from.
    #[test]
    fn a_take_takes_every_file_whose_text_changed() {
        let mut ws = workspace();
        let config = stage_text(&mut ws, CONFIG, EDITED_CONFIG);
        let label = stage_text(&mut ws, LABEL, SHORT_LABEL);
        ws.analysis.begin_reindex(vec![config]);

        assert!(ws.analysis.compilation.has_taken_text(label));
        settle(&mut ws, 1, |_| {});
        assert_settled(
            &ws,
            &cold(&[(CONFIG, EDITED_CONFIG), (LABEL, SHORT_LABEL)]),
            "text the take was not given",
        );
    }

    /// A file an earlier take already took is not rebuilt when a later take
    /// names it.
    #[test]
    fn a_file_already_taken_is_not_taken_again() {
        let evaluations = |name_label_again: bool| {
            let mut ws = workspace();
            let config = stage_text(&mut ws, CONFIG, EDITED_CONFIG);
            let label = stage_text(&mut ws, LABEL, SHORT_LABEL);
            ws.analysis.begin_reindex(vec![config]);
            if name_label_again {
                ws.analysis.begin_reindex(vec![label]);
            }
            settle(&mut ws, 1, |_| {});
            ws.analysis.compilation.last_stats().total_evaluations()
        };

        assert_eq!(evaluations(true), evaluations(false));
    }

    /// A take that lands before the previous take's pass opened, its file's
    /// text already shorter, settles as a cold build of both edits.
    #[test]
    fn a_take_onto_a_pass_not_yet_opened_matches_a_cold_build() {
        let mut ws = workspace();
        let config = stage_text(&mut ws, CONFIG, EDITED_CONFIG);
        ws.analysis.begin_reindex(vec![config]);
        let label = stage_text(&mut ws, LABEL, SHORT_LABEL);
        assert_eq!(
            ws.analysis.advance_reindex(Slice::Whole),
            UpdateProgress::TextChanged
        );

        ws.analysis.begin_reindex(vec![label]);
        settle(&mut ws, 1, |_| {});
        assert_settled(
            &ws,
            &cold(&[(CONFIG, EDITED_CONFIG), (LABEL, SHORT_LABEL)]),
            "take onto an unopened pass",
        );
    }

    const FOO: &str = "lua/autorun/sh_foo.lua";
    const FOO_USER: &str = "lua/autorun/server/sv_foo_user.lua";
    const FOO_CLASS: &str = "lua/autorun/sh_foo_class.lua";
    const FOO_CALL: &str = "Foo:Bar(1)\n";

    /// `Foo:Bar` declared after `padding` lines of fields.
    fn foo_text(padding: usize) -> String {
        let mut text = String::from("---@class Foo\nFoo = {}\n");
        for line in 0..padding {
            text.push_str(&format!("Foo.Pad{line} = {line}\n"));
        }
        text.push_str("\nfunction Foo:Bar(count)\n    return count\nend\n");
        text
    }

    /// The signature of the first closure in `path`.
    fn first_closure_signature(ws: &VirtualWorkspace, path: &str) -> crate::LuaSignatureId {
        use glua_parser::{LuaAstNode, LuaClosureExpr};
        let uri = ws.virtual_url_generator.new_uri(path);
        let file_id = ws.analysis.get_file_id(&uri).expect("a workspace file");
        let closure = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("a live file")
            .get_chunk_node()
            .descendants::<LuaClosureExpr>()
            .next()
            .expect("a closure");
        crate::LuaSignatureId::new(file_id, closure.get_position())
    }

    /// Asserts a derivation in another file reads `signature`'s receiver
    /// authority, so a re-analysis checks that read.
    fn assert_receiver_authority_read(ws: &VirtualWorkspace, signature: crate::LuaSignatureId) {
        let key = derivation::HashedKey::new(derivation::FactKey::ReceiverAuthority(signature));
        let graph = ws.analysis.compilation.get_db().reader_graph();
        assert!(
            graph
                .stale_readers_seen(&key, |_| false)
                .any(|(reader, _)| reader.file_id != signature.get_file_id()),
            "another file reads the receiver authority of {signature:?}"
        );
    }

    /// `sh_foo.lua` with `Foo:Bar` far down it, called from another file,
    /// which reads the method's receiver authority.
    fn foo_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        ws.def_files(vec![(FOO, &foo_text(40)), (FOO_USER, FOO_CALL)]);
        assert_receiver_authority_read(&ws, first_closure_signature(&ws, FOO));
        ws
    }

    /// A cold build of [`foo_workspace`] with `sh_foo.lua` holding `text`.
    fn cold_foo(text: &str) -> Vec<String> {
        let mut ws = VirtualWorkspace::new();
        ws.def_files(vec![(FOO, text), (FOO_USER, FOO_CALL)]);
        ws.settled_digest()
    }

    /// A take landing before the previous take's pass opened checks reads of
    /// a method's receiver authority at the position the previous take moved
    /// it to, which the newer, shorter text no longer reaches.
    #[test]
    fn a_take_onto_a_pass_whose_moved_method_lies_past_the_newer_text_matches_a_cold_build() {
        let mut ws = foo_workspace();
        let foo = stage_text(&mut ws, FOO, &foo_text(20));
        ws.analysis.begin_reindex(vec![foo]);
        stage_text(&mut ws, FOO, &foo_text(0));

        ws.analysis.begin_reindex(vec![foo]);
        settle(&mut ws, 1, |_| {});

        assert_settled(&ws, &cold_foo(&foo_text(0)), "two takes, one pass");
    }

    /// A pass opening after an edit deleted a method checks reads of its
    /// receiver authority at a position past the end of the shorter text.
    #[test]
    fn a_pass_opening_after_a_method_was_cut_from_a_file_matches_a_cold_build() {
        let mut ws = foo_workspace();
        let short = "---@class Foo\nFoo = {}\n";
        let foo = stage_text(&mut ws, FOO, short);

        ws.analysis.begin_reindex(vec![foo]);
        settle(&mut ws, 1, |_| {});

        assert_settled(&ws, &cold_foo(short), "method cut");
    }

    /// `Foo:Bar` far down a file that does not declare `Foo`, called with an
    /// explicit receiver from another file.
    fn split_foo_files(foo_padding: usize, class: &str) -> Vec<(&'static str, String)> {
        let mut foo = String::new();
        for line in 0..foo_padding {
            foo.push_str(&format!("local pad{line} = {line}\n"));
        }
        foo.push_str("\nfunction Foo:Bar(count)\n    return count\nend\n");
        vec![
            (FOO_CLASS, class.to_string()),
            (FOO, foo),
            (FOO_USER, "Foo.Bar({}, 1)\n".to_string()),
        ]
    }

    /// A receiver authority recomputed between slices moves under an edit
    /// that then shifts its method: its readers are checked where it moved.
    #[test]
    fn a_receiver_authority_recomputed_between_slices_follows_a_later_edit_of_its_method() {
        let class = "---@class Foo\nFoo = {}\n";
        let unclassed = "Foo = {}\n";
        let mut ws = VirtualWorkspace::new();
        let files = split_foo_files(20, class);
        ws.def_files(
            files
                .iter()
                .map(|(path, text)| (*path, text.as_str()))
                .collect(),
        );
        let signature = first_closure_signature(&ws, FOO);
        assert_receiver_authority_read(&ws, signature);
        let db = ws.analysis.compilation.get_db();
        let before = *db.queries().receiver_authority.get(db, signature);

        let class_file = stage_text(&mut ws, FOO_CLASS, unclassed);
        ws.analysis.begin_reindex(vec![class_file]);
        ws.analysis.advance_reindex(Slice::Evaluations(1));
        let db = ws.analysis.compilation.get_db();
        let computed = db.queries().receiver_authority.stats().computed;
        let between = *db.queries().receiver_authority.get(db, signature);
        assert_eq!(
            db.queries().receiver_authority.stats().computed,
            computed + 1,
            "recomputed between slices"
        );
        assert_ne!(
            before, between,
            "the class edit moves the receiver authority"
        );
        let shifted = split_foo_files(40, unclassed);
        let foo = stage_text(&mut ws, FOO, &shifted[1].1);
        ws.analysis.begin_reindex(vec![foo]);
        settle(&mut ws, 1, |_| {});

        let mut cold = VirtualWorkspace::new();
        cold.def_files(
            shifted
                .iter()
                .map(|(path, text)| (*path, text.as_str()))
                .collect(),
        );
        assert_settled(&ws, &cold.settled_digest(), "recomputed, then shifted");
    }
}
