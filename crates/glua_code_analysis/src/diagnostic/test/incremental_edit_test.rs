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
    /// but changes nothing another file can read must not invalidate dependents.
    /// Hashing any position-derived identity breaks it for every edit that is
    /// not at the end of the file.
    #[gtest]
    fn comment_edit_above_a_declaration_keeps_dependents_settled() {
        let mut ws = workspace_with(vec![DiagnosticCode::ParamTypeMismatch]);
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
            ---@param count number
            local function takesNumber(count) end
            takesNumber(values.Count)
            "#,
        );
        let before = codes_in(&ws, consumer_id);

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
    /// index of the same parent kind. An anchor built from the parent's kind
    /// alone collides, and a collision drops both from the remap, leaving their
    /// members owned by a range the edit already moved.
    #[gtest]
    fn sibling_call_argument_tables_survive_an_offset_shift() {
        let mut ws = workspace_with(vec![DiagnosticCode::UndefinedField]);
        let provider_uri = ws.virtual_url_generator.new_uri("lua/register.lua");
        write(
            &mut ws,
            &provider_uri,
            r#"
            registry = registry or {}
            ---@param name string
            ---@param spec table
            function registry.Add(name, spec) end

            registry.Add("first", { alpha = 1 })
            registry.Add("second", { beta = 2 })
            "#,
        );

        let consumer_uri = ws.virtual_url_generator.new_uri("lua/consume.lua");
        let consumer_id = write(
            &mut ws,
            &consumer_uri,
            r#"
            local one = { alpha = 1 }
            local two = { beta = 2 }
            local _ = one.alpha
            local _ = two.beta
            "#,
        );
        let before = codes_in(&ws, consumer_id);

        write(
            &mut ws,
            &provider_uri,
            r#"
            -- a comment that shifts every offset below it
            registry = registry or {}
            ---@param name string
            ---@param spec table
            function registry.Add(name, spec) end

            registry.Add("first", { alpha = 1 })
            registry.Add("second", { beta = 2 })
            "#,
        );

        expect_that!(codes_in(&ws, consumer_id), eq(&before));
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
}
