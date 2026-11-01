//! A file's source tail (flow tree and its errors, function summaries, role
//! mentions) is computed for a batch of files at once on the analysis pool.
//! Whatever the thread count, and across edits, deletes, re-creates and
//! reopens and shifts, every fact it publishes must match a cold build on
//! one thread.
#[cfg(test)]
mod test {
    use crate::compilation::analyzer::parallel;
    use crate::db_index::derivation::{ABSENT, FactKey};
    use crate::{Emmyrc, VirtualWorkspace};

    const CONST_AND_MUTABLE: &str = r#"
        local limit = 10
        local count = 0
        count = count + limit
        local function bump() count = count + 1 end
        if limit > count then bump() end
        local label = limit > 5 and "big" or nil
        if label then print(label, count) end
    "#;

    const EXPLICIT_SELF: &str = r#"
        local Obj = {}
        function Obj.Shadow(self, value)
            local self = {}
            self.value = value
            return self
        end
        function Obj.Nested(self)
            local function reset() self = nil end
            reset()
            return self
        end
        function Obj.Keep(self, other)
            other = self
            return self
        end
        return Obj
    "#;

    const IMPLICIT_SELF: &str = r#"
        local Meta = {}
        function Meta:Rebind(other)
            self = other
            return self
        end
        function Meta:Chain() return self end
        function Meta:Write(key, value) self[key] = value end
        return Meta
    "#;

    const GOTO: &str = r#"
        for i = 1, 3 do
            if i == 2 then goto continue end
            print(i)
            ::continue::
        end
        goto missing
        break
    "#;

    const FIELD_SETTER: &str = r#"
        local function set(tbl, key, value) tbl[key] = value end
        local Store = {}
        set(Store, "name", "x")
        local function nested(tbl, key)
            local function inner() tbl[key] = true end
            inner()
        end
        nested(Store, "flag")
        print(Store.name, Store.flag)
    "#;

    const ROLE_MENTIONS: &str = r#"
        local net = { Start = function() end }
        net.Start("local_message")
        util.AddNetworkString("global_message")
        hook.Add("Think", "source_tail", function() end)
        local hook = hook
        hook.Run("source_tail")
        timer.Simple(1, function() print(SOURCE_TAIL_GLOBAL.value) end)
    "#;

    const NO_CLOSURES: &str = r#"
        print("no functions here")
    "#;

    const SPLIT_CLASS_A: &str = r#"
        ---@class SourceTailSplit
        SourceTailSplit = SourceTailSplit or {}
        ---@return number
        function SourceTailSplit:A() return 1 end
        local helper = function(self) return self end
        SourceTailSplit.Helper = helper
    "#;

    const SPLIT_CLASS_B: &str = r#"
        ---@class SourceTailSplit
        local S = SourceTailSplit
        function S:B() return self:A() + 1 end
        local value = S:B()
        print(value)
    "#;

    const FILLER: &str = r#"
        local total = 0
        for _, value in ipairs({ 1, 2, 3 }) do total = total + value end
        print(total)
    "#;

    fn files() -> Vec<(&'static str, &'static str)> {
        vec![
            ("lua/autorun/sh_const_mutable.lua", CONST_AND_MUTABLE),
            ("lua/autorun/sh_explicit_self.lua", EXPLICIT_SELF),
            ("lua/autorun/sh_implicit_self.lua", IMPLICIT_SELF),
            ("lua/autorun/sh_goto.lua", GOTO),
            ("lua/autorun/sh_field_setter.lua", FIELD_SETTER),
            ("lua/autorun/server/sv_role_mentions.lua", ROLE_MENTIONS),
            ("lua/autorun/sh_no_closures.lua", NO_CLOSURES),
            ("lua/autorun/sh_split_a.lua", SPLIT_CLASS_A),
            ("lua/autorun/sh_split_b.lua", SPLIT_CLASS_B),
            ("lua/autorun/sh_filler_a.lua", FILLER),
            ("lua/autorun/sh_filler_b.lua", FILLER),
        ]
    }

