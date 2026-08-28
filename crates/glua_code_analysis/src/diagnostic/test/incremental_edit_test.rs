#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, FileId, VirtualWorkspace, file_export_fingerprint};
    use googletest::prelude::*;
    use lsp_types::Uri;
    use tokio_util::sync::CancellationToken;

    fn workspace_with(codes: Vec<DiagnosticCode>) -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.diagnostics.enables = codes;
        ws.update_emmyrc(emmyrc);
        ws
    }

    fn codes_in(ws: &VirtualWorkspace, file_id: FileId) -> Vec<String> {
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let mut codes: Vec<String> = diagnostics
            .into_iter()
            .filter_map(|diagnostic| match diagnostic.code {
                Some(lsp_types::NumberOrString::String(code)) => Some(code),
                _ => None,
            })
            .collect();
        codes.sort();
        codes
    }

    fn write(ws: &mut VirtualWorkspace, uri: &Uri, text: &str) -> FileId {
        ws.analysis
            .update_file_by_uri(uri, Some(text.to_string()))
            .expect("file id")
    }

    /// A `@return` edit changes no arity, so a fingerprint that hashes only the
    /// parameter count reports no export change and the reader of the call
    /// keeps the type the old annotation gave it.
    #[gtest]
    fn return_annotation_change_reaches_the_caller() {
        let mut ws = workspace_with(vec![DiagnosticCode::AssignTypeMismatch]);
        let provider_uri = ws.virtual_url_generator.new_uri("lua/provider.lua");
        write(
            &mut ws,
            &provider_uri,
            r#"
            provider = provider or {}
            ---@return string
            function provider.Describe() end
            "#,
        );

        let consumer_uri = ws.virtual_url_generator.new_uri("lua/consumer.lua");
        let consumer_id = write(
            &mut ws,
            &consumer_uri,
            r#"
            ---@type string
            local described = provider.Describe()
            "#,
        );

        expect_that!(
            codes_in(&ws, consumer_id),
            not(contains(eq(DiagnosticCode::AssignTypeMismatch.get_name())))
        );

        write(
            &mut ws,
            &provider_uri,
            r#"
            provider = provider or {}
            ---@return number
            function provider.Describe() end
            "#,
        );

        expect_that!(
            codes_in(&ws, consumer_id),
            contains(eq(DiagnosticCode::AssignTypeMismatch.get_name()))
        );
    }

    /// A string literal is a value, not a shape. Collapsing it to `string` in
    /// the fingerprint hides the change from every file that narrows on it.
    #[gtest]
    fn string_literal_export_change_reaches_the_caller() {
        let mut ws = workspace_with(vec![DiagnosticCode::ParamTypeMismatch]);
        let provider_uri = ws.virtual_url_generator.new_uri("lua/mode.lua");
        write(
            &mut ws,
            &provider_uri,
            r#"
            config = config or {}
            config.Mode = "server"
            "#,
        );

        let consumer_uri = ws.virtual_url_generator.new_uri("lua/reader.lua");
        let consumer_id = write(
            &mut ws,
            &consumer_uri,
            r#"
            ---@param mode "server"
            local function takesServer(mode) end
            takesServer(config.Mode)
            "#,
        );

        expect_that!(codes_in(&ws, consumer_id), is_empty());

        write(
            &mut ws,
            &provider_uri,
            r#"
            config = config or {}
            config.Mode = "client"
            "#,
        );

        expect_that!(
            codes_in(&ws, consumer_id),
            contains(eq(DiagnosticCode::ParamTypeMismatch.get_name()))
        );
    }

    /// The fast path exists for this: an edit that shifts every offset below it
    /// but changes nothing another file can read must not invalidate
    /// dependents. Hashing any position-derived identity breaks it for every
    /// edit that is not at the end of the file.
    #[gtest]
    fn comment_edit_above_a_declaration_keeps_dependents_settled() {
        let mut ws = workspace_with(vec![DiagnosticCode::AssignTypeMismatch]);
        let provider_uri = ws.virtual_url_generator.new_uri("lua/values.lua");
        write(
            &mut ws,
            &provider_uri,
            r#"
            -- leading note
            values = values or {}
            values.Count = 1
            "#,
        );

        let consumer_uri = ws.virtual_url_generator.new_uri("lua/counter.lua");
        let consumer_id = write(
            &mut ws,
            &consumer_uri,
            r#"
            ---@type string
            local wrong = values.Count
            "#,
        );
        // A baseline of "no diagnostics" would let the assertion below pass
        // with the whole cross-file read broken, so the consumer reports one
        // that only survives while that read still resolves.
        let before = codes_in(&ws, consumer_id);
        expect_that!(
            before,
            contains(eq(DiagnosticCode::AssignTypeMismatch.get_name()))
        );

        write(
            &mut ws,
            &provider_uri,
            r#"
            -- leading note, now considerably longer than it was before
            values = values or {}
            values.Count = 1
            "#,
        );

        expect_that!(codes_in(&ws, consumer_id), eq(&before));
    }

    /// Two table literals in two different call argument lists sit at the same
    /// index of the same parent kind, and inserting another shifts every later
    /// ordinal. Either mistake re-homes one literal's members onto the other's
    /// range, so the anchor has to identify the literal, not just its position.
    #[gtest]
    fn sibling_call_argument_tables_keep_their_own_ranges() {
        let register = |extra: &str| {
            format!(
                r#"
            registry = registry or {{}}
            ---@param name string
            ---@param spec table
            function registry.Add(name, spec) end
{extra}
            registry.Add("first", {{ alpha = 1 }})
            registry.Add("second", {{ beta = 2 }})
            "#
            )
        };

        let mut ws = workspace_with(vec![]);
        let uri = ws.virtual_url_generator.new_uri("lua/register.lua");
        let file_id = write(&mut ws, &uri, &register(""));

        // The anchor whose literal declares `alpha`, found by reading the text
        // its range covers.
        fn anchor_for(
            ws: &VirtualWorkspace,
            file_id: FileId,
            field: &str,
        ) -> Option<crate::TableAnchor> {
            let db = ws.analysis.compilation.get_db();
            let text = db.get_vfs().get_file_content(&file_id)?.clone();
            crate::collect_anchored_map(db, file_id)
                .into_iter()
                .find(|(_, range)| text[range.value].contains(field))
                .map(|(anchor, _)| anchor)
        }

        fn text_at(ws: &VirtualWorkspace, file_id: FileId, anchor: &crate::TableAnchor) -> String {
            let db = ws.analysis.compilation.get_db();
            let text = db
                .get_vfs()
                .get_file_content(&file_id)
                .expect("file content")
                .clone();
            let range = crate::collect_anchored_map(db, file_id)
                .get(anchor)
                .expect("anchor still resolves")
                .value;
            text[range].to_string()
        }

        let alpha_anchor = anchor_for(&ws, file_id, "alpha").expect("alpha literal");
        let beta_anchor = anchor_for(&ws, file_id, "beta").expect("beta literal");
        expect_that!(alpha_anchor, not(eq(&beta_anchor)));

        // Insert a third registration above the pair. A positional anchor
        // renumbers here and maps alpha's members onto the new literal.
        let file_id = write(
            &mut ws,
            &uri,
            &register("            registry.Add(\"zeroth\", { zeta = 0 })"),
        );

        expect_that!(
            text_at(&ws, file_id, &alpha_anchor),
            contains_substring("alpha")
        );
        expect_that!(
            text_at(&ws, file_id, &beta_anchor),
            contains_substring("beta")
        );
    }

    /// Deleting a file has no new text to fingerprint. Comparing fingerprints
    /// lets a file that exported nothing return before the removal runs, and
    /// its dependents keep resolving members it no longer defines.
    #[gtest]
    fn deleting_a_provider_invalidates_its_dependents() {
        let mut ws = workspace_with(vec![
            DiagnosticCode::UndefinedField,
            DiagnosticCode::UndefinedGlobal,
        ]);
        let provider_uri = ws.virtual_url_generator.new_uri("lua/provider.lua");
        write(
            &mut ws,
            &provider_uri,
            r#"
            shared = shared or {}
            shared.Helper = function() end
            "#,
        );

        let consumer_uri = ws.virtual_url_generator.new_uri("lua/uses_helper.lua");
        let consumer_id = write(
            &mut ws,
            &consumer_uri,
            r#"
            shared.Helper()
            "#,
        );
        expect_that!(codes_in(&ws, consumer_id), is_empty());

        ws.analysis.update_file_by_uri(&provider_uri, None);

        expect_that!(
            codes_in(&ws, consumer_id),
            contains(eq(DiagnosticCode::UndefinedGlobal.get_name()))
        );
    }

    /// Every `LuaType::Signature` reachable from a stored type cache, whose id
    /// the signature index no longer holds.
    fn dangling_signature_references(ws: &VirtualWorkspace) -> Vec<String> {
        let db = ws.analysis.compilation.get_db();
        let mut dangling = Vec::new();
        for file_id in db.get_vfs().get_all_file_ids() {
            let Some(owners) = db.get_type_index().file_type_owners(file_id) else {
                continue;
            };
            for owner in owners.iter() {
                let Some(cache) = db.get_type_index().get_type_cache(owner) else {
                    continue;
                };
                crate::db_index::TypeVisitTrait::visit_type(cache.as_type(), &mut |inner| {
                    if let crate::LuaType::Signature(id) = inner
                        && db.get_signature_index().get(id).is_none()
                    {
                        dangling.push(format!("{owner:?} -> {id:?}"));
                    }
                });
            }
        }
        dangling.sort();
        dangling
    }

    /// The fingerprint decides whether an edit ripples to dependents. These
    /// exercise it directly: a diagnostic-level assertion on a two-file
    /// workspace can be satisfied by an unrelated re-analysis, so it does not
    /// prove which dimension the fingerprint actually reads.
    fn fingerprint_of(ws: &VirtualWorkspace, file_id: FileId) -> u64 {
        file_export_fingerprint(ws.analysis.compilation.get_db(), file_id)
    }

    fn fingerprint_after_edit(first: &str, second: &str) -> (u64, u64) {
        let mut ws = workspace_with(vec![]);
        let uri = ws.virtual_url_generator.new_uri("lua/subject.lua");
        let file_id = write(&mut ws, &uri, first);
        let before = fingerprint_of(&ws, file_id);
        let file_id = write(&mut ws, &uri, second);
        (before, fingerprint_of(&ws, file_id))
    }

    #[gtest]
    fn fingerprint_moves_when_a_return_annotation_changes() {
        let (before, after) = fingerprint_after_edit(
            r#"
            provider = provider or {}
            ---@return string
            function provider.Describe() end
            "#,
            r#"
            provider = provider or {}
            ---@return number
            function provider.Describe() end
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    #[gtest]
    fn fingerprint_moves_when_a_param_annotation_changes() {
        let (before, after) = fingerprint_after_edit(
            r#"
            provider = provider or {}
            ---@param value string
            function provider.Accept(value) end
            "#,
            r#"
            provider = provider or {}
            ---@param value number
            function provider.Accept(value) end
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    #[gtest]
    fn fingerprint_moves_when_an_overload_is_added() {
        let (before, after) = fingerprint_after_edit(
            r#"
            provider = provider or {}
            ---@param value string
            function provider.Accept(value) end
            "#,
            r#"
            provider = provider or {}
            ---@overload fun(value: number, extra: boolean)
            ---@param value string
            function provider.Accept(value) end
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    #[gtest]
    fn fingerprint_moves_when_an_exported_string_literal_changes() {
        let (before, after) = fingerprint_after_edit(
            r#"
            config = config or {}
            config.Mode = "server"
            "#,
            r#"
            config = config or {}
            config.Mode = "client"
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// A member key that is a path is still a member key. Skipping keys that
    /// look like model paths hid the writer evidence for those entries, so a
    /// dependent kept whatever type the previous write gave them.
    #[gtest]
    fn fingerprint_moves_when_a_path_shaped_entry_changes_type() {
        let (before, after) = fingerprint_after_edit(
            r#"
            models = models or {}
            models["models/vehicles/car.mdl"] = 100
            "#,
            r#"
            models = models or {}
            models["models/vehicles/car.mdl"] = "expensive"
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// The property the whole fast path rests on: an edit that shifts every
    /// offset below it, without changing anything a dependent can read, must
    /// leave the fingerprint alone.
    #[gtest]
    fn fingerprint_holds_across_a_comment_edit_above_every_declaration() {
        let (before, after) = fingerprint_after_edit(
            r#"
            -- note
            config = config or {}
            config.Mode = "server"
            ---@return string
            function config.Describe() end
            "#,
            r#"
            -- note, rewritten at greater length so every offset below moves
            config = config or {}
            config.Mode = "server"
            ---@return string
            function config.Describe() end
            "#,
        );
        expect_that!(after, eq(before));
    }

    /// Local state is not observable from another file, so changing it must
    /// not cost a ripple.
    ///
    /// The signature section still reads a local function's *inferred* return
    /// type, so an edit that changes what one returns does ripple. That is an
    /// over-ripple, not a stale read, and narrowing it would mean hashing a
    /// signature by content wherever an exported type names it.
    #[gtest]
    fn fingerprint_holds_across_a_local_only_edit() {
        let (before, after) = fingerprint_after_edit(
            r#"
            config = config or {}
            config.Mode = "server"
            local function helper()
                local scratch = 1
                return scratch
            end
            "#,
            r#"
            config = config or {}
            config.Mode = "server"
            local function helper()
                local scratch = 1
                local unrelated = scratch + 1
                _ = unrelated
                return scratch
            end
            "#,
        );
        expect_that!(after, eq(before));
    }

    /// Every anchor must survive an edit that shifts the file, and must still
    /// name the same literal afterwards. An anchor that resolves to a
    /// *different* literal is worse than one that stops resolving: the remap
    /// then re-homes members onto the wrong table instead of leaving them.
    #[gtest]
    fn every_anchor_keeps_naming_its_own_literal_across_an_offset_shift() {
        let body = r#"
            local Reg = {}
            Reg.Existing = 1
            Registry = Reg

            Direct = { alpha = 1 }

            local Nested = {}
            Nested.inner = { beta = 2 }

            local Config = { section = { epsilon = 5 } }

            register("first", { gamma = 3 })
            register("second", { delta = 4 })
            "#;

        let mut ws = workspace_with(vec![]);
        let uri = ws.virtual_url_generator.new_uri("lua/anchors.lua");

        fn anchored_text(
            ws: &VirtualWorkspace,
            file_id: FileId,
        ) -> std::collections::BTreeMap<String, String> {
            let db = ws.analysis.compilation.get_db();
            let text = db
                .get_vfs()
                .get_file_content(&file_id)
                .expect("file content")
                .clone();
            crate::collect_anchored_map(db, file_id)
                .into_iter()
                .map(|(anchor, range)| {
                    (
                        format!("{anchor:?}"),
                        text[range.value]
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" "),
                    )
                })
                .collect()
        }

        let file_id = write(&mut ws, &uri, body);
        let before = anchored_text(&ws, file_id);
        expect_that!(before.len(), gt(5));

        let file_id = write(
            &mut ws,
            &uri,
            &format!(
                "-- a comment that shifts every offset below it
{body}"
            ),
        );

        // Same anchors, and each still covering the same source text.
        expect_that!(anchored_text(&ws, file_id), eq(&before));
    }

    /// A dependent caches `Signature(file, position)`. Moving the function
    /// changes that position, but the signature's own shape is what the
    /// fingerprint reads, so nothing ripples. If nothing re-homes the id, the
    /// dependent is left naming a signature the index no longer holds.
    #[gtest]
    fn moving_a_function_leaves_no_dangling_signature_reference() {
        let mut ws = workspace_with(vec![]);
        let provider_uri = ws.virtual_url_generator.new_uri("lua/provider.lua");
        let provider = r#"
            provider = provider or {}
            ---@return string
            function provider.Describe() end
            "#;
        write(&mut ws, &provider_uri, provider);

        let consumer_uri = ws.virtual_url_generator.new_uri("lua/alias.lua");
        write(
            &mut ws,
            &consumer_uri,
            r#"
            local describe = provider.Describe
            local described = describe()
            "#,
        );

        let provider_id = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_id(&provider_uri)
            .expect("provider file id");
        let before_fingerprint = fingerprint_of(&ws, provider_id);

        write(
            &mut ws,
            &provider_uri,
            &format!(
                "-- a comment that moves the function below it
{provider}"
            ),
        );

        // Without this the test is vacuous: a changed fingerprint pays the
        // ripple, which re-derives the dependent's cache anyway.
        expect_that!(fingerprint_of(&ws, provider_id), eq(before_fingerprint));
        expect_that!(dangling_signature_references(&ws), is_empty());
    }

    /// A call argument is the only evidence an unannotated parameter in
    /// another file has, and an argument edit changes no member, type decl or
    /// signature in the editing file. Without a call-site section the
    /// fingerprint calls it local and the callee keeps the type the previous
    /// argument gave it.
    #[gtest]
    fn fingerprint_moves_when_a_call_site_changes_an_inferred_receiver() {
        let consumer = |argument: &str| {
            format!(
                r#"
            local PANEL = {{}}
            local OTHER = {{}}
            function PANEL:ProvidedByReceiver() end
            function OTHER:SomethingElse() end
            function PANEL:Load()
                self.Mixin = include("mixins/shared.lua")
            end
            function PANEL:Dispatch(name)
                local callback = self.Mixin[name]
                callback({argument})
            end
            "#
            )
        };

        let mut ws = workspace_with(vec![]);
        ws.def_file(
            "lua/mixins/shared.lua",
            r#"
            local MIXIN = {}
            function MIXIN.Run(self)
                self:ProvidedByReceiver()
            end
            return MIXIN
            "#,
        );

        let consumer_uri = ws.virtual_url_generator.new_uri("lua/autorun/consumer.lua");
        let consumer_id = write(&mut ws, &consumer_uri, &consumer("self"));
        let before = fingerprint_of(&ws, consumer_id);

        write(&mut ws, &consumer_uri, &consumer("OTHER"));

        expect_that!(fingerprint_of(&ws, consumer_id), not(eq(before)));
    }

    /// Adding an `include` changes which files load this one and in what
    /// order, which realm and load-order analysis both read. It moves no
    /// member, type or signature in the editing file.
    #[gtest]
    fn fingerprint_moves_when_a_load_edge_is_added() {
        let mut ws = workspace_with(vec![]);
        ws.def_file(
            "lua/shared/helper.lua",
            r#"
            helper = helper or {}
            function helper.Run() end
            "#,
        );

        let loader_uri = ws.virtual_url_generator.new_uri("lua/autorun/loader.lua");
        let loader_id = write(
            &mut ws,
            &loader_uri,
            r#"
            local ready = true
            "#,
        );
        let before = fingerprint_of(&ws, loader_id);

        write(
            &mut ws,
            &loader_uri,
            r#"
            include("shared/helper.lua")
            local ready = true
            "#,
        );

        expect_that!(fingerprint_of(&ws, loader_id), not(eq(before)));
    }

    /// A `@deprecated` on an exported symbol changes the diagnostics every
    /// call site in every other file reports.
    #[gtest]
    fn fingerprint_moves_when_an_annotation_other_files_act_on_changes() {
        let (before, after) = fingerprint_after_edit(
            r#"
            provider = provider or {}
            function provider.Doc() end
            "#,
            r#"
            provider = provider or {}
            ---@deprecated
            function provider.Doc() end
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// A description is read from this file's index when a hover in another
    /// file asks for it, so no dependent caches one. Rippling a hub file for
    /// prose nothing stores would cost seconds for nothing.
    #[gtest]
    fn fingerprint_holds_across_a_description_edit() {
        let (before, after) = fingerprint_after_edit(
            r#"
            provider = provider or {}
            --- first description
            function provider.Doc() end
            "#,
            r#"
            provider = provider or {}
            --- second description, at greater length
            function provider.Doc() end
            "#,
        );
        expect_that!(after, eq(before));
    }

    /// A metamethod is read by any file that applies the operator to the
    /// owning type.
    #[gtest]
    fn fingerprint_moves_when_an_operator_is_declared() {
        let (before, after) = fingerprint_after_edit(
            r#"
            ---@class Vec
            Vec = {}
            "#,
            r#"
            ---@class Vec
            ---@operator add(Vec): Vec
            Vec = {}
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// Network diagnostics compare a message's writes against its reads across
    /// files, so changing either half is an export change.
    #[gtest]
    fn fingerprint_moves_when_a_net_write_changes() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        // Net ops are recognised through signature metadata, so the annotated
        // builtins have to be present or no flows are collected at all.
        ws.def_gmod_call_arg_builtins();

        let uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/client/sender.lua");
        let file_id = write(
            &mut ws,
            &uri,
            r#"
            net.Start("Msg")
            net.WriteString("payload")
            net.SendToServer()
            "#,
        );
        expect_that!(
            ws.analysis
                .compilation
                .get_db()
                .get_gmod_network_index()
                .get_file_data(file_id)
                .map(|data| data.send_flows.len()),
            some(gt(0))
        );
        let before = fingerprint_of(&ws, file_id);

        let file_id = write(
            &mut ws,
            &uri,
            r#"
            net.Start("Msg")
            net.WriteInt(1, 8)
            net.SendToServer()
            "#,
        );

        expect_that!(fingerprint_of(&ws, file_id), not(eq(before)));
    }

    /// Realm is first-class. Wrapping an existing definition in `if SERVER`
    /// changes which callers may reach it and which realm-mismatch
    /// diagnostics other files report, while leaving its name, type and
    /// signature alone.
    #[gtest]
    fn fingerprint_moves_when_a_declaration_changes_realm() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        let uri = ws.virtual_url_generator.new_uri("lua/autorun/subject.lua");
        let file_id = write(
            &mut ws,
            &uri,
            "function Shared() end
",
        );
        let before = fingerprint_of(&ws, file_id);

        let file_id = write(
            &mut ws,
            &uri,
            "if SERVER then
function Shared() end
end
",
        );

        expect_that!(fingerprint_of(&ws, file_id), not(eq(before)));
    }

    /// Repointing an exported alias at a different function in the same file
    /// changes no member key, no owner and no signature shape. Only which
    /// signature the alias names moves, so an identity that keeps just the
    /// file cannot see it.
    #[gtest]
    fn fingerprint_moves_when_an_export_is_repointed_at_another_function() {
        let (before, after) = fingerprint_after_edit(
            r#"
            provider = provider or {}
            ---@return string
            function provider.A() end
            ---@return number
            function provider.B() end
            provider.Dispatch = provider.A
            "#,
            r#"
            provider = provider or {}
            ---@return string
            function provider.A() end
            ---@return number
            function provider.B() end
            provider.Dispatch = provider.B
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// The same for a table literal: the export names a different literal in
    /// the same file, and nothing else about the file changes.
    #[gtest]
    fn fingerprint_moves_when_an_export_is_repointed_at_another_table() {
        let (before, after) = fingerprint_after_edit(
            r#"
            local first = { alpha = 1 }
            local second = { beta = 2 }
            Exported = first
            _ = second
            "#,
            r#"
            local first = { alpha = 1 }
            local second = { beta = 2 }
            Exported = second
            _ = first
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// The property the whole fast path rests on, swept across every shape the
    /// fingerprint reads: an edit that only shifts byte offsets must not move
    /// it. A source position reaching the hash through any section - often via
    /// `Debug` on a struct that embeds a range - defeats the optimisation for
    /// every edit that is not at the end of a file.
    #[gtest]
    fn fingerprint_holds_across_an_offset_shift_for_every_hashed_shape() {
        let bodies: Vec<(&str, &str)> = vec![
            (
                "global number",
                "A = 1
",
            ),
            (
                "global string",
                "B = \"two\"
",
            ),
            (
                "global function",
                "function C() end
",
            ),
            (
                "annotated function",
                "P = P or {}
---@param x string
---@return integer
function P.F(x) end
",
            ),
            (
                "class and field",
                "---@class K
---@field a string
K = {}
",
            ),
            (
                "alias",
                "---@alias M string
",
            ),
            (
                "enum",
                "---@enum E
E = { X = 1 }
",
            ),
            (
                "operator",
                "---@class V
---@operator add(V): V
V = {}
",
            ),
            (
                "metatable operator",
                "Obj = setmetatable({ v = 1 }, { __add = function(a, b) return a end })
",
            ),
            (
                "table literals",
                "T = { a = 1, b = { c = 2 } }
local L = { d = 3 }
U = L
",
            ),
            (
                "realm branches",
                "if SERVER then
S = 1
else
S = 2
end
",
            ),
            (
                "vgui panel",
                "local PANEL = {}
AccessorFunc(PANEL, \"m_a\", \"A\")
vgui.Register(\"W\", PANEL, \"Panel\")
",
            ),
            (
                "include",
                "include(\"shared/other.lua\")
R = 1
",
            ),
            (
                "local function",
                "local function helper()
  return 1
end
G = helper()
",
            ),
            (
                "deprecated",
                "P = P or {}
---@deprecated
function P.Old() end
",
            ),
        ];

        // A net flow needs a realm path and the annotated builtins, so it gets
        // its own fixture below rather than a shared one that would silently
        // record no flows and make the sweep vacuous for it.
        let mut moved: Vec<&str> = Vec::new();
        for (name, body) in bodies {
            // Both a comment and a blank line: a comment directly above a
            // declaration also becomes its doc comment, which must not count
            // as an export change either.
            for prefix in [
                "-- padding above everything
",
                "
",
            ] {
                let mut ws = VirtualWorkspace::new();
                let mut emmyrc = Emmyrc::default();
                emmyrc.gmod.enabled = true;
                ws.update_emmyrc(emmyrc);
                ws.def_gmod_call_arg_builtins();
                ws.def_file(
                    "lua/shared/other.lua",
                    "other = 1
",
                );
                let uri = ws.virtual_url_generator.new_uri("lua/autorun/subject.lua");
                let file_id = write(&mut ws, &uri, body);
                let before = fingerprint_of(&ws, file_id);
                let file_id = write(&mut ws, &uri, &format!("{prefix}{body}"));
                if fingerprint_of(&ws, file_id) != before {
                    moved.push(name);
                }
            }
        }

        expect_that!(moved, is_empty());
    }

    /// The net-flow half of the sweep. Kept separate because it only records
    /// flows on a realm path with the annotated builtins loaded, and a fixture
    /// that records none would pass whatever the network section hashed.
    #[gtest]
    fn fingerprint_holds_across_an_offset_shift_for_network_flows() {
        let body = |note: &str| {
            format!(
                "-- {note}
net.Start(\"M\")
net.WriteString(\"x\")
net.SendToServer()
"
            )
        };
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        let uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/client/sender.lua");
        let file_id = write(&mut ws, &uri, &body("note"));
        expect_that!(
            ws.analysis
                .compilation
                .get_db()
                .get_gmod_network_index()
                .get_file_data(file_id)
                .map(|data| data.send_flows.len()),
            some(gt(0))
        );
        let before = fingerprint_of(&ws, file_id);

        let file_id = write(&mut ws, &uri, &body("note, rewritten at greater length"));

        expect_that!(fingerprint_of(&ws, file_id), eq(before));
    }

    /// The editor writes the text first and re-indexes later, so by the time
    /// the ripple decision is made the VFS already holds the new tree while the
    /// index still holds the old entries. A fingerprint taken at that moment
    /// compares the old index against the new tree and moves for any edit that
    /// shifts a table literal - which is most files.
    #[gtest]
    fn the_editors_write_then_index_sequence_keeps_a_shifted_file_settled() {
        let body = |note: &str| {
            format!(
                r#"
            -- {note}
            MYLIB = MYLIB or {{}}
            MYLIB.Config = {{ enabled = true }}
            function MYLIB.Run() end
            "#
            )
        };

        let mut ws = workspace_with(vec![]);
        let uri = ws.virtual_url_generator.new_uri("lua/autorun/mylib.lua");
        let file_id = write(&mut ws, &uri, &body("note"));

        // Exactly what the editor does: text first, index after.
        ws.analysis
            .update_file_text_only(&uri, body("note, rewritten at greater length"));
        let (changed, expansion) = ws
            .analysis
            .self_index_files_and_get_ripple_with_changed(vec![file_id]);

        expect_that!(changed, is_empty());
        expect_that!(expansion, is_empty());
    }

    /// The same sequence, for an edit that does change an export: it has to
    /// still report the ripple.
    #[gtest]
    fn the_editors_write_then_index_sequence_still_reports_a_real_change() {
        let mut ws = workspace_with(vec![]);
        let uri = ws.virtual_url_generator.new_uri("lua/autorun/mylib.lua");
        let file_id = write(
            &mut ws,
            &uri,
            r#"
            MYLIB = MYLIB or {}
            MYLIB.Mode = "server"
            "#,
        );

        ws.analysis.update_file_text_only(
            &uri,
            r#"
            MYLIB = MYLIB or {}
            MYLIB.Mode = "client"
            "#
            .to_string(),
        );
        let (changed, _) = ws
            .analysis
            .self_index_files_and_get_ripple_with_changed(vec![file_id]);

        expect_that!(changed, contains(eq(&file_id)));
    }

    /// Two literals with the *same* field names, and a literal with no fields
    /// at all, cannot be told apart by their fields. Neither may fall back to
    /// a sibling ordinal: inserting a third registration renumbers those, and
    /// the old anchor would then resolve to a different literal and re-home its
    /// members onto the wrong table.
    #[gtest]
    fn indistinguishable_literals_are_never_remapped_onto_each_other() {
        let source = |extra: &str| {
            format!(
                r#"
            registry = registry or {{}}
            ---@param name string
            ---@param spec table
            function registry.Add(name, spec) end
{extra}
            registry.Add("first", {{ name = "a" }})
            registry.Add("second", {{ name = "b" }})
            registry.Add("third", {{}})
            "#
            )
        };

        let mut ws = workspace_with(vec![]);
        let uri = ws.virtual_url_generator.new_uri("lua/registry.lua");
        let file_id = write(&mut ws, &uri, &source(""));

        let text_by_anchor = |ws: &VirtualWorkspace, file_id: FileId| {
            let db = ws.analysis.compilation.get_db();
            let text = db
                .get_vfs()
                .get_file_content(&file_id)
                .expect("file content")
                .clone();
            crate::collect_anchored_map(db, file_id)
                .into_iter()
                .map(|(anchor, range)| (format!("{anchor:?}"), text[range.value].to_string()))
                .collect::<std::collections::BTreeMap<_, _>>()
        };

        let before = text_by_anchor(&ws, file_id);
        let file_id = write(
            &mut ws,
            &uri,
            &source("            registry.Add(\"zeroth\", { name = \"z\" })"),
        );
        let after = text_by_anchor(&ws, file_id);

        // Any anchor that survives the insertion must still cover the same
        // literal. An anchor that cannot promise that must not be emitted.
        let survivors: Vec<&String> = before.keys().filter(|a| after.contains_key(*a)).collect();
        // Without this the loop below would be satisfied by nothing surviving.
        expect_that!(survivors.len(), gt(0));
        for (anchor, text) in &before {
            if let Some(after_text) = after.get(anchor) {
                expect_that!(after_text, eq(text), "anchor {anchor} moved literal");
            }
        }
    }

    /// An alias is read by name and resolved to its target, so changing the
    /// target changes what every file that names it infers. The alias body
    /// lives on the type declaration, not among its supertypes.
    #[gtest]
    fn fingerprint_moves_when_an_alias_target_changes() {
        let (before, after) = fingerprint_after_edit(
            r#"
            ---@alias Mode string
            "#,
            r#"
            ---@alias Mode integer
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// A supertype is part of what a dependent resolves through the class.
    #[gtest]
    fn fingerprint_moves_when_a_supertype_is_added() {
        let (before, after) = fingerprint_after_edit(
            r#"
            ---@class Base
            Base = {}
            ---@class Derived
            Derived = {}
            "#,
            r#"
            ---@class Base
            Base = {}
            ---@class Derived : Base
            Derived = {}
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// A namespace changes how every name in the file resolves for a
    /// dependent, without moving any member or signature.
    #[gtest]
    fn fingerprint_moves_when_a_namespace_is_declared() {
        let (before, after) = fingerprint_after_edit(
            r#"
            ---@class Thing
            Thing = {}
            "#,
            r#"
            ---@namespace Shared
            ---@class Thing
            Thing = {}
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// A writer's own evidence, not the merged result: whether the write was
    /// guarded decides how the widening merge treats it, and the merge runs
    /// for every file that contributes to the same slot.
    #[gtest]
    fn fingerprint_moves_when_a_writers_guard_changes() {
        let (before, after) = fingerprint_after_edit(
            r#"
            config = config or {}
            config.Values = {}
            "#,
            r#"
            config = config or {}
            config.Values = config.Values or {}
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// The end-to-end invariant the remap exists for: no member may be left
    /// owned by a range that is no longer a table literal.
    ///
    /// A dependent's members are not re-derived by the edited file's
    /// re-index, so if the remap does not move them they point into the wrong
    /// text. Repeated edits matter: a stash that is never consumed makes the
    /// remap a no-op from the second edit onwards.
    #[gtest]
    fn no_member_is_left_owned_by_a_range_that_is_no_longer_a_literal() {
        let provider = |note: &str| {
            format!(
                r#"
            -- {note}
            Registry = {{ existing = 1 }}
            Extra = {{ other = 2 }}
            "#
            )
        };

        let mut ws = workspace_with(vec![]);
        let provider_uri = ws.virtual_url_generator.new_uri("lua/registry.lua");
        write(&mut ws, &provider_uri, &provider("note"));

        let consumer_uri = ws.virtual_url_generator.new_uri("lua/adds_handler.lua");
        write(
            &mut ws,
            &consumer_uri,
            r#"
            Registry.Handlers = {}
            Extra.More = {}
            "#,
        );

        /// Element owners that no table literal in the current text occupies.
        fn orphaned_owners(ws: &VirtualWorkspace) -> Vec<String> {
            let db = ws.analysis.compilation.get_db();
            let mut live: std::collections::HashSet<crate::InFiled<rowan::TextRange>> =
                std::collections::HashSet::new();
            for file_id in db.get_vfs().get_all_file_ids() {
                let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) else {
                    continue;
                };
                for table in glua_parser::LuaAstNode::descendants::<glua_parser::LuaTableExpr>(
                    &tree.get_chunk_node(),
                ) {
                    live.insert(crate::InFiled::new(
                        file_id,
                        glua_parser::LuaAstNode::get_range(&table),
                    ));
                }
            }
            db.get_member_index()
                .element_owner_ranges()
                .into_iter()
                .filter(|range| !live.contains(range))
                .map(|range| format!("{range:?}"))
                .collect()
        }

        expect_that!(orphaned_owners(&ws), is_empty());

        for note in ["note, rewritten once", "note, rewritten a second time"] {
            write(&mut ws, &provider_uri, &provider(note));
            expect_that!(orphaned_owners(&ws), is_empty(), "after edit: {note}");
        }
    }

    /// `AccessorFunc` synthesizes getter and setter members on the owning
    /// class, which any file can then call. They are synthesized into this
    /// file's member set, so the members section is what carries them - there
    /// is no separate call-index section to keep in step.
    #[gtest]
    fn fingerprint_moves_when_an_accessor_func_is_renamed() {
        let panel = |accessor: &str| {
            format!(
                "local PANEL = {{}}
AccessorFunc(PANEL, \"m_name\", \"{accessor}\")
vgui.Register(\"MyPanel\", PANEL, \"Panel\")
"
            )
        };
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        let uri = ws.virtual_url_generator.new_uri("lua/autorun/panel.lua");
        let file_id = write(&mut ws, &uri, &panel("Name"));
        // The synthesized accessors have to actually be there, or the
        // assertion below would hold for a file that declares nothing.
        let member_keys: Vec<String> = ws
            .analysis
            .compilation
            .get_db()
            .get_member_index()
            .get_file_members(file_id)
            .iter()
            .map(|member| format!("{:?}", member.get_key()))
            .collect();
        expect_that!(member_keys, contains(contains_substring("GetName")));
        let before = fingerprint_of(&ws, file_id);

        let file_id = write(&mut ws, &uri, &panel("Title"));

        expect_that!(fingerprint_of(&ws, file_id), not(eq(before)));
    }

    /// A `setmetatable` binding is read by every file that resolves a member
    /// through the table. Repointing it at a different literal in the same
    /// file moves no member, type or signature.
    #[gtest]
    fn fingerprint_moves_when_a_metatable_binding_is_repointed() {
        let source = |metatable: &str| {
            format!(
                r#"
            local mtA = {{ alpha = 1 }}
            local mtB = {{ beta = 2 }}
            _ = mtA
            _ = mtB
            Foo = setmetatable({{}}, {metatable})
            "#
            )
        };

        let mut ws = workspace_with(vec![]);
        let uri = ws.virtual_url_generator.new_uri("lua/meta.lua");
        let file_id = write(&mut ws, &uri, &source("mtA"));
        // Without a recorded binding the assertion below would hold whatever
        // the section hashed.
        expect_that!(
            ws.analysis
                .compilation
                .get_db()
                .get_metatable_index()
                .metatable_count(),
            gt(0)
        );
        let before = fingerprint_of(&ws, file_id);

        let file_id = write(&mut ws, &uri, &source("mtB"));

        expect_that!(fingerprint_of(&ws, file_id), not(eq(before)));
    }

    /// A `@field` default gates whether that field counts as required, and the
    /// missing-field diagnostic is reported by the file that builds the table.
    #[gtest]
    fn fingerprint_moves_when_a_field_default_is_added() {
        let (before, after) = fingerprint_after_edit(
            r#"
            ---@class Config
            ---@field timeout number
            Config = {}
            "#,
            r#"
            ---@class Config
            ---@field timeout number
            ---@field retries? number
            Config = {}
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// A guard inferred from a function body narrows the parameter for every
    /// caller, in any file. A file that already has one takes the full path,
    /// so the case the fingerprint has to catch is a file whose guard changes
    /// what it narrows to.
    #[gtest]
    fn fingerprint_moves_when_an_inferred_guard_narrows_differently() {
        let mut ws = workspace_with(vec![]);
        ws.def_file(
            "lua/shared/entity_meta.lua",
            r#"
            ---@class Entity
            ---@class NULL: Entity
            ---@class Player: Entity
            ---@class NPC: Entity
            ---@param value any
            ---@return TypeGuard<any>
            ---@return_cast value -NULL
            function IsValid(value) end
            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end
            ---@return boolean
            ---@return_cast self NPC
            function Entity:IsNPC() end
            "#,
        );

        fn guard_count(ws: &VirtualWorkspace, file_id: FileId) -> usize {
            ws.analysis
                .compilation
                .get_db()
                .get_signature_index()
                .inferred_guard_facts_for_files(&std::collections::HashSet::from([file_id]))
                .len()
        }

        let uri = ws.virtual_url_generator.new_uri("lua/shared/guard.lua");
        let file_id = write(
            &mut ws,
            &uri,
            "function GuardA(ent) return IsValid(ent) and ent:IsPlayer() end",
        );
        // Without a recorded guard the assertion below would hold whatever the
        // section hashed.
        expect_that!(guard_count(&ws, file_id), gt(0));
        let before = fingerprint_of(&ws, file_id);

        let file_id = write(
            &mut ws,
            &uri,
            "function GuardA(ent) return IsValid(ent) and ent:IsNPC() end",
        );
        expect_that!(guard_count(&ws, file_id), gt(0));

        expect_that!(fingerprint_of(&ws, file_id), not(eq(before)));
    }

    /// The table a module returns is its export type, which every consumer of
    /// `require`/`include` reads. The returned local is skipped by the
    /// type-cache section, so returning a different table moves nothing else.
    #[gtest]
    fn fingerprint_moves_when_a_module_returns_a_different_table() {
        let (before, after) = fingerprint_after_edit(
            r#"
            local M = { a = 1 }
            local N = { b = 2 }
            _ = N
            return M
            "#,
            r#"
            local M = { a = 1 }
            local N = { b = 2 }
            _ = M
            return N
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// Two table literals in one file are different owners. Collapsing both to
    /// their file makes moving a field from one to the other invisible, even
    /// though every dependent resolving through either literal sees it.
    #[gtest]
    fn fingerprint_moves_when_a_field_moves_between_two_literals() {
        let (before, after) = fingerprint_after_edit(
            r#"
            Shared = { alpha = 1 }
            Other = { beta = 2 }
            "#,
            r#"
            Shared = { alpha = 1, beta = 2 }
            Other = {}
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// `@accessorfunc` registers the annotated function in a workspace-wide
    /// index by name, and every other file's call analysis consults it to
    /// decide which argument names the accessor. Retargeting it changes what
    /// gets synthesized there, and moves nothing in this file.
    #[gtest]
    fn fingerprint_moves_when_an_accessorfunc_annotation_is_retargeted() {
        let declaration = |param_index: &str| {
            format!(
                r#"
            ---@class base_item
            ITEM = {{}}

            ---@accessorfunc {param_index}
            function ITEM:AutoFunction(name, key)
            end
            "#
            )
        };

        let mut ws = workspace_with(vec![]);
        let uri = ws.virtual_url_generator.new_uri("lua/items/base_item.lua");
        let file_id = write(&mut ws, &uri, &declaration("1"));
        // Without a registered annotation the assertion below would hold
        // whatever the section hashed.
        expect_that!(
            ws.analysis
                .compilation
                .get_db()
                .get_accessor_func_index()
                .annotations_in_file(file_id)
                .len(),
            gt(0)
        );
        let before = fingerprint_of(&ws, file_id);

        let file_id = write(&mut ws, &uri, &declaration("2"));

        expect_that!(fingerprint_of(&ws, file_id), not(eq(before)));
    }

    /// A computed-key or unresolved-receiver write creates no member, which is
    /// why the dynamic field index exists, but every other file reads it by
    /// name to decide whether a field is known.
    #[gtest]
    fn fingerprint_moves_when_a_dynamic_field_contribution_is_removed() {
        let mut ws = workspace_with(vec![]);
        ws.def_file(
            "lua/shared/player_meta.lua",
            r#"
            ---@class Player
            Player = {}
            "#,
        );

        let uri = ws.virtual_url_generator.new_uri("lua/shared/writes.lua");
        let with_write = r#"
            ---@type Player
            local ply = Player
            ply.myCustomField = 1
            "#;
        let file_id = write(&mut ws, &uri, with_write);
        let before = fingerprint_of(&ws, file_id);

        let file_id = write(
            &mut ws,
            &uri,
            r#"
            ---@type Player
            local ply = Player
            "#,
        );

        expect_that!(fingerprint_of(&ws, file_id), not(eq(before)));
    }

    /// Retargeting a metatable lookup moves the method onto a different class,
    /// which every file that calls it resolves through. The member's key and
    /// this file's text length are unchanged; only its owner moves.
    #[gtest]
    fn fingerprint_moves_when_a_method_is_attached_to_a_different_class() {
        let mut ws = workspace_with(vec![]);
        ws.def_file(
            "lua/shared/meta.lua",
            r#"
            ---@class Entity
            Entity = {}
            ---@class Player : Entity
            Player = {}
            ---@generic T: string
            ---@param name `T`
            ---@return T
            function FindMetaTable(name) end
            "#,
        );

        let uri = ws.virtual_url_generator.new_uri("lua/autorun/extend.lua");
        let owner_of_custom = |ws: &VirtualWorkspace, file_id: FileId| {
            let db = ws.analysis.compilation.get_db();
            db.get_member_index()
                .get_file_members(file_id)
                .iter()
                .find(|member| member.get_key() == &crate::LuaMemberKey::Name("Custom".into()))
                .and_then(|member| db.get_member_index().get_member_owner(&member.get_id()))
                .map(|owner| format!("{owner:?}"))
        };

        let file_id = write(
            &mut ws,
            &uri,
            "local meta = FindMetaTable(\"Player\")
function meta:Custom() end
",
        );
        // The lookup has to actually resolve, or both versions would produce an
        // ownerless member and the assertion below would hold for the wrong
        // reason.
        expect_that!(
            owner_of_custom(&ws, file_id),
            some(contains_substring("Player"))
        );
        let before = fingerprint_of(&ws, file_id);

        let file_id = write(
            &mut ws,
            &uri,
            "local meta = FindMetaTable(\"Entity\")
function meta:Custom() end
",
        );
        expect_that!(
            owner_of_custom(&ws, file_id),
            some(contains_substring("Entity"))
        );

        expect_that!(fingerprint_of(&ws, file_id), not(eq(before)));
    }

    /// The remap has to reach every store keyed by a table literal's range,
    /// not just the member index. A fingerprint test only proves the hash
    /// moved; this proves the entries survived the move.
    ///
    /// The consumer writes into literals the provider declares, so those
    /// entries belong to a file the provider's re-index never revisits.
    ///
    /// These are the only stores that hold another file's literal range. The
    /// metatable, operator and call-site-param indexes were checked and do not:
    /// their entries resolve to a range in the file that writes them, so that
    /// file's own re-index re-derives them.
    #[gtest]
    fn every_store_keyed_by_a_literal_still_points_at_the_same_code() {
        let provider = |note: &str| {
            format!(
                r#"
            -- {note}
            Registry = {{ existing = 1 }}
            Meta = setmetatable({{ value = 1 }}, {{ __add = function(a, b) return a end }})
            ---@param name string
            ---@param spec table
            function Registry.Add(name, spec) end
            Registry.Add("first", {{ alpha = 1 }})
            "#
            )
        };

        let mut ws = workspace_with(vec![]);
        let provider_uri = ws.virtual_url_generator.new_uri("lua/provider.lua");
        write(&mut ws, &provider_uri, &provider("note"));

        let consumer_uri = ws.virtual_url_generator.new_uri("lua/consumer.lua");
        // Every write here keys a store by a literal declared in the provider,
        // so the entries belong to a file the provider's re-index never
        // revisits. Without the remap they keep the pre-edit range.
        write(
            &mut ws,
            &consumer_uri,
            r#"
            Registry.Handlers = {}
            local key = "computed"
            Registry[key] = 1
            setmetatable(Registry, { __add = function(a, b) return a end })
            Registry.Add("second", Registry)
            "#,
        );

        /// What every remapped store currently points at, as the source text
        /// its range covers.
        ///
        /// Checking the text rather than "is this still a literal" is what the
        /// remap actually promises: not every range these stores hold is a
        /// table literal, but each must keep covering the same code.
        fn held_text(ws: &VirtualWorkspace) -> Vec<String> {
            let db = ws.analysis.compilation.get_db();
            let member_index = db.get_member_index();
            let mut held: Vec<(&'static str, crate::InFiled<rowan::TextRange>)> = Vec::new();
            let mut push = |label, ranges: Vec<crate::InFiled<rowan::TextRange>>| {
                held.extend(ranges.into_iter().map(move |range| (label, range)));
            };
            push("member owner", member_index.element_owner_ranges());
            push(
                "contribution",
                member_index
                    .member_assignment_contributions()
                    .table_ranges(),
            );
            push("dynamic field", db.get_dynamic_field_index().table_ranges());

            let mut out: Vec<String> = held
                .into_iter()
                .map(|(store, range)| {
                    let text = db
                        .get_vfs()
                        .get_file_content(&range.file_id)
                        .and_then(|text| text.get(std::ops::Range::<usize>::from(range.value)))
                        .map(|slice| slice.split_whitespace().collect::<Vec<_>>().join(" "))
                        .unwrap_or_else(|| "<out of bounds>".to_string());
                    format!("{store}: {text}")
                })
                .collect();
            // Not deduped: two entries rendering the same text are distinct
            // entries, and dropping one would hide a lost entry whose text
            // happens to match a survivor's.
            out.sort();
            out
        }

        let before = held_text(&ws);
        // A fixture that fills none of these stores would satisfy the loop
        // below with an empty set.
        expect_that!(before.len(), gt(5));

        for note in ["note, rewritten once", "note, rewritten a second time"] {
            write(&mut ws, &provider_uri, &provider(note));
            expect_that!(held_text(&ws), eq(&before), "after edit: {note}");
        }
    }

    /// Deleting a file purges every `Element` owner in it, including literals
    /// no anchor could name. Recreating it must leave the workspace exactly as
    /// it was - an over-eager purge would take members belonging to files the
    /// deletion never touched.
    #[gtest]
    fn deleting_and_recreating_a_file_restores_the_workspace() {
        let provider_source = r#"
            Registry = { existing = 1 }
            Anonymous = { {}, {} }
            "#;

        let mut ws = workspace_with(vec![
            DiagnosticCode::UndefinedField,
            DiagnosticCode::UndefinedGlobal,
        ]);
        let provider_uri = ws.virtual_url_generator.new_uri("lua/registry.lua");
        write(&mut ws, &provider_uri, provider_source);

        let unrelated_uri = ws.virtual_url_generator.new_uri("lua/unrelated.lua");
        let unrelated_id = write(
            &mut ws,
            &unrelated_uri,
            r#"
            Other = { kept = 1 }
            Other.Added = {}
            local _ = Other.kept
            local _ = Other.Added
            "#,
        );

        let consumer_uri = ws.virtual_url_generator.new_uri("lua/consumer.lua");
        let consumer_id = write(
            &mut ws,
            &consumer_uri,
            r#"
            Registry.Handlers = {}
            local _ = Registry.existing
            "#,
        );

        let baseline_consumer = codes_in(&ws, consumer_id);
        let baseline_unrelated = codes_in(&ws, unrelated_id);
        expect_that!(baseline_unrelated, is_empty());

        ws.analysis.update_file_by_uri(&provider_uri, None);
        // A file that shares no literal with the deleted one must be untouched.
        expect_that!(codes_in(&ws, unrelated_id), eq(&baseline_unrelated));

        write(&mut ws, &provider_uri, provider_source);

        expect_that!(codes_in(&ws, consumer_id), eq(&baseline_consumer));
        expect_that!(codes_in(&ws, unrelated_id), eq(&baseline_unrelated));
    }

    /// Reopening a file re-sends its unchanged text. The semantic-no-op gate
    /// should skip the work, and skipping must not leave the index behind.
    #[gtest]
    fn reopening_a_file_with_unchanged_text_keeps_dependents_settled() {
        let mut ws = workspace_with(vec![DiagnosticCode::AssignTypeMismatch]);
        let provider_uri = ws.virtual_url_generator.new_uri("lua/values.lua");
        let provider_source = r#"
            values = values or {}
            values.Count = 1
            values.Table = { nested = true }
            "#;
        write(&mut ws, &provider_uri, provider_source);

        let consumer_uri = ws.virtual_url_generator.new_uri("lua/counter.lua");
        let consumer_id = write(
            &mut ws,
            &consumer_uri,
            r#"
            ---@type string
            local wrong = values.Count
            "#,
        );
        let before = codes_in(&ws, consumer_id);
        expect_that!(
            before,
            contains(eq(DiagnosticCode::AssignTypeMismatch.get_name()))
        );

        // Twice, because the first reopen and every one after take different
        // branches of the unchanged-text gate.
        for _ in 0..2 {
            write(&mut ws, &provider_uri, provider_source);
            expect_that!(codes_in(&ws, consumer_id), eq(&before));
        }
    }

    /// `(exact)` decides whether another file's write creates a member on the
    /// class. The flag lives only on the declaration, so nothing else in this
    /// file moves when it is added.
    #[gtest]
    fn fingerprint_moves_when_a_class_becomes_exact() {
        let (before, after) = fingerprint_after_edit(
            r#"
            ---@class Config
            ---@field known string
            Config = {}
            "#,
            r#"
            ---@class (exact) Config
            ---@field known string
            Config = {}
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// A flag on the declaration lives on each of its locations, not on the
    /// declaration itself, so it is a separate dimension from the base type.
    #[gtest]
    fn fingerprint_moves_when_an_enum_gains_a_flag() {
        let (before, after) = fingerprint_after_edit(
            r#"
            ---@enum Colours
            Colours = { Red = 1 }
            "#,
            r#"
            ---@enum (key) Colours
            Colours = { Red = 1 }
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// An attribute's type is the other half of `extra_type()`, alongside the
    /// enum base, and a consumer resolves it by name.
    #[gtest]
    fn fingerprint_moves_when_an_attribute_type_changes() {
        let (before, after) = fingerprint_after_edit(
            r#"
            ---@class Holder
            ---@field value string
            Holder = {}
            "#,
            r#"
            ---@class Holder
            ---@field value integer
            Holder = {}
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// A member's owner is hashed by its literal's anchor, so that the same
    /// logical table declared in several files hashes the same whichever
    /// literal the resolver happens to pick. That normalisation must not hide
    /// a member genuinely moving between two literals that share a path -
    /// `collect_anchored_map` drops a duplicated anchor as ambiguous, so those
    /// literals fall back to file plus ordinal and stay distinguishable.
    #[gtest]
    fn anchor_keyed_owners_still_see_a_member_move_between_shared_paths() {
        let (before, after) = fingerprint_after_edit(
            r#"
            Cfg = { a = 1 }
            if SERVER then
                Cfg = { b = 2 }
            end
            "#,
            r#"
            Cfg = { a = 1, b = 2 }
            if SERVER then
                Cfg = {}
            end
            "#,
        );
        expect_that!(after, not(eq(before)));
    }

    /// The same for two literals reached by distinct paths, and for a nested
    /// path shared by two roots.
    #[gtest]
    fn anchor_keyed_owners_still_see_a_member_move_between_distinct_paths() {
        let (before, after) = fingerprint_after_edit(
            r#"
            Root = { inner = { a = 1 } }
            Other = { inner = { b = 2 } }
            "#,
            r#"
            Root = { inner = { a = 1, b = 2 } }
            Other = { inner = {} }
            "#,
        );
        expect_that!(after, not(eq(before)));
    }
}