    fn workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.enable_full_diagnostic();
        // A flow tree's fingerprint names its file, so every workspace gives
        // each path the same id whichever files it holds.
        for (path, _) in files() {
            let uri = ws.virtual_url_generator.new_uri(path);
            ws.get_db_mut().get_vfs_mut().file_id(&uri);
        }
        ws
    }

    /// The semantic digest plus, per main-workspace file by path, what its
    /// source tail published and the analyzer errors it holds.
    fn tail_digest(ws: &VirtualWorkspace) -> Vec<String> {
        let mut digest = ws.semantic_digest();
        let db = ws.analysis.compilation.get_db();
        let mut files = db
            .get_module_index()
            .get_main_workspace_file_ids()
            .into_iter()
            .map(|file_id| {
                let path = db
                    .get_vfs()
                    .get_file_path(&file_id)
                    .map(|path| path.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                (path, file_id)
            })
            .collect::<Vec<_>>();
        files.sort();
        for (path, file_id) in files {
            digest.push(format!(
                "{path} flow={:?} summaries={:?} mentions={:?} {:?} errors={:?}",
                db.fact_fingerprint(&FactKey::FlowTree(file_id)),
                db.fact_fingerprint(&FactKey::FunctionSummaries(file_id)),
                db.fact_fingerprint(&FactKey::RoleMentions(file_id)),
                db.role_mentions(file_id),
                db.get_diagnostic_index().get_diagnostics(&file_id),
            ));
        }
        digest
    }

    /// [`tail_digest`] plus the settled digest and every derivation's
    /// recorded reads and writes.
    fn recorded_digest(ws: &VirtualWorkspace) -> Vec<String> {
        let mut digest = tail_digest(ws);
        digest.extend(ws.settled_digest());
        let graph = ws.analysis.compilation.get_db().reader_graph();
        let mut ids = graph.derivations().collect::<Vec<_>>();
        ids.sort();
        for id in ids {
            let mut reads = graph
                .reads_of(id)
                .zip(graph.recorded_slots(id))
                .map(|(key, (_, seen))| format!("{:?}={seen:?}", key.key()))
                .collect::<Vec<_>>();
            reads.sort();
            let mut outputs = graph
                .outputs_of(id)
                .map(|key| format!("{:?}", key.key()))
                .collect::<Vec<_>>();
            outputs.sort();
            digest.push(format!("{id:?} reads {reads:?} writes {outputs:?}"));
        }
        digest
    }

    /// A cold one-thread build of `texts`. A flow tree's fingerprint names
    /// its file, so each path takes the id it has in `warm`.
    fn cold_digest(warm: &VirtualWorkspace, texts: &[(&str, &str)]) -> Vec<String> {
        parallel::set_analysis_threads(1);
        let mut cold = workspace();
        let mut ids = texts
            .iter()
            .map(|(path, _)| {
                let uri = warm.virtual_url_generator.new_uri(path);
                (warm.analysis.get_file_id(&uri).expect("warm file id"), uri)
            })
            .collect::<Vec<_>>();
        ids.sort_by_key(|(id, _)| *id);
        let vfs = cold.get_db_mut().get_vfs_mut();
        for (id, uri) in ids {
            while vfs.file_id(&uri) != id {
                assert!(vfs.file_id(&uri).id < id.id, "{uri:?} took a later id");
                vfs.remove_file(&uri);
            }
        }
        cold.def_files(texts.to_vec());
        tail_digest(&cold)
    }

    fn with_threads<T>(f: impl FnOnce() -> T) -> T {
        let threads = parallel::THREADS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let out = f();
        parallel::set_analysis_threads(0);
        drop(threads);
        out
    }

    /// An empty summary is never stored; mentions are stored even when empty.
    fn assert_empty_and_absent(ws: &VirtualWorkspace) {
        let db = ws.analysis.compilation.get_db();
        let file = |path: &str| {
            ws.analysis
                .get_file_id(&ws.virtual_url_generator.new_uri(path))
                .expect("file id")
        };
        let summaries = |path: &str| db.fact_fingerprint(&FactKey::FunctionSummaries(file(path)));
        let mentions = |path: &str| db.fact_fingerprint(&FactKey::RoleMentions(file(path)));
        assert_eq!(summaries("lua/autorun/sh_no_closures.lua"), ABSENT);
        assert_ne!(summaries("lua/autorun/sh_field_setter.lua"), ABSENT);
        assert_ne!(summaries("lua/autorun/sh_implicit_self.lua"), ABSENT);
        let implicit_self = file("lua/autorun/sh_implicit_self.lua");
        assert_ne!(mentions("lua/autorun/sh_implicit_self.lua"), ABSENT);
        assert!(db.role_mentions(implicit_self).is_empty());
        let role_mentions = db.role_mentions(file("lua/autorun/server/sv_role_mentions.lua"));
        assert!(
            role_mentions
                .iter()
                .any(|path| path == "util.AddNetworkString")
        );
        assert!(!role_mentions.iter().any(|path| path == "net.Start"));
        assert!(role_mentions.iter().any(|path| path == "hook.Run"));
    }

    #[test]
    fn source_tails_match_across_thread_counts_cold() {
        let digests = with_threads(|| {
            [1, 4, 16].map(|threads| {
                parallel::set_analysis_threads(threads);
                let mut ws = workspace();
                ws.def_files(files());
                assert_empty_and_absent(&ws);
                recorded_digest(&ws)
            })
        });
        assert!(
            digests[0]
                .iter()
                .any(|line| line.contains("sh_goto.lua") && line.contains("SyntaxError")),
            "the goto fixture reports its flow errors"
        );
        assert_eq!(digests[0], digests[1]);
        assert_eq!(digests[0], digests[2]);
    }

    #[test]
    fn source_tails_match_cold_across_edit_delete_recreate_reopen_and_shift() {
        with_threads(|| {
            let mut texts = files();
            parallel::set_analysis_threads(4);
            let mut ws = workspace();
            ws.def_files(texts.clone());
            let check = |ws: &VirtualWorkspace, texts: &[(&str, &str)], step: &str| {
                let warm = tail_digest(ws);
                assert_eq!(ws.stale_derivations(), Vec::new(), "{step}: stale");
                assert_eq!(ws.phantom_derivations(), Vec::new(), "{step}: phantom");
                let cold = cold_digest(ws, texts);
                parallel::set_analysis_threads(4);
                assert_eq!(warm, cold, "{step}");
            };
            check(&ws, &texts, "cold");

            // Every file at once: summaries go empty and nonempty, mentions
            // switch between local and global.
            let swap = |path: &str| match path {
                "lua/autorun/sh_field_setter.lua" => NO_CLOSURES,
                "lua/autorun/sh_no_closures.lua" => FIELD_SETTER,
                "lua/autorun/sh_explicit_self.lua" => IMPLICIT_SELF,
                "lua/autorun/sh_implicit_self.lua" => EXPLICIT_SELF,
                "lua/autorun/server/sv_role_mentions.lua" => CONST_AND_MUTABLE,
                "lua/autorun/sh_const_mutable.lua" => ROLE_MENTIONS,
                "lua/autorun/sh_filler_a.lua" => GOTO,
                "lua/autorun/sh_goto.lua" => FILLER,
                _ => FILLER,
            };
            for (path, text) in texts.iter_mut() {
                *text = swap(path);
            }
            ws.def_files(texts.clone());
            check(&ws, &texts, "batch edit");

            // One file inline: its summaries go back to nonempty.
            ws.def_file("lua/autorun/sh_field_setter.lua", FIELD_SETTER);
            texts
                .iter_mut()
                .find(|(path, _)| *path == "lua/autorun/sh_field_setter.lua")
                .expect("field setter file")
                .1 = FIELD_SETTER;
            check(&ws, &texts, "single edit");

            let deleted = texts.split_off(2);
            ws.analysis.apply_file_system_changes(
                deleted
                    .iter()
                    .map(|(path, _)| (ws.virtual_url_generator.new_uri(path), None))
                    .collect(),
            );
            check(&ws, &texts, "delete");

            ws.def_files(deleted.clone());
            texts.extend(deleted);
            check(&ws, &texts, "recreate");

            let reopened = ws.analysis.update_files_by_uri(
                texts
                    .iter()
                    .map(|(path, text)| {
                        (
                            ws.virtual_url_generator.new_uri(path),
                            Some(text.to_string()),
                        )
                    })
                    .collect(),
            );
            assert!(
                reopened.is_empty(),
                "reopening unchanged texts re-analyses nothing"
            );
            check(&ws, &texts, "reopen");

            // Every file's positions move at once, so every tail is rebuilt in
            // one batch behind an edit remap.
            let shifted = texts
                .iter()
                .map(|(path, text)| {
                    (
                        *path,
                        format!(
                            "
{text}"
                        ),
                    )
                })
                .collect::<Vec<_>>();
            let shifted = shifted
                .iter()
                .map(|(path, text)| (*path, text.as_str()))
                .collect::<Vec<_>>();
            ws.def_files(shifted.clone());
            check(&ws, &shifted, "shift");
        });
    }
}
