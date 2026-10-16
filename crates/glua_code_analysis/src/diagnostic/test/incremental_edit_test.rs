#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, FileId, LuaMemberKey, LuaType, VirtualWorkspace};
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

    fn revision_of(ws: &VirtualWorkspace, file_id: FileId) -> u64 {
        ws.analysis
            .compilation
            .get_db()
            .get_reference_index()
            .file_reference_revision(file_id)
    }

    fn write(ws: &mut VirtualWorkspace, uri: &Uri, text: &str) -> FileId {
        ws.analysis
            .update_file_by_uri(uri, Some(text.to_string()))
            .map(|(id, _)| id)
            .expect("file id")
    }

    fn write_deferred(ws: &mut VirtualWorkspace, uri: &Uri, text: &str) -> FileId {
        let file_id = ws
            .analysis
            .update_file_text_only(uri, text.to_string())
            .expect("file id");
        ws.analysis.reindex_files(vec![file_id]);
        file_id
    }

    fn decl_type(ws: &VirtualWorkspace, file_id: FileId, name: &str) -> LuaType {
        let db = ws.analysis.compilation.get_db();
        let decl_id = db
            .get_decl_index()
            .get_decl_tree(&file_id)
            .and_then(|tree| {
                tree.get_decls()
                    .values()
                    .find(|decl| decl.get_name() == name)
                    .map(|decl| decl.get_id())
            })
            .expect("declaration");
        db.get_type_index()
            .get_type_cache(&decl_id.into())
            .map(|cache| cache.as_type().clone())
            .expect("declaration type")
    }

    /// `target`'s diagnostics in a cold build of `files`.
    fn cold_codes(codes: Vec<DiagnosticCode>, files: &[(&str, &str)], target: &str) -> Vec<String> {
        let mut cold = workspace_with(codes);
        let uris: Vec<(Uri, Option<String>)> = files
            .iter()
            .map(|(path, text)| {
                (
                    cold.virtual_url_generator.new_uri(path),
                    Some(text.to_string()),
                )
            })
            .collect();
        cold.analysis.update_files_by_uri(uris);
        let target_uri = cold.virtual_url_generator.new_uri(target);
        let target_id = cold
            .analysis
            .get_file_id(&target_uri)
            .expect("cold file id");
        codes_in(&cold, target_id)
    }

    /// A `@return` edit changes no arity, so an invalidation keyed on the
    /// parameter count alone leaves the reader of the call with the type the
    /// old annotation gave it.
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

    /// A `---@module` reader caches `LuaType::ModuleRef(provider)`, which names
    /// the providing file rather than any symbol it exports, and still has to
    /// see a member type the provider changes.
    #[gtest]
    fn module_annotation_reader_sees_provider_edit() {
        let mut ws = workspace_with(vec![DiagnosticCode::AssignTypeMismatch]);
        let provider_uri = ws.virtual_url_generator.new_uri("modprovider.lua");
        write(
            &mut ws,
            &provider_uri,
            r#"
            local M = {}
            M.value = 1
            return M
            "#,
        );

        let consumer_uri = ws.virtual_url_generator.new_uri("modconsumer.lua");
        let consumer_id = write(
            &mut ws,
            &consumer_uri,
            r#"
            ---@module "modprovider"
            ModuleProvider = {}

            local observed = ModuleProvider.value
            "#,
        );

        expect_that!(
            decl_type(&ws, consumer_id, "observed"),
            eq(&LuaType::IntegerConst(1))
        );

        write(
            &mut ws,
            &provider_uri,
            r#"
            local M = {}
            M.value = "text"
            return M
            "#,
        );

        let observed = decl_type(&ws, consumer_id, "observed");
        expect_true!(
            matches!(&observed, LuaType::StringConst(value) if value.as_str() == "text"),
            "module reader kept {observed:?} instead of the provider's new string"
        );
    }

    /// A string literal is a value, not a shape. Collapsing it to `string`
    /// hides the change from every file that narrows on it.
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

    /// Deleting a file has no new text to compare, and its dependents must stop
    /// resolving members it no longer defines.
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

        ws.analysis
            .update_file_by_uri(&provider_uri, None)
            .map(|(id, _)| id);

        expect_that!(
            codes_in(&ws, consumer_id),
            contains(eq(DiagnosticCode::UndefinedGlobal.get_name()))
        );
    }

    /// A name defined only by a *later* edit: the reader failed to resolve it,
    /// so it holds no reference the export graph can follow back. The added
    /// key still has to reach it, without the reader being edited.
    #[gtest]
    fn defining_a_global_later_clears_its_caller() {
        let mut ws = workspace_with(vec![DiagnosticCode::UndefinedGlobal]);
        let provider_uri = ws.virtual_url_generator.new_uri("lua/provider.lua");
        write(
            &mut ws,
            &provider_uri,
            r#"
            function AlreadyThere() end
            "#,
        );

        let consumer_uri = ws.virtual_url_generator.new_uri("lua/calls_later.lua");
        let consumer_id = write(
            &mut ws,
            &consumer_uri,
            r#"
            CallsLater()
            "#,
        );
        expect_that!(
            codes_in(&ws, consumer_id),
            contains(eq(DiagnosticCode::UndefinedGlobal.get_name()))
        );

        write(
            &mut ws,
            &provider_uri,
            r#"
            function AlreadyThere() end
            function CallsLater() end
            "#,
        );

        expect_that!(
            codes_in(&ws, consumer_id),
            not(contains(eq(DiagnosticCode::UndefinedGlobal.get_name())))
        );
    }

    /// The same for a member on a global path table several files already
    /// write to: the reader's failed lookup has to be repaired when the key
    /// appears.
    #[gtest]
    fn defining_a_member_later_clears_its_reader() {
        let mut ws = workspace_with(vec![
            DiagnosticCode::UndefinedField,
            DiagnosticCode::UndefinedGlobal,
        ]);
        let base_uri = ws.virtual_url_generator.new_uri("lua/util_base.lua");
        write(
            &mut ws,
            &base_uri,
            r#"
            gutil = gutil or {}
            function gutil.Existing() end
            "#,
        );

        let provider_uri = ws.virtual_url_generator.new_uri("lua/util_more.lua");
        write(
            &mut ws,
            &provider_uri,
            r#"
            function gutil.AlsoThere() end
            "#,
        );

        let consumer_uri = ws.virtual_url_generator.new_uri("lua/reads_new_fn.lua");
        let consumer_id = write(
            &mut ws,
            &consumer_uri,
            r#"
            gutil.NewFn()
            "#,
        );
        expect_that!(
            codes_in(&ws, consumer_id),
            contains(eq(DiagnosticCode::UndefinedField.get_name()))
        );

        write(
            &mut ws,
            &provider_uri,
            r#"
            function gutil.AlsoThere() end
            function gutil.NewFn() end
            "#,
        );

        expect_that!(
            codes_in(&ws, consumer_id),
            not(contains(eq(DiagnosticCode::UndefinedField.get_name())))
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

    /// A dependent caches `Signature(file, position)`. Moving the function
    /// changes that position without changing the signature's shape. If
    /// nothing re-homes the id, the dependent is left naming a signature the
    /// index no longer holds.
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

        let moved = format!(
            "-- a comment that moves the function below it
{provider}"
        );

        write(&mut ws, &provider_uri, &moved);

        expect_that!(dangling_signature_references(&ws), is_empty());
    }

    /// The editor writes the text first and re-indexes later, so the VFS holds
    /// the new tree while the index still holds the old entries. An edit that
    /// only shifts offsets must not report the files that read the edited one.
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

        let mut ws = workspace_with(vec![
            DiagnosticCode::UndefinedField,
            DiagnosticCode::UndefinedGlobal,
        ]);
        let uri = ws.virtual_url_generator.new_uri("lua/autorun/mylib.lua");
        let file_id = write(&mut ws, &uri, &body("note"));
        let reader_uri = ws.virtual_url_generator.new_uri("lua/autorun/reader.lua");
        let reader_id = write(
            &mut ws,
            &reader_uri,
            r#"
            MYLIB.Run()
            print(MYLIB.Config.enabled)
            "#,
        );
        expect_that!(codes_in(&ws, reader_id), is_empty());

        // Exactly what the editor does: text first, index after.
        ws.analysis
            .update_file_text_only(&uri, body("note, rewritten at greater length"));
        let affected = ws.analysis.reindex_files(vec![file_id]);

        expect_that!(affected, eq(&vec![file_id]));
    }

    /// Each shift is judged against the text the last one left: a reader
    /// diagnosed before both still reads what the second shift only moved.
    #[gtest]
    fn a_reader_stays_settled_through_two_shifts_of_the_file_it_reads() {
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

        let mut ws = workspace_with(vec![
            DiagnosticCode::UndefinedField,
            DiagnosticCode::UndefinedGlobal,
        ]);
        let uri = ws.virtual_url_generator.new_uri("lua/autorun/mylib.lua");
        let file_id = write(&mut ws, &uri, &body("note"));
        let reader_uri = ws.virtual_url_generator.new_uri("lua/autorun/reader.lua");
        let reader_id = write(
            &mut ws,
            &reader_uri,
            r#"
            MYLIB.Run()
            print(MYLIB.Config.enabled)
            "#,
        );
        expect_that!(codes_in(&ws, reader_id), is_empty());

        let mut affected = Vec::new();
        for note in ["a longer note", "a note longer than the last one was"] {
            ws.analysis.update_file_text_only(&uri, body(note));
            affected.push(ws.analysis.reindex_files(vec![file_id]));
        }

        expect_that!(affected, eq(&vec![vec![file_id], vec![file_id]]));
    }

    /// The same sequence, for an edit that does change an export: the file
    /// that reads it has to be reported and see the new value.
    #[gtest]
    fn the_editors_write_then_index_sequence_still_reports_a_real_change() {
        let mut ws = workspace_with(vec![DiagnosticCode::ParamTypeMismatch]);
        let uri = ws.virtual_url_generator.new_uri("lua/autorun/mylib.lua");
        let file_id = write(
            &mut ws,
            &uri,
            r#"
            MYLIB = MYLIB or {}
            MYLIB.Mode = "server"
            "#,
        );
        let reader_uri = ws.virtual_url_generator.new_uri("lua/autorun/reader.lua");
        let reader_id = write(
            &mut ws,
            &reader_uri,
            r#"
            ---@param mode "server"
            local function takesServer(mode) end
            local current = MYLIB.Mode
            takesServer(current)
            "#,
        );
        expect_that!(codes_in(&ws, reader_id), is_empty());

        ws.analysis.update_file_text_only(
            &uri,
            r#"
            MYLIB = MYLIB or {}
            MYLIB.Mode = "client"
            "#
            .to_string(),
        );
        let affected = ws.analysis.reindex_files(vec![file_id]);

        expect_that!(affected, contains(eq(&reader_id)));
        let current = decl_type(&ws, reader_id, "current");
        expect_true!(
            matches!(&current, LuaType::StringConst(value) if value.as_str() == "client"),
            "reader kept {current:?} instead of the provider's new string"
        );
        expect_that!(
            codes_in(&ws, reader_id),
            contains(eq(DiagnosticCode::ParamTypeMismatch.get_name()))
        );
    }

    /// No member may be left owned by a range that is no longer a table
    /// literal.
    ///
    /// A dependent's members are keyed by the provider's literal ranges, so an
    /// edit that shifts those literals must not leave them pointing into the
    /// wrong text. Repeated edits matter: state that is only refreshed on the
    /// first edit would pass once and fail on the second.
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

    /// Every store keyed by a table literal's range, not just the member
    /// index, must keep its entries pointing at the same code across an edit
    /// that shifts the literal.
    ///
    /// The consumer writes into literals the provider declares, so those
    /// entries belong to a file other than the one being edited.
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
        // so the entries belong to a file other than the one being edited.
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

        /// What every literal-keyed store currently points at, as the source
        /// text its range covers.
        ///
        /// Checking the text rather than "is this still a literal": not every
        /// range these stores hold is a table literal, but each must keep
        /// covering the same code.
        fn held_text(ws: &VirtualWorkspace) -> Vec<String> {
            let db = ws.analysis.compilation.get_db();
            let member_index = db.get_member_index();
            let mut held: Vec<(&'static str, crate::InFiled<rowan::TextRange>)> = Vec::new();
            let mut push = |label, ranges: Vec<crate::InFiled<rowan::TextRange>>| {
                held.extend(ranges.into_iter().map(move |range| (label, range)));
            };
            push("member owner", member_index.element_owner_ranges());
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

    /// What the batch reports: the edited file and every diagnosed file whose
    /// diagnostics read a fact the edit moved, sorted and deduped, and not a
    /// diagnosed file that reads nothing the edit touched.
    ///
    /// A caller that pushes diagnostics uses the whole result as its refresh set.
    #[gtest]
    fn a_filesystem_batch_reports_the_dependents_it_settled() {
        let mut ws = workspace_with(vec![DiagnosticCode::AssignTypeMismatch]);

        let provider_uri = ws.virtual_url_generator.new_uri("lua/fsprovider.lua");
        write(
            &mut ws,
            &provider_uri,
            r#"
            fsprovider = fsprovider or {}
            ---@return string
            function fsprovider.Describe() end
            "#,
        );

        let consumer_uri = ws.virtual_url_generator.new_uri("lua/fsconsumer.lua");
        let consumer_id = write(
            &mut ws,
            &consumer_uri,
            r#"
            ---@type string
            local described = fsprovider.Describe()
            "#,
        );

        let unrelated_uri = ws.virtual_url_generator.new_uri("lua/fsunrelated.lua");
        let unrelated_id = write(
            &mut ws,
            &unrelated_uri,
            r#"
            local untouched = 1
            print(untouched)
            "#,
        );

        expect_that!(
            codes_in(&ws, consumer_id),
            not(contains(eq(DiagnosticCode::AssignTypeMismatch.get_name())))
        );
        expect_that!(codes_in(&ws, unrelated_id), is_empty());

        // The provider's return type moves, so the consumer has to be re-diagnosed
        // even though nothing edited it.
        let affected = ws.analysis.apply_file_system_changes(vec![(
            provider_uri.clone(),
            Some(
                r#"
            fsprovider = fsprovider or {}
            ---@return number
            function fsprovider.Describe() end
            "#
                .to_string(),
            ),
        )]);

        let provider_id = ws.analysis.get_file_id(&provider_uri).expect("provider id");
        expect_that!(affected, contains(eq(&provider_id)));
        expect_that!(affected, contains(eq(&consumer_id)));
        expect_that!(affected, not(contains(eq(&unrelated_id))));
        // Deterministic output: sorted and deduped.
        let mut sorted = affected.clone();
        sorted.sort_unstable();
        sorted.dedup();
        expect_that!(affected, eq(&sorted));
        // The consumer's diagnostics still move, which is why it is returned.
        expect_that!(
            codes_in(&ws, consumer_id),
            contains(eq(DiagnosticCode::AssignTypeMismatch.get_name()))
        );
    }

    /// The language server diagnoses against shared data it computes outside
    /// any file's recording, so a file's own reads have to cover what the
    /// shared data decided for it: deprecating a function elsewhere reports its
    /// caller affected.
    #[gtest]
    fn deprecating_a_function_elsewhere_reports_its_caller_affected() {
        let mut ws = workspace_with(vec![DiagnosticCode::Deprecated]);
        let provider_uri = ws.virtual_url_generator.new_uri("lua/deprprovider.lua");
        write(&mut ws, &provider_uri, "function OldApi() end\n");
        let caller_uri = ws.virtual_url_generator.new_uri("lua/deprcaller.lua");
        let caller_id = write(&mut ws, &caller_uri, "OldApi()\n");
        let shared = ws
            .analysis
            .diagnostic
            .precompute_shared_data(&ws.analysis.compilation);
        ws.analysis
            .diagnose_file_with_shared(caller_id, CancellationToken::new(), shared);

        let affected = ws.analysis.apply_file_system_changes(vec![(
            provider_uri,
            Some("---@deprecated\nfunction OldApi() end\n".to_string()),
        )]);

        expect_that!(affected, contains(eq(&caller_id)));
        expect_that!(
            codes_in(&ws, caller_id),
            contains(eq(DiagnosticCode::Deprecated.get_name()))
        );
    }

    /// A diagnosed file that reads the realm at one offset of another file is
    /// reported affected when the load graph changes that realm, though
    /// neither file was edited.
    #[gtest]
    fn a_realm_the_load_graph_changes_reports_its_diagnosed_reader_affected() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.diagnostics.enables = vec![DiagnosticCode::GmodRealmMismatch];
        ws.update_emmyrc(emmyrc);
        let utils_uri = ws.virtual_url_generator.new_uri("lua/utils.lua");
        write(
            &mut ws,
            &utils_uri,
            "Utils = Utils or {}\nfunction Utils.Ping() return 1 end\n",
        );
        let init_uri = ws.virtual_url_generator.new_uri("lua/sv_init.lua");
        write(&mut ws, &init_uri, "include(\"utils.lua\")\n");
        let reader_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/client/cl_use.lua");
        let reader = write(&mut ws, &reader_uri, "local pinged = Utils.Ping()\n");
        let before = codes_in(&ws, reader);

        let affected = ws.analysis.apply_file_system_changes(vec![(
            init_uri,
            Some("include(\"utils.lua\")\nAddCSLuaFile(\"utils.lua\")\n".to_string()),
        )]);

        expect_that!(codes_in(&ws, reader), not(eq(&before)));
        expect_that!(affected, contains(eq(&reader)));
    }

    fn gmod_workspace_with(code: DiagnosticCode) -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.analysis.diagnostic.enable_only(code);
        ws
    }

    /// `file_id`'s diagnostics as the language server computes them: against
    /// data shared by every file, built outside any file's recorded reads.
    fn shared_diagnostics(ws: &VirtualWorkspace, file_id: FileId) -> Vec<lsp_types::Diagnostic> {
        let shared = ws.analysis.precompute_diagnostic_shared_data();
        ws.analysis
            .diagnose_file_with_shared(file_id, CancellationToken::new(), shared)
            .unwrap_or_default()
    }

    /// A receiver's network checks compare it with the send flows of the
    /// whole workspace, shared by every file: a sender's new bit count
    /// reports the receiver affected, though the receiver was not edited.
    #[gtest]
    fn a_senders_new_bit_count_reports_its_receiver_affected() {
        let mut ws = gmod_workspace_with(DiagnosticCode::GmodNetReadWriteBitsMismatch);
        let sender = |bits: u32| {
            format!(
                "util.AddNetworkString(\"Bits\")\nnet.Start(\"Bits\")\nnet.WriteUInt(1, {bits})\nnet.Broadcast()\n"
            )
        };
        let sender_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/sv_send.lua");
        write(&mut ws, &sender_uri, &sender(8));
        let receiver_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/client/cl_receive.lua");
        let receiver = write(
            &mut ws,
            &receiver_uri,
            "net.Receive(\"Bits\", function()\n    local value = net.ReadUInt(16)\nend)\n",
        );
        let before = shared_diagnostics(&ws, receiver);

        let affected = ws
            .analysis
            .apply_file_system_changes(vec![(sender_uri, Some(sender(16)))]);

        expect_that!(shared_diagnostics(&ws, receiver), not(eq(&before)));
        expect_that!(affected, contains(eq(&receiver)));
    }

    /// A receiver's network checks read only the flows of the messages it
    /// receives: another message's sender changing leaves it unaffected.
    #[gtest]
    fn another_messages_sender_leaves_a_receiver_unaffected() {
        let mut ws = gmod_workspace_with(DiagnosticCode::GmodNetReadWriteBitsMismatch);
        let sender = |name: &str, bits: u32| {
            format!(
                "util.AddNetworkString(\"{name}\")
net.Start(\"{name}\")
net.WriteUInt(1, {bits})
net.Broadcast()
"
            )
        };
        let bits_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/sv_send.lua");
        write(&mut ws, &bits_uri, &sender("Bits", 16));
        let other_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/sv_other.lua");
        write(&mut ws, &other_uri, &sender("Other", 8));
        let receiver_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/client/cl_receive.lua");
        let receiver = write(
            &mut ws,
            &receiver_uri,
            "net.Receive(\"Bits\", function()
    local value = net.ReadUInt(16)
end)
",
        );
        shared_diagnostics(&ws, receiver);

        let affected = ws
            .analysis
            .apply_file_system_changes(vec![(other_uri, Some(sender("Other", 16)))]);

        expect_that!(affected, not(contains(eq(&receiver))));
    }

    /// A sender's network checks read only whether its message is received:
    /// another sender of the message changing what it writes reports the
    /// receiver affected, not the sender.
    #[gtest]
    fn another_sender_of_a_message_leaves_a_sender_unaffected() {
        let mut ws = gmod_workspace_with(DiagnosticCode::GmodNetMissingNetworkCounterpart);
        let sender = |bits: u32| {
            format!(
                "util.AddNetworkString(\"Chat\")
net.Start(\"Chat\")
net.WriteUInt(1, {bits})
net.Broadcast()
"
            )
        };
        let edited_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/sv_edited.lua");
        write(&mut ws, &edited_uri, &sender(16));
        let other_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/sv_other.lua");
        let other = write(&mut ws, &other_uri, &sender(16));
        let receiver_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/client/cl_receive.lua");
        let receiver = write(
            &mut ws,
            &receiver_uri,
            "net.Receive(\"Chat\", function()
    local value = net.ReadUInt(16)
end)
",
        );
        shared_diagnostics(&ws, other);
        shared_diagnostics(&ws, receiver);

        let affected = ws
            .analysis
            .apply_file_system_changes(vec![(edited_uri, Some(sender(8)))]);

        expect_that!(affected, contains(eq(&receiver)));
        expect_that!(affected, not(contains(eq(&other))));
    }

    /// A receiver's network checks see a sender's flows by their place in
    /// the sender's file: lines inserted above them leave it unaffected.
    #[gtest]
    fn lines_inserted_above_a_senders_flow_leave_its_receiver_unaffected() {
        let mut ws = gmod_workspace_with(DiagnosticCode::GmodNetReadWriteBitsMismatch);
        let sender = "util.AddNetworkString(\"Bits\")
local function send()
    net.Start(\"Bits\")
    net.WriteUInt(1, 16)
    net.Broadcast()
end
send()
";
        let sender_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/sv_send.lua");
        write(&mut ws, &sender_uri, sender);
        let receiver_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/client/cl_receive.lua");
        let receiver = write(
            &mut ws,
            &receiver_uri,
            "net.Receive(\"Bits\", function()
    local value = net.ReadUInt(16)
end)
",
        );
        shared_diagnostics(&ws, receiver);

        let affected = ws.analysis.apply_file_system_changes(vec![(
            sender_uri,
            Some(format!("function ScnAdd(a, b) return a + b end\n{sender}")),
        )]);

        expect_that!(affected, not(contains(eq(&receiver))));
    }

    /// Realm checks read the callee realms shared by every file: moving a
    /// server function into a shared file reports its client caller affected,
    /// though the caller was not edited.
    #[gtest]
    fn moving_a_server_function_to_a_shared_file_reports_its_client_caller_affected() {
        let mut ws = gmod_workspace_with(DiagnosticCode::GmodRealmMismatchHeuristic);
        let api = "function ServerApi() return 1 end\n";
        let server_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/sv_api.lua");
        write(&mut ws, &server_uri, api);
        let caller_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/client/cl_use.lua");
        let caller = write(&mut ws, &caller_uri, "local value = ServerApi()\n");
        let before = shared_diagnostics(&ws, caller);

        let shared_uri = ws.virtual_url_generator.new_uri("lua/autorun/sh_api.lua");
        let affected = ws.analysis.apply_file_system_changes(vec![
            (server_uri, Some(String::new())),
            (shared_uri, Some(api.to_string())),
        ]);

        expect_that!(shared_diagnostics(&ws, caller), not(eq(&before)));
        expect_that!(affected, contains(eq(&caller)));
    }

    /// The language server reloads a changed config by applying it and
    /// re-reading every workspace file; files already indexed from the same
    /// text must still show what a cold build under the new config shows.
    #[gtest]
    fn a_realm_config_change_matches_a_cold_build() {
        let files = [
            (
                "lua/myaddon/sv_api.lua",
                "function ServerApi() return 1 end\n",
            ),
            (
                "lua/autorun/client/cl_use.lua",
                "local value = ServerApi()\n",
            ),
        ];
        let build = |detect_from_filename: bool| {
            let mut ws = gmod_workspace_with(DiagnosticCode::GmodRealmMismatchHeuristic);
            let mut emmyrc = (*ws.analysis.get_emmyrc()).clone();
            emmyrc.gmod.detect_realm_from_filename = Some(detect_from_filename);
            ws.update_emmyrc(emmyrc);
            let uris = files
                .iter()
                .map(|(path, text)| {
                    (
                        ws.virtual_url_generator.new_uri(path),
                        Some(text.to_string()),
                    )
                })
                .collect::<Vec<_>>();
            ws.analysis.update_files_by_uri(uris.clone());
            (ws, uris)
        };
        let caller_of = |ws: &VirtualWorkspace| {
            let uri = ws.virtual_url_generator.new_uri(files[1].0);
            ws.analysis.get_file_id(&uri).expect("caller file id")
        };
        let (cold, _) = build(false);
        let expected = shared_diagnostics(&cold, caller_of(&cold));

        let (mut warm, uris) = build(true);
        let before = shared_diagnostics(&warm, caller_of(&warm));
        let mut emmyrc = (*warm.analysis.get_emmyrc()).clone();
        emmyrc.gmod.detect_realm_from_filename = Some(false);
        warm.update_emmyrc(emmyrc);
        warm.analysis.update_files_by_uri(uris);

        expect_that!(expected, not(eq(&before)));
        expect_that!(shared_diagnostics(&warm, caller_of(&warm)), eq(&expected));
    }

    /// A sender's counterpart check reads the realm of each receiver of its
    /// message: moving a receiver into and out of an `if SERVER then` block, or
    /// changing only the realm its guard names, leaves the sender showing what a
    /// cold build shows and scheduled for diagnostics when its report changes.
    #[gtest]
    fn a_receivers_realm_changing_matches_a_cold_build() {
        let guarded = |realm: &str| {
            format!(
                "if {realm} then
    net.Receive(\"Moved\", function() end)
end
"
            )
        };
        let shared = "net.Receive(\"Moved\", function() end)
"
        .to_string();
        assert_receiver_edits_match_cold_builds(&[
            shared.clone(),
            guarded("SERVER"),
            shared,
            guarded("CLIENT"),
            guarded("SERVER"),
            guarded("CLIENT"),
        ]);
    }

    /// Changing only the realm an inner guard names, at equal width, leaves the
    /// sender showing what a cold build shows when the guards contradict.
    #[gtest]
    fn a_nested_guard_changing_matches_a_cold_build() {
        let nested = |outer: &str, inner: &str| {
            format!(
                "if {outer} then
    if {inner} then
        net.Receive(\"Moved\", function() end)
    end
end
"
            )
        };
        assert_receiver_edits_match_cold_builds(&[
            nested("CLIENT", "CLIENT"),
            nested("CLIENT", "SERVER"),
            nested("CLIENT", "CLIENT"),
            nested("SERVER", "CLIENT"),
            nested("CLIENT", "CLIENT"),
        ]);
    }

    /// Writes each of `receivers` over the receiver of "Moved" in turn, checking
    /// the sender against a cold build and that it is scheduled when its report changes.
    fn assert_receiver_edits_match_cold_builds(receivers: &[String]) {
        let sender = "util.AddNetworkString(\"Moved\")
net.Start(\"Moved\")
net.Broadcast()
";
        let sender_path = "lua/autorun/server/sv_send.lua";
        let receiver_path = "lua/autorun/sh_receive.lua";
        let cold_sender_codes = |receiver: &str| {
            let mut cold = gmod_workspace_with(DiagnosticCode::GmodNetMissingNetworkCounterpart);
            let uris = [(sender_path, sender), (receiver_path, receiver)]
                .iter()
                .map(|(path, text)| {
                    (
                        cold.virtual_url_generator.new_uri(path),
                        Some(text.to_string()),
                    )
                })
                .collect::<Vec<_>>();
            cold.analysis.update_files_by_uri(uris);
            let sender_uri = cold.virtual_url_generator.new_uri(sender_path);
            let sender_id = cold.analysis.get_file_id(&sender_uri).expect("sender id");
            shared_diagnostics(&cold, sender_id)
        };
        let colds = receivers
            .iter()
            .map(|receiver| cold_sender_codes(receiver))
            .collect::<Vec<_>>();
        expect_that!(colds[1] != colds[0], eq(true));

        let mut warm = gmod_workspace_with(DiagnosticCode::GmodNetMissingNetworkCounterpart);
        let sender_uri = warm.virtual_url_generator.new_uri(sender_path);
        let sender_id = write(&mut warm, &sender_uri, sender);
        let receiver_uri = warm.virtual_url_generator.new_uri(receiver_path);
        write(&mut warm, &receiver_uri, &receivers[0]);
        expect_that!(shared_diagnostics(&warm, sender_id), eq(&colds[0]));

        for (receiver, cold) in receivers.iter().zip(&colds).skip(1) {
            let before = shared_diagnostics(&warm, sender_id);
            let affected = warm
                .analysis
                .apply_file_system_changes(vec![(receiver_uri.clone(), Some(receiver.clone()))]);
            let after = shared_diagnostics(&warm, sender_id);
            let changed = after != before;
            expect_that!(after, eq(cold));
            if changed {
                expect_that!(affected, contains(eq(&sender_id)));
            }
        }
    }

    /// A change only diagnostics read leaves the index as built: nothing is
    /// analysed again, and diagnostics follow the new config at once.
    #[gtest]
    fn a_diagnostics_only_config_change_keeps_the_index() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/autorun/sh_unused.lua",
            "local unused_value = 1
",
        );
        let unused = Some(lsp_types::NumberOrString::String(
            DiagnosticCode::Unused.get_name().to_string(),
        ));
        let reports_unused = |ws: &VirtualWorkspace| {
            shared_diagnostics(ws, file_id)
                .iter()
                .any(|diagnostic| diagnostic.code == unused)
        };
        expect_that!(reports_unused(&ws), eq(true));

        let mut emmyrc = ws.get_emmyrc();
        emmyrc.diagnostics.disable.push(DiagnosticCode::Unused);
        ws.analysis.compilation.start_batch_log();
        ws.update_emmyrc(emmyrc);

        expect_that!(ws.analysis.compilation.take_batch_log(), is_empty());
        expect_that!(reports_unused(&ws), eq(false));
    }

    /// A field check asks whether a table literal is empty only for a numeric
    /// key, so a member added to a global table elsewhere leaves a reader of
    /// one of its named fields unaffected.
    #[gtest]
    fn a_member_added_to_a_global_table_leaves_a_reader_of_another_field_unaffected() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.diagnostics.enables = vec![DiagnosticCode::UndefinedField];
        ws.update_emmyrc(emmyrc);
        let config_uri = ws.virtual_url_generator.new_uri("lua/sh_config.lua");
        let config_text = "cityrp = cityrp or {}\ncityrp.Name = \"x\"\n";
        write(&mut ws, &config_uri, config_text);
        let plugin_uri = ws.virtual_url_generator.new_uri("lua/sh_plugin.lua");
        write(&mut ws, &plugin_uri, "cityrp = cityrp or {}\n");
        let reader_uri = ws.virtual_url_generator.new_uri("lua/cl_name.lua");
        let reader = write(&mut ws, &reader_uri, "local name = cityrp.Name\n");
        codes_in(&ws, reader);

        let affected = ws.analysis.apply_file_system_changes(vec![(
            config_uri,
            Some(format!("{config_text}cityrp.Other = 1\n")),
        )]);

        expect_that!(affected, not(contains(eq(&reader))));
    }

    /// A watched-file delete/create pair in one batch settles to the create.
    ///
    /// The watcher can deliver `DELETED` for a file the editor immediately
    /// re-created (branch switches, atomic saves). Sorting keeps the two
    /// events adjacent and only the last event per URI survives, so the file
    /// must end present with the new text, not detached.
    #[gtest]
    fn watched_batch_delete_then_create_keeps_new_content() {
        let mut ws = workspace_with(vec![]);
        let uri = ws.virtual_url_generator.new_uri("lua/toggle.lua");
        write(&mut ws, &uri, "local old = 1");

        ws.analysis.apply_file_system_changes(vec![
            (uri.clone(), None),
            (uri.clone(), Some("local new = 2".to_string())),
        ]);

        let file_id = ws.analysis.get_file_id(&uri).expect("file present");
        expect_that!(
            ws.analysis
                .compilation
                .get_db()
                .get_vfs()
                .get_file_content(&file_id),
            some(eq(&"local new = 2".to_string()))
        );
        expect_that!(
            decl_type(&ws, file_id, "new"),
            eq(&LuaType::IntegerConst(2))
        );
    }

    /// The reverse order — create then delete — must not resurrect the file.
    ///
    /// The last event per URI is the state on disk, so the batch leaves the
    /// URI detached and no FileId is reachable through it.
    #[gtest]
    fn watched_batch_create_then_delete_detaches_uri() {
        let mut ws = workspace_with(vec![]);
        let uri = ws.virtual_url_generator.new_uri("lua/reverted.lua");
        write(&mut ws, &uri, "local keep = 1");

        ws.analysis.apply_file_system_changes(vec![
            (uri.clone(), Some("local resurrected = 1".to_string())),
            (uri.clone(), None),
        ]);

        expect_that!(ws.analysis.get_file_id(&uri), none());
    }

    /// Distinct URIs in one batch settle independently.
    #[gtest]
    fn watched_batch_handles_distinct_uris() {
        let mut ws = workspace_with(vec![]);
        let kept_uri = ws.virtual_url_generator.new_uri("lua/kept.lua");
        let dropped_uri = ws.virtual_url_generator.new_uri("lua/dropped.lua");
        write(&mut ws, &kept_uri, "local kept_old = 1");
        write(&mut ws, &dropped_uri, "local dropped = 1");

        ws.analysis.apply_file_system_changes(vec![
            (kept_uri.clone(), Some("local kept_new = 2".to_string())),
            (dropped_uri.clone(), None),
        ]);

        let kept_id = ws.analysis.get_file_id(&kept_uri).expect("kept present");
        expect_that!(
            ws.analysis
                .compilation
                .get_db()
                .get_vfs()
                .get_file_content(&kept_id),
            some(eq(&"local kept_new = 2".to_string()))
        );
        expect_that!(ws.analysis.get_file_id(&dropped_uri), none());
    }

    /// A textual collision is not a dependency.
    ///
    /// Both files reference the member key `Field`, but on different owners, so
    /// the provider's type change has no semantic edge to the collider: it is
    /// not reported and its diagnostics are unchanged. A library file
    /// referencing the same name is not returned: annotations are not
    /// diagnostic targets.
    #[gtest]
    fn textual_collision_keeps_unchanged_index_snapshot() {
        let mut ws = workspace_with(vec![DiagnosticCode::AssignTypeMismatch]);
        let library_root = ws.virtual_url_generator.new_path("textual_collision_lib");
        ws.analysis.add_library_workspace(library_root);

        let provider_uri = ws.virtual_url_generator.new_uri("lua/collideprovider.lua");
        write(&mut ws, &provider_uri, "OwnerA = { Field = 1 }\n");
        let collider_uri = ws.virtual_url_generator.new_uri("lua/collideother.lua");
        let collider_id = write(
            &mut ws,
            &collider_uri,
            r#"
            OwnerB = { Field = "kept" }
            ---@type string
            local observed = OwnerB.Field
            "#,
        );
        let library_uri = ws
            .virtual_url_generator
            .new_uri("textual_collision_lib/annot.lua");
        let library_id = write(
            &mut ws,
            &library_uri,
            "OwnerC = { Field = 1 }\nlocal _ = OwnerC.Field\n",
        );
        // Sanity: the collider actually references `Field`, or the test is vacuous.
        expect_that!(
            ws.analysis
                .compilation
                .get_db()
                .get_reference_index()
                .files_referencing_name(&"Field".into()),
            contains(eq(&collider_id))
        );

        let codes_before = codes_in(&ws, collider_id);

        let affected = ws.analysis.apply_file_system_changes(vec![(
            provider_uri.clone(),
            Some("OwnerA = { Field = \"changed\" }\n".to_string()),
        )]);

        expect_that!(affected, not(contains(eq(&collider_id))));
        expect_that!(affected, not(contains(eq(&library_id))));
        expect_that!(codes_in(&ws, collider_id), eq(&codes_before));
    }

    /// Renames and removals cover both the old and the new name.
    ///
    /// The provider renames the member `Old` to `New` on the same owner and at
    /// the same position. Both referencers are returned and their diagnostics
    /// flip sides.
    #[gtest]
    fn rename_and_removal_cover_old_and_new_names() {
        let mut ws = workspace_with(vec![DiagnosticCode::UndefinedField]);
        let provider_uri = ws.virtual_url_generator.new_uri("lua/renameprovider.lua");
        write(&mut ws, &provider_uri, "RenameLib = { Old = 1 }\n");

        let old_consumer_uri = ws.virtual_url_generator.new_uri("lua/renameold.lua");
        let old_consumer_id = write(&mut ws, &old_consumer_uri, "local _ = RenameLib.Old\n");
        let new_consumer_uri = ws.virtual_url_generator.new_uri("lua/renamenew.lua");
        let new_consumer_id = write(&mut ws, &new_consumer_uri, "local _ = RenameLib.New\n");
        expect_that!(codes_in(&ws, old_consumer_id), is_empty());
        expect_that!(
            codes_in(&ws, new_consumer_id),
            contains(eq(DiagnosticCode::UndefinedField.get_name()))
        );

        let affected = ws.analysis.apply_file_system_changes(vec![(
            provider_uri.clone(),
            Some("RenameLib = { New = 1 }\n".to_string()),
        )]);

        expect_that!(affected, contains(eq(&old_consumer_id)));
        expect_that!(affected, contains(eq(&new_consumer_id)));
        // Old referencer now dangles; new referencer now resolves.
        expect_that!(
            codes_in(&ws, old_consumer_id),
            contains(eq(DiagnosticCode::UndefinedField.get_name()))
        );
        expect_that!(codes_in(&ws, new_consumer_id), is_empty());
    }

    /// Duplicate changed names yield one sorted, deduped entry.
    ///
    /// Two files in one batch change the same global `Dup`; the single
    /// consumer referencing it must appear exactly once and the whole result
    /// must already be sorted.
    #[gtest]
    fn duplicate_changed_names_yield_sorted_deduped_output() {
        let mut ws = workspace_with(vec![DiagnosticCode::AssignTypeMismatch]);
        let provider_a_uri = ws.virtual_url_generator.new_uri("lua/dupa.lua");
        write(&mut ws, &provider_a_uri, "Dup = 1\n");
        let provider_b_uri = ws.virtual_url_generator.new_uri("lua/dupb.lua");
        write(&mut ws, &provider_b_uri, "Dup = 2\n");
        let consumer_uri = ws.virtual_url_generator.new_uri("lua/dupconsumer.lua");
        let consumer_id = write(
            &mut ws,
            &consumer_uri,
            "---@type integer\nlocal observed = Dup\n",
        );
        expect_that!(
            codes_in(&ws, consumer_id),
            not(contains(eq(DiagnosticCode::AssignTypeMismatch.get_name())))
        );

        let affected = ws.analysis.apply_file_system_changes(vec![
            (provider_b_uri.clone(), Some("Dup = \"b\"\n".to_string())),
            (provider_a_uri.clone(), Some("Dup = \"a\"\n".to_string())),
        ]);

        expect_that!(affected, contains(eq(&consumer_id)));
        expect_that!(
            affected.iter().filter(|id| **id == consumer_id).count(),
            eq(1)
        );
        let mut sorted = affected.clone();
        sorted.sort_unstable();
        sorted.dedup();
        expect_that!(affected, eq(&sorted));
    }

    /// A hub name fans out to every referencer, with no cap.
    ///
    /// Twenty-five diagnosed consumers all reference `HubField`; changing its
    /// type must return all of them. The count is asserted exactly so any
    /// truncation or budget would fail the test.
    #[gtest]
    fn hub_name_fanout_is_reported_without_cap() {
        const CONSUMERS: usize = 25;
        let mut ws = workspace_with(vec![DiagnosticCode::AssignTypeMismatch]);
        let provider_uri = ws.virtual_url_generator.new_uri("lua/hubprovider.lua");
        write(&mut ws, &provider_uri, "Hub = { HubField = 1 }\n");

        let mut consumer_ids = Vec::new();
        for index in 0..CONSUMERS {
            let uri = ws
                .virtual_url_generator
                .new_uri(&format!("lua/hubconsumer{index}.lua"));
            let file_id = write(
                &mut ws,
                &uri,
                "---@type integer\nlocal observed = Hub.HubField\n",
            );
            expect_that!(
                codes_in(&ws, file_id),
                not(contains(eq(DiagnosticCode::AssignTypeMismatch.get_name())))
            );
            consumer_ids.push(file_id);
        }

        let affected = ws.analysis.apply_file_system_changes(vec![(
            provider_uri.clone(),
            Some("Hub = { HubField = \"changed\" }\n".to_string()),
        )]);

        for consumer_id in &consumer_ids {
            expect_that!(affected, contains(eq(consumer_id)));
            expect_that!(
                codes_in(&ws, *consumer_id),
                contains(eq(DiagnosticCode::AssignTypeMismatch.get_name()))
            );
        }
        let returned_consumers = affected
            .iter()
            .filter(|file_id| consumer_ids.contains(file_id))
            .count();
        expect_that!(returned_consumers, eq(CONSUMERS));
    }

    /// A deletion through the filesystem batch detaches the URI rather than
    /// leaving a tombstone, and the detach happens after the settle, not before
    /// it: until the settle is done the path is what finds the files whose reads
    /// of it have to be re-derived.
    ///
    /// The reader here resolved the deleted file through `include`, so it has to
    /// come back to the state a workspace that never had the provider would be
    /// in, and the provider's path has to be free for a later file to take.
    #[gtest]
    fn a_filesystem_deletion_detaches_the_uri_after_settling_its_readers() {
        let mut ws = workspace_with(vec![DiagnosticCode::UndefinedGlobal]);

        let provider_uri = ws.virtual_url_generator.new_uri("lua/detachprovider.lua");
        write(
            &mut ws,
            &provider_uri,
            r#"
            DetachProvided = 1
            "#,
        );
        let provider_id = ws.analysis.get_file_id(&provider_uri).expect("provider id");

        let reader_uri = ws.virtual_url_generator.new_uri("lua/detachreader.lua");
        let reader_id = write(
            &mut ws,
            &reader_uri,
            r#"
            local _ = DetachProvided
            "#,
        );
        expect_that!(codes_in(&ws, reader_id), is_empty());

        let affected = ws
            .analysis
            .apply_file_system_changes(vec![(provider_uri.clone(), None)]);

        // The deleted file is gone, so it cannot be in a set of live files.
        expect_that!(affected, not(contains(eq(&provider_id))));
        // Detached, not tombstoned: the path is free again.
        expect_that!(ws.analysis.get_file_id(&provider_uri), none());
        // Its reader was settled against the deletion.
        expect_that!(codes_in(&ws, reader_id), not(is_empty()));

        // A new file at the same path is a new file, and restores the reader.
        write(
            &mut ws,
            &provider_uri,
            r#"
            DetachProvided = 1
            "#,
        );
        expect_that!(codes_in(&ws, reader_id), is_empty());
    }

    #[gtest]
    fn repeated_deletion_during_reanalysis_refreshes_member_readers() {
        let codes = vec![
            DiagnosticCode::UndefinedField,
            DiagnosticCode::UncheckedNilAccess,
        ];
        let files = [
            ("gamemode/core/sh_cityrp.lua", "cityrp = {}"),
            (
                "gamemode/core/sh_data.lua",
                "cityrp.data = {}\nfunction cityrp.data.Get(key, default) return default end",
            ),
            (
                "plugins/warps/sh_plugin.lua",
                "local _ = cityrp.data.Get(\"places\", {})",
            ),
        ];
        for slice in [0, 1] {
            let mut ws = workspace_with(codes.clone());
            ws.def_files(files.to_vec());
            let provider_uri = ws.virtual_url_generator.new_uri(files[1].0);
            let reader_uri = ws.virtual_url_generator.new_uri(files[2].0);
            let reader_id = ws.analysis.get_file_id(&reader_uri).unwrap();
            expect_that!(codes_in(&ws, reader_id), is_empty());
            let changed = ws
                .analysis
                .stage_file_system_changes(vec![(provider_uri.clone(), None)]);
            ws.analysis.begin_reindex(changed);
            if slice > 0 {
                ws.analysis
                    .advance_reindex(crate::Slice::Evaluations(slice));
            }
            let changed = ws
                .analysis
                .stage_file_system_changes(vec![(provider_uri.clone(), None)]);
            ws.analysis.begin_reindex(changed);
            let crate::UpdateProgress::Settled(affected) =
                ws.analysis.advance_reindex(crate::Slice::Whole)
            else {
                panic!("whole slice must settle");
            };
            expect_that!(affected, contains(eq(&reader_id)));
            let expected = cold_codes(codes.clone(), &[files[0], files[2]], files[2].0);
            expect_that!(expected, not(is_empty()));
            expect_that!(codes_in(&ws, reader_id), eq(&expected));
            write(&mut ws, &provider_uri, files[1].1);
            expect_that!(codes_in(&ws, reader_id), is_empty());
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

        ws.analysis
            .update_file_by_uri(&provider_uri, None)
            .map(|(id, _)| id);
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

    #[test]
    fn editing_a_nested_record_refreshes_cross_file_pairs_value_caches() {
        const CONSUMER: &str = r#"
            for _, value in pairs(cityrp.configuration["Contraband"]) do end
            "#;
        const BEFORE: &str = r#"
            cityrp = {}
            cityrp.configuration = {}
            cityrp.configuration["Contraband"] = {}
            cityrp.configuration["Contraband"]["first"] = {
                damageReduction = 0.05,
            }
            cityrp.configuration["Contraband"]["second"] = {
                maximum = 2,
            }
            "#;
        const AFTER: &str = r#"
            -- shifted during the edit
            cityrp = {}
            cityrp.configuration = {}
            cityrp.configuration["Contraband"] = {}
            cityrp.configuration["Contraband"]["first"] = {
                damageReduction = 0.06,
            }
            cityrp.configuration["Contraband"]["second"] = {
                maximum = 2,
            }
            "#;

        fn pairs_value_field(ws: &VirtualWorkspace, file_id: FileId) -> LuaType {
            let db = ws.analysis.compilation.get_db();
            let value_id = db
                .get_decl_index()
                .get_decl_tree(&file_id)
                .and_then(|tree| {
                    tree.get_decls()
                        .values()
                        .find(|decl| decl.get_name() == "value")
                        .map(|decl| decl.get_id())
                })
                .expect("pairs value declaration");
            let LuaType::Object(value) = db
                .get_type_index()
                .get_type_cache(&value_id.into())
                .map(|cache| cache.as_type())
                .expect("pairs value type")
            else {
                panic!("pairs value should be a compact record");
            };
            value
                .get_field(&LuaMemberKey::Name("damageReduction".into()))
                .cloned()
                .expect("damageReduction field")
        }

        let mut warm = VirtualWorkspace::new_with_init_std_lib();
        let provider_uri = warm.virtual_url_generator.new_uri("lua/a_config.lua");
        let consumer_uri = warm.virtual_url_generator.new_uri("lua/b_consumer.lua");
        warm.analysis.update_files_by_uri(vec![
            (provider_uri.clone(), Some(BEFORE.to_string())),
            (consumer_uri.clone(), Some(CONSUMER.to_string())),
        ]);
        let consumer_id = warm
            .analysis
            .get_file_id(&consumer_uri)
            .expect("consumer file id");
        write(&mut warm, &provider_uri, AFTER);

        let mut fresh = VirtualWorkspace::new_with_init_std_lib();
        let fresh_provider_uri = fresh.virtual_url_generator.new_uri("lua/a_config.lua");
        let fresh_consumer_uri = fresh.virtual_url_generator.new_uri("lua/b_consumer.lua");
        fresh.analysis.update_files_by_uri(vec![
            (fresh_provider_uri, Some(AFTER.to_string())),
            (fresh_consumer_uri.clone(), Some(CONSUMER.to_string())),
        ]);
        let fresh_consumer_id = fresh
            .analysis
            .get_file_id(&fresh_consumer_uri)
            .expect("fresh consumer file id");

        let warm_field = pairs_value_field(&warm, consumer_id);
        let fresh_field = pairs_value_field(&fresh, fresh_consumer_id);
        assert_eq!(warm.humanize_type(warm_field.clone()), "0.06?");
        assert_eq!(warm_field, fresh_field);
    }

    #[test]
    fn semantic_edit_rebuilds_an_ordinary_structural_generic_cache() {
        const CONSUMER: &str = r#"
            ---@generic T: table, U: table
            ---@param a T
            ---@param b U
            ---@return Merge<T, U>
            local function extend(a, b) end

            local merged = extend(Provider, {})
            local observed = merged.value
            "#;

        let mut warm = VirtualWorkspace::new_with_init_std_lib();
        let provider_uri = warm.virtual_url_generator.new_uri("lua/a_provider.lua");
        let consumer_uri = warm.virtual_url_generator.new_uri("lua/b_consumer.lua");
        warm.analysis.update_files_by_uri(vec![
            (
                provider_uri.clone(),
                Some("Provider = { value = 1 }".to_string()),
            ),
            (consumer_uri.clone(), Some(CONSUMER.to_string())),
        ]);
        let consumer_id = warm
            .analysis
            .get_file_id(&consumer_uri)
            .expect("consumer file id");
        write(&mut warm, &provider_uri, "Provider = { value = \"new\" }");

        let mut fresh = VirtualWorkspace::new_with_init_std_lib();
        let fresh_provider_uri = fresh.virtual_url_generator.new_uri("lua/a_provider.lua");
        let fresh_consumer_uri = fresh.virtual_url_generator.new_uri("lua/b_consumer.lua");
        fresh.analysis.update_files_by_uri(vec![
            (
                fresh_provider_uri,
                Some("Provider = { value = \"new\" }".to_string()),
            ),
            (fresh_consumer_uri.clone(), Some(CONSUMER.to_string())),
        ]);
        let fresh_consumer_id = fresh
            .analysis
            .get_file_id(&fresh_consumer_uri)
            .expect("fresh consumer file id");

        let warm_observed = decl_type(&warm, consumer_id, "observed");
        let fresh_observed = decl_type(&fresh, fresh_consumer_id, "observed");
        assert!(
            matches!(&warm_observed, LuaType::StringConst(value) if value.as_str() == "new"),
            "edited structural cache should contain the new source value, got {warm_observed:?}"
        );
        assert_eq!(warm_observed, fresh_observed);
        assert_eq!(
            decl_type(&warm, consumer_id, "merged"),
            decl_type(&fresh, fresh_consumer_id, "merged")
        );
    }

    #[test]
    fn producer_edit_refreshes_cross_file_inferred_return_consumers() {
        const RETURNER: &str = "function GetStateValue() return State.value end";
        const MOVED_RETURNER: &str =
            "local padding = true\nfunction GetStateValue() return State.value end";
        const CONSUMER: &str = "local observed = GetStateValue()";

        let mut warm = VirtualWorkspace::new_with_init_std_lib();
        let producer_uri = warm.virtual_url_generator.new_uri("lua/a_producer.lua");
        let returner_uri = warm.virtual_url_generator.new_uri("lua/b_returner.lua");
        let consumer_uri = warm.virtual_url_generator.new_uri("lua/c_consumer.lua");
        warm.analysis.update_files_by_uri(vec![
            (
                producer_uri.clone(),
                Some("State = { value = 1 }".to_string()),
            ),
            (returner_uri.clone(), Some(RETURNER.to_string())),
            (consumer_uri.clone(), Some(CONSUMER.to_string())),
        ]);
        let consumer_id = warm
            .analysis
            .get_file_id(&consumer_uri)
            .expect("consumer file id");
        write_deferred(&mut warm, &returner_uri, MOVED_RETURNER);
        write_deferred(&mut warm, &producer_uri, "State = { value = \"new\" }");

        let mut fresh = VirtualWorkspace::new_with_init_std_lib();
        let fresh_producer_uri = fresh.virtual_url_generator.new_uri("lua/a_producer.lua");
        let fresh_returner_uri = fresh.virtual_url_generator.new_uri("lua/b_returner.lua");
        let fresh_consumer_uri = fresh.virtual_url_generator.new_uri("lua/c_consumer.lua");
        fresh.analysis.update_files_by_uri(vec![
            (
                fresh_producer_uri,
                Some("State = { value = \"new\" }".to_string()),
            ),
            (fresh_returner_uri, Some(MOVED_RETURNER.to_string())),
            (fresh_consumer_uri.clone(), Some(CONSUMER.to_string())),
        ]);
        let fresh_consumer_id = fresh
            .analysis
            .get_file_id(&fresh_consumer_uri)
            .expect("fresh consumer file id");

        let warm_observed = decl_type(&warm, consumer_id, "observed");
        assert!(
            matches!(&warm_observed, LuaType::StringConst(value) if value.as_str() == "new"),
            "edited inferred return should contain the new source value, got {warm_observed:?}"
        );
        assert_eq!(
            warm_observed,
            decl_type(&fresh, fresh_consumer_id, "observed")
        );
    }

    /// The editor re-analyses on whichever pool thread is free: a reader
    /// re-evaluated on a thread that never ran an evaluation keeps what it
    /// read, so the next edit still reaches it.
    #[test]
    fn a_reader_re_evaluated_on_a_fresh_thread_still_follows_the_next_edit() {
        const MIDDLE: &str = "Relayed = Chain.value";
        const CONSUMER: &str = "local observed = Relayed";
        const FINAL_PROVIDER: &str = "Chain = { value = true }";

        let mut warm = VirtualWorkspace::new_with_init_std_lib();
        let provider_uri = warm.virtual_url_generator.new_uri("lua/a_provider.lua");
        let middle_uri = warm.virtual_url_generator.new_uri("lua/b_middle.lua");
        let consumer_uri = warm.virtual_url_generator.new_uri("lua/c_consumer.lua");
        warm.analysis.update_files_by_uri(vec![
            (
                provider_uri.clone(),
                Some("Chain = { value = 1 }".to_string()),
            ),
            (middle_uri, Some(MIDDLE.to_string())),
            (consumer_uri.clone(), Some(CONSUMER.to_string())),
        ]);
        let consumer = warm
            .analysis
            .get_file_id(&consumer_uri)
            .expect("consumer file id");
        for text in ["Chain = { value = \"text\" }", FINAL_PROVIDER] {
            std::thread::scope(|scope| {
                std::thread::Builder::new()
                    .stack_size(64 << 20)
                    .spawn_scoped(scope, || write_deferred(&mut warm, &provider_uri, text))
                    .expect("edit thread")
                    .join()
                    .expect("edit on a fresh thread");
            });
        }

        let mut fresh = VirtualWorkspace::new_with_init_std_lib();
        let fresh_provider_uri = fresh.virtual_url_generator.new_uri("lua/a_provider.lua");
        let fresh_middle_uri = fresh.virtual_url_generator.new_uri("lua/b_middle.lua");
        let fresh_consumer_uri = fresh.virtual_url_generator.new_uri("lua/c_consumer.lua");
        fresh.analysis.update_files_by_uri(vec![
            (fresh_provider_uri, Some(FINAL_PROVIDER.to_string())),
            (fresh_middle_uri, Some(MIDDLE.to_string())),
            (fresh_consumer_uri.clone(), Some(CONSUMER.to_string())),
        ]);
        let fresh_consumer = fresh
            .analysis
            .get_file_id(&fresh_consumer_uri)
            .expect("fresh consumer file id");

        assert_eq!(
            decl_type(&warm, consumer, "observed"),
            decl_type(&fresh, fresh_consumer, "observed")
        );
    }

    #[test]
    fn member_addition_refreshes_cross_file_failed_lookup_consumers() {
        const CONSUMER: &str = "local observed = Provider.value";

        let mut warm = VirtualWorkspace::new_with_init_std_lib();
        let provider_uri = warm.virtual_url_generator.new_uri("lua/a_provider.lua");
        let consumer_uri = warm.virtual_url_generator.new_uri("lua/b_consumer.lua");
        warm.analysis.update_files_by_uri(vec![
            (provider_uri.clone(), Some("Provider = {}".to_string())),
            (consumer_uri.clone(), Some(CONSUMER.to_string())),
        ]);
        let consumer_id = warm
            .analysis
            .get_file_id(&consumer_uri)
            .expect("consumer file id");
        write_deferred(&mut warm, &provider_uri, "Provider = { value = \"new\" }");

        let mut fresh = VirtualWorkspace::new_with_init_std_lib();
        let fresh_provider_uri = fresh.virtual_url_generator.new_uri("lua/a_provider.lua");
        let fresh_consumer_uri = fresh.virtual_url_generator.new_uri("lua/b_consumer.lua");
        fresh.analysis.update_files_by_uri(vec![
            (
                fresh_provider_uri,
                Some("Provider = { value = \"new\" }".to_string()),
            ),
            (fresh_consumer_uri.clone(), Some(CONSUMER.to_string())),
        ]);
        let fresh_consumer_id = fresh
            .analysis
            .get_file_id(&fresh_consumer_uri)
            .expect("fresh consumer file id");

        let warm_observed = decl_type(&warm, consumer_id, "observed");
        assert!(
            matches!(&warm_observed, LuaType::StringConst(value) if value.as_str() == "new"),
            "new member should replace the failed lookup, got {warm_observed:?}"
        );
        assert_eq!(
            warm_observed,
            decl_type(&fresh, fresh_consumer_id, "observed")
        );
    }

    #[test]
    fn failed_lookup_survives_anonymous_table_owner_remap() {
        const INITIAL: &str = "function GetContainer() return { inner = {} } end";
        const MOVED: &str = "function GetContainer() return { -- shift\n inner = {} } end";
        const POPULATED: &str =
            "function GetContainer() return { -- shift\n inner = { value = \"new\" } } end";
        const CONSUMER: &str =
            "local container = GetContainer()\nlocal observed = container.inner.value";

        let mut warm = VirtualWorkspace::new_with_init_std_lib();
        let provider_uri = warm.virtual_url_generator.new_uri("lua/a_provider.lua");
        let consumer_uri = warm.virtual_url_generator.new_uri("lua/b_consumer.lua");
        warm.analysis.update_files_by_uri(vec![
            (provider_uri.clone(), Some(INITIAL.to_string())),
            (consumer_uri.clone(), Some(CONSUMER.to_string())),
        ]);
        let consumer_id = warm
            .analysis
            .get_file_id(&consumer_uri)
            .expect("consumer file id");
        write_deferred(&mut warm, &provider_uri, MOVED);
        write_deferred(&mut warm, &provider_uri, POPULATED);

        let mut fresh = VirtualWorkspace::new_with_init_std_lib();
        let fresh_provider_uri = fresh.virtual_url_generator.new_uri("lua/a_provider.lua");
        let fresh_consumer_uri = fresh.virtual_url_generator.new_uri("lua/b_consumer.lua");
        fresh.analysis.update_files_by_uri(vec![
            (fresh_provider_uri, Some(POPULATED.to_string())),
            (fresh_consumer_uri.clone(), Some(CONSUMER.to_string())),
        ]);
        let fresh_consumer_id = fresh
            .analysis
            .get_file_id(&fresh_consumer_uri)
            .expect("fresh consumer file id");

        let warm_observed = decl_type(&warm, consumer_id, "observed");
        assert!(
            matches!(&warm_observed, LuaType::StringConst(value) if value.as_str() == "new"),
            "new member should replace the remapped failed lookup, got {warm_observed:?}"
        );
        assert_eq!(
            warm_observed,
            decl_type(&fresh, fresh_consumer_id, "observed")
        );
    }

    /// Cold-versus-single-file assignment-policy regression: a `target[k] = v`
    /// write inside a loop body must carry the non-overwriting mark no matter
    /// which pass created the member.
    ///
    /// The writer sorts before the file that defines its tables, so during the
    /// cold batch the `Target` prefix is not inferable while the writer is
    /// walked and the loop-body member is created later by the deferred
    /// index-expression path. A single-file re-index finds the prefix
    /// immediately and marks the member through
    /// `is_member_assignment_in_conditional_branch`. A cold member that misses
    /// the mark makes every unrelated edit to the file look like a change its
    /// dependents read.
    #[test]
    fn deferred_loop_body_write_marks_non_overwriting_from_cold_index() {
        use glua_parser::LuaAstNode;

        const DEFS: &str = r#"
Source = {
    alpha = 1,
    beta = 2,
}
Target = {}
"#;
        const WRITER: &str = r#"
for k in pairs(Source) do
    Target[k] = true
end
"#;
        const READER: &str = r#"
local direct = Target.alpha
for _, value in pairs(Target) do
    local _ = value
end
"#;
        const WRITER_CHANGED: &str = r#"
for k in pairs(Source) do
    Target[k] = 1
end
"#;

        /// The loop write's member, located from syntax rather than from a
        /// hard-coded offset: the member id is the write's own syntax node in
        /// this file.
        fn loop_write_member_id(ws: &VirtualWorkspace, file_id: FileId) -> crate::LuaMemberId {
            let db = ws.analysis.compilation.get_db();
            let tree = db
                .get_vfs()
                .get_syntax_tree(&file_id)
                .expect("writer syntax tree");
            let chunk = tree.get_chunk_node();
            for assign_stat in LuaAstNode::descendants::<glua_parser::LuaAssignStat>(&chunk) {
                let (vars, _) = assign_stat.get_var_and_expr_list();
                for var in vars {
                    if var.syntax().text() == "Target[k]"
                        && let glua_parser::LuaVarExpr::IndexExpr(index_expr) = var
                    {
                        return crate::LuaMemberId::new(index_expr.get_syntax_id(), file_id);
                    }
                }
            }
            panic!("loop write `Target[k] = true` not found in writer");
        }

        fn is_non_overwriting(ws: &VirtualWorkspace, member_id: crate::LuaMemberId) -> bool {
            ws.analysis
                .compilation
                .get_db()
                .get_member_index()
                .is_non_overwriting_assignment_member(member_id)
        }

        // Cold build: every file lands in one batch with the writer ordered
        // first, so the writer's `Target` prefix only settles after its own
        // Lua walk and the loop-body member goes through deferred creation.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let writer_uri = ws.virtual_url_generator.new_uri("lua/a_writer.lua");
        let defs_uri = ws.virtual_url_generator.new_uri("lua/b_defs.lua");
        let reader_uri = ws.virtual_url_generator.new_uri("lua/c_reader.lua");
        ws.analysis.update_files_by_uri(vec![
            (writer_uri.clone(), Some(WRITER.to_string())),
            (defs_uri.clone(), Some(DEFS.to_string())),
            (reader_uri.clone(), Some(READER.to_string())),
        ]);
        let writer_id = ws
            .analysis
            .get_file_id(&writer_uri)
            .expect("writer file id");
        let reader_id = ws
            .analysis
            .get_file_id(&reader_uri)
            .expect("reader file id");

        let member_id = loop_write_member_id(&ws, writer_id);
        {
            let db = ws.analysis.compilation.get_db();
            let member_index = db.get_member_index();
            let member = member_index
                .get_member(&member_id)
                .expect("cold index must create the loop-body member");
            assert!(
                member_index.is_non_overwriting_assignment_member(member_id),
                "cold loop-body write must carry the non-overwriting mark; \
                 got non_overwriting=false for member {member_id:?} \
                 key={:?} owner={:?} feature={:?}",
                member.get_key(),
                member_index.get_member_owner(&member_id),
                member.get_feature(),
            );
        }

        let codes_before = codes_in(&ws, reader_id);
        let direct_before = decl_type(&ws, reader_id, "direct");
        let value_before = decl_type(&ws, reader_id, "value");

        // Production staged edit path for a semantic no-op: text first, then
        // re-index. It must move nothing the reader reads — including the
        // assignment-policy mark — and owe dependents no work.
        let noop = format!("{WRITER}-- unrelated touch\n");
        let staged = ws
            .analysis
            .update_file_text_only(&writer_uri, noop)
            .expect("staged file id");
        assert_eq!(staged, writer_id);
        let affected = ws.analysis.reindex_files(vec![writer_id]);
        assert!(
            !affected.contains(&reader_id),
            "unrelated EOF comment owes dependents no work, got {affected:?}"
        );

        // The EOF comment shifts no loop offset, so the member keeps its
        // identity and mark, and downstream facts are untouched.
        assert_eq!(loop_write_member_id(&ws, writer_id), member_id);
        assert!(is_non_overwriting(&ws, member_id));
        assert_eq!(codes_in(&ws, reader_id), codes_before);
        assert_eq!(decl_type(&ws, reader_id, "direct"), direct_before);
        assert_eq!(decl_type(&ws, reader_id, "value"), value_before);

        // Paired real edit: changing what the loop stores must still reach the
        // dependent, or the no-op assertions above could be met by never
        // invalidating anything.
        let staged = ws
            .analysis
            .update_file_text_only(&writer_uri, WRITER_CHANGED.to_string())
            .expect("staged file id");
        assert_eq!(staged, writer_id);
        let affected = ws.analysis.reindex_files(vec![writer_id]);
        assert!(
            affected.contains(&reader_id),
            "loop-semantics change must reach the target reader, got {affected:?}"
        );

        // The re-analysed workspace agrees with a fresh cold build of the same
        // texts: no stale dependent facts survive the propagation.
        let mut fresh = VirtualWorkspace::new_with_init_std_lib();
        let fresh_writer_uri = fresh.virtual_url_generator.new_uri("lua/a_writer.lua");
        let fresh_defs_uri = fresh.virtual_url_generator.new_uri("lua/b_defs.lua");
        let fresh_reader_uri = fresh.virtual_url_generator.new_uri("lua/c_reader.lua");
        fresh.analysis.update_files_by_uri(vec![
            (fresh_writer_uri, Some(WRITER_CHANGED.to_string())),
            (fresh_defs_uri, Some(DEFS.to_string())),
            (fresh_reader_uri.clone(), Some(READER.to_string())),
        ]);
        let fresh_reader_id = fresh
            .analysis
            .get_file_id(&fresh_reader_uri)
            .expect("fresh reader file id");
        assert_eq!(
            decl_type(&ws, reader_id, "value"),
            decl_type(&fresh, fresh_reader_id, "value")
        );
        assert_eq!(
            decl_type(&ws, reader_id, "direct"),
            decl_type(&fresh, fresh_reader_id, "direct")
        );
    }

    /// An inference moved through a middleman still refreshes the file that
    /// only names it.
    #[gtest]
    fn ripple_moved_inference_refreshes_its_lazy_reader() {
        // The provider changes the middleman's inferred global. Its actual
        // readers must then refresh their materialized type caches.
        let mut ws = workspace_with(vec![DiagnosticCode::AssignTypeMismatch]);
        let provider_uri = ws.virtual_url_generator.new_uri("lua/chainprovider.lua");
        let provider_id = write(&mut ws, &provider_uri, "Chain = { Value = 1 }\n");
        let middle_uri = ws.virtual_url_generator.new_uri("lua/chainmiddle.lua");
        write(&mut ws, &middle_uri, "Binferred = Chain.Value\n");
        let consumer_uri = ws.virtual_url_generator.new_uri("lua/chainconsumer.lua");
        let consumer_id = write(
            &mut ws,
            &consumer_uri,
            "local got = Binferred\n---@type integer\nlocal typed = Binferred\n",
        );
        let assert_reader_sees_text = |ws: &VirtualWorkspace, affected: &Vec<FileId>| {
            expect_that!(affected, contains(eq(&consumer_id)));
            let got = decl_type(ws, consumer_id, "got");
            expect_true!(
                matches!(&got, LuaType::StringConst(value) if value.as_str() == "text"),
                "lazy reader kept {got:?} instead of the provider's new string"
            );
            expect_that!(
                codes_in(ws, consumer_id),
                contains(eq(DiagnosticCode::AssignTypeMismatch.get_name()))
            );
        };

        // The editor's staged path: text first, index after.
        expect_that!(
            codes_in(&ws, consumer_id),
            not(contains(eq(DiagnosticCode::AssignTypeMismatch.get_name())))
        );
        ws.analysis
            .update_file_text_only(&provider_uri, "Chain = { Value = \"text\" }\n".to_string())
            .expect("staged provider text");
        let affected = ws.analysis.reindex_files(vec![provider_id]);
        assert_reader_sees_text(&ws, &affected);

        // Exercise the same chain through a watched-file update.
        write(&mut ws, &provider_uri, "Chain = { Value = 1 }\n");
        expect_that!(
            codes_in(&ws, consumer_id),
            not(contains(eq(DiagnosticCode::AssignTypeMismatch.get_name())))
        );
        let affected = ws.analysis.apply_file_system_changes(vec![(
            provider_uri.clone(),
            Some("Chain = { Value = \"text\" }\n".to_string()),
        )]);
        assert_reader_sees_text(&ws, &affected);
    }

    /// A contributed-param change reaches the callee's file.
    ///
    /// The caller contributes argument types to the callee's signature, which
    /// lives in the mixin. Changing the argument has to re-diagnose the mixin
    /// and leave it as a cold build of the same texts would.
    #[gtest]
    fn contributed_param_change_reaches_the_callee() {
        const CALLEE_PATH: &str = "lua/mixins/shared.lua";
        const CALLER_PATH: &str = "lua/autorun/consumer.lua";
        const CALLEE: &str = r#"
            local MIXIN = {}
            function MIXIN.Run(self)
                self:ProvidedByReceiver()
            end
            return MIXIN
            "#;
        let caller_text = |argument: &str| {
            format!(
                r#"
            CallerMarker = {{}}
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
        let mut ws = workspace_with(vec![DiagnosticCode::UndefinedField]);
        let callee_id = ws.def_file(CALLEE_PATH, CALLEE);
        let caller_uri = ws.virtual_url_generator.new_uri(CALLER_PATH);
        let caller_id = write(&mut ws, &caller_uri, &caller_text("self"));
        // The call has to actually contribute, or the edit below is vacuous.
        expect_that!(
            ws.analysis
                .compilation
                .get_db()
                .get_call_site_param_index()
                .iter_inferred_params()
                .count(),
            gt(0)
        );

        expect_that!(codes_in(&ws, callee_id), is_empty());

        ws.analysis
            .update_file_text_only(&caller_uri, caller_text("OTHER"))
            .expect("staged caller text");
        let affected = ws.analysis.reindex_files(vec![caller_id]);

        expect_that!(affected, contains(eq(&callee_id)));
        expect_that!(
            codes_in(&ws, callee_id),
            eq(&cold_codes(
                vec![DiagnosticCode::UndefinedField],
                &[(CALLEE_PATH, CALLEE), (CALLER_PATH, &caller_text("OTHER"))],
                CALLEE_PATH,
            ))
        );
    }

    /// The same for an assignment-form callee.
    ///
    /// `M.Fn = function() end` stores the signature in the member's type
    /// cache rather than sharing a property with it, unlike the function-stat
    /// (`function M.Fn() end`) form above.
    #[gtest]
    fn contributed_param_change_reaches_the_assignment_form_callee() {
        const CALLEE_PATH: &str = "lua/mixins/shared_assign.lua";
        const CALLER_PATH: &str = "lua/autorun/assignconsumer.lua";
        const CALLEE: &str = r#"
            local MIXIN = {}
            MIXIN.Run = function(self)
                self:ProvidedByReceiver()
            end
            return MIXIN
            "#;
        let caller_text = |argument: &str| {
            format!(
                r#"
            CallerMarker = {{}}
            local PANEL = {{}}
            local OTHER = {{}}
            function PANEL:ProvidedByReceiver() end
            function OTHER:SomethingElse() end
            function PANEL:Load()
                self.Mixin = include("mixins/shared_assign.lua")
            end
            function PANEL:Dispatch(name)
                local callback = self.Mixin[name]
                callback({argument})
            end
            "#
            )
        };
        let mut ws = workspace_with(vec![DiagnosticCode::UndefinedField]);
        let callee_id = ws.def_file(CALLEE_PATH, CALLEE);
        let caller_uri = ws.virtual_url_generator.new_uri(CALLER_PATH);
        let caller_id = write(&mut ws, &caller_uri, &caller_text("self"));
        expect_that!(
            ws.analysis
                .compilation
                .get_db()
                .get_call_site_param_index()
                .iter_inferred_params()
                .count(),
            gt(0)
        );

        expect_that!(codes_in(&ws, callee_id), is_empty());

        ws.analysis
            .update_file_text_only(&caller_uri, caller_text("OTHER"))
            .expect("staged caller text");
        let affected = ws.analysis.reindex_files(vec![caller_id]);

        expect_that!(affected, contains(eq(&callee_id)));
        expect_that!(
            codes_in(&ws, callee_id),
            eq(&cold_codes(
                vec![DiagnosticCode::UndefinedField],
                &[(CALLEE_PATH, CALLEE), (CALLER_PATH, &caller_text("OTHER"))],
                CALLEE_PATH,
            ))
        );
    }

    /// A late callee move propagates through a middleman to its cached reader.
    /// The affected-file report includes the diagnosed reader and remains unique.
    #[gtest]
    fn contributed_param_late_callee_move_refreshes_lazy_reader() {
        let caller_text = |argument: &str| {
            format!(
                r#"
            local PANEL = {{}}
            local OTHER = {{}}
            function PANEL:ProvidedByReceiver() end
            function OTHER:SomethingElse() end
            function PANEL:Load()
                self.Mixin = include("mixins/shared_late.lua")
            end
            function PANEL:Dispatch(name)
                local callback = self.Mixin[name]
                callback({argument})
            end
            "#
            )
        };
        let mut ws = workspace_with(vec![DiagnosticCode::AssignTypeMismatch]);
        ws.def_file(
            "lua/mixins/shared_late.lua",
            r#"
            local MIXIN = {}
            ParamLateHolder = {}
            function MIXIN.Run(self)
                ParamLateHolder.Out = self
            end
            return MIXIN
            "#,
        );
        let caller_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/lateconsumer.lua");
        let caller_id = write(&mut ws, &caller_uri, &caller_text("self"));
        let middle_uri = ws.virtual_url_generator.new_uri("lua/latemiddle.lua");
        let middle_id = write(
            &mut ws,
            &middle_uri,
            "MidLateInferred = ParamLateHolder.Out\n",
        );
        let reader_uri = ws.virtual_url_generator.new_uri("lua/latereader.lua");
        let reader_id = write(
            &mut ws,
            &reader_uri,
            "local got = MidLateInferred\n---@type integer\nlocal typed = MidLateInferred\n",
        );
        expect_that!(
            ws.analysis
                .compilation
                .get_db()
                .get_call_site_param_index()
                .iter_inferred_params()
                .count(),
            gt(0)
        );

        codes_in(&ws, reader_id);

        // The late callee repair must reach the reader through the middleman.
        let affected = ws
            .analysis
            .apply_file_system_changes(vec![(caller_uri.clone(), Some(caller_text("OTHER")))]);
        expect_that!(affected, contains(eq(&reader_id)));
        // A reader must not retain its earlier receiver type after the
        // middleman's inference changes.
        assert_eq!(
            decl_type(&ws, reader_id, "got"),
            decl_type(&ws, middle_id, "MidLateInferred")
        );
        let observed = decl_type(&ws, reader_id, "got");
        assert_eq!(observed, decl_type(&ws, caller_id, "OTHER"));
        ws.analysis.reindex();
        assert_eq!(observed, decl_type(&ws, reader_id, "got"));
        let mut sorted = affected.clone();
        sorted.sort_unstable();
        sorted.dedup();
        expect_that!(affected, eq(&sorted));
    }

    /// A removed non-name member reaches its owner's readers.
    ///
    /// The dropped slot's integer key is no Lua name, but its owner — the
    /// `Arr` global path — is. The reader of the slot has to be reported and
    /// end as a cold build of the same texts would.
    #[gtest]
    fn non_name_member_removal_refreshes_owner_referencers() {
        let mut ws = workspace_with(vec![DiagnosticCode::AssignTypeMismatch]);
        let first = "Arr = {}\nArr[1] = \"a\"\nArr[2] = \"b\"\n";
        let second = "Arr = {}\nArr[1] = \"a\"\n";
        let provider_uri = ws.virtual_url_generator.new_uri("lua/arrprovider.lua");
        let provider_id = write(&mut ws, &provider_uri, first);
        let consumer_uri = ws.virtual_url_generator.new_uri("lua/arrconsumer.lua");
        let consumer = "local observed = Arr\n---@type string\nlocal second = Arr[2]\n";
        let consumer_id = write(&mut ws, &consumer_uri, consumer);

        // The dropped slot really is a non-name member on a named owner, or
        // the refresh below would prove nothing.
        {
            let db = ws.analysis.compilation.get_db();
            let member_index = db.get_member_index();
            let dropped = member_index
                .get_file_members(provider_id)
                .iter()
                .find(|member| member.get_key() == &crate::LuaMemberKey::Integer(2))
                .map(|member| member.get_id())
                .expect("integer slot under test");
            let owner = member_index
                .get_member_owner(&dropped)
                .map(|owner| format!("{owner:?}"));
            expect_that!(owner, some(contains_substring("Arr")));
        }

        let cold = |provider: &str| {
            cold_codes(
                vec![DiagnosticCode::AssignTypeMismatch],
                &[
                    ("lua/arrprovider.lua", provider),
                    ("lua/arrconsumer.lua", consumer),
                ],
                "lua/arrconsumer.lua",
            )
        };
        expect_that!(codes_in(&ws, consumer_id), eq(&cold(first)));

        // The editor's staged path: text first, index after.
        ws.analysis
            .update_file_text_only(&provider_uri, second.to_string())
            .expect("staged provider text");
        let affected = ws.analysis.reindex_files(vec![provider_id]);
        expect_that!(affected, contains(eq(&consumer_id)));
        expect_that!(codes_in(&ws, consumer_id), eq(&cold(second)));

        // The same removal through a watched-file update.
        write(&mut ws, &provider_uri, first);
        expect_that!(codes_in(&ws, consumer_id), eq(&cold(first)));
        let affected = ws
            .analysis
            .apply_file_system_changes(vec![(provider_uri.clone(), Some(second.to_string()))]);
        expect_that!(affected, contains(eq(&consumer_id)));
        expect_that!(codes_in(&ws, consumer_id), eq(&cold(second)));
    }

    /// A same-position global rename keeps every position-derived identity,
    /// yet both the old and the new name's referencers owe a diagnostic
    /// refresh.
    #[gtest]
    fn global_function_rename_refreshes_old_and_new_referencers() {
        let mut ws = workspace_with(vec![DiagnosticCode::UndefinedGlobal]);
        let first = "function OldName() end\n";
        let second = "function NewName() end\n";
        let provider_uri = ws.virtual_url_generator.new_uri("lua/renamefunc.lua");
        let provider_id = write(&mut ws, &provider_uri, first);

        let old_consumer_uri = ws.virtual_url_generator.new_uri("lua/usesold.lua");
        let old_consumer_id = write(&mut ws, &old_consumer_uri, "OldName()\n");
        let new_consumer_uri = ws.virtual_url_generator.new_uri("lua/usesnew.lua");
        let new_consumer_id = write(&mut ws, &new_consumer_uri, "NewName()\n");
        expect_that!(codes_in(&ws, old_consumer_id), is_empty());
        expect_that!(
            codes_in(&ws, new_consumer_id),
            contains(eq(DiagnosticCode::UndefinedGlobal.get_name()))
        );

        // The editor's staged path: text first, index after.
        ws.analysis
            .update_file_text_only(&provider_uri, second.to_string())
            .expect("staged provider text");
        let affected = ws.analysis.reindex_files(vec![provider_id]);
        expect_that!(affected, contains(eq(&old_consumer_id)));
        expect_that!(affected, contains(eq(&new_consumer_id)));
        expect_that!(
            codes_in(&ws, old_consumer_id),
            contains(eq(DiagnosticCode::UndefinedGlobal.get_name()))
        );
        expect_that!(codes_in(&ws, new_consumer_id), is_empty());

        // The same rename through a watched-file update, from clean.
        write(&mut ws, &provider_uri, first);
        expect_that!(codes_in(&ws, old_consumer_id), is_empty());
        expect_that!(
            codes_in(&ws, new_consumer_id),
            contains(eq(DiagnosticCode::UndefinedGlobal.get_name()))
        );
        let affected = ws
            .analysis
            .apply_file_system_changes(vec![(provider_uri.clone(), Some(second.to_string()))]);
        expect_that!(affected, contains(eq(&old_consumer_id)));
        expect_that!(affected, contains(eq(&new_consumer_id)));
        expect_that!(
            codes_in(&ws, old_consumer_id),
            contains(eq(DiagnosticCode::UndefinedGlobal.get_name()))
        );
        expect_that!(codes_in(&ws, new_consumer_id), is_empty());
    }

    /// Re-adding a member must reach the files whose read missed while it
    /// was gone.
    ///
    /// A call statement's callee is only resolved by the net-flow pass, whose
    /// `NetCallResolver` infers through caches it owns and drops, so the miss
    /// is easy to lose and the stale `undefined-field` diagnostics stay.
    #[gtest]
    fn re_added_member_reaches_readers_that_missed_it() {
        let mut ws = workspace_with(vec![DiagnosticCode::UndefinedField]);
        let with_get = r#"
            ns = ns or {}
            ns.data = ns.data or {}
            function ns.data.Get(key)
                if key then
                    return {}
                end
            end
        "#;
        let without_get = r#"
            ns = ns or {}
            ns.data = ns.data or {}
        "#;
        let provider_uri = ws.virtual_url_generator.new_uri("lua/missprovider.lua");
        let provider_id = write(&mut ws, &provider_uri, with_get);
        let consumer_uri = ws.virtual_url_generator.new_uri("lua/missconsumer.lua");
        // A call statement, not an initializer: the callee read happens in the
        // net-flow pass's detached resolver caches, outside any
        // settled-candidate record.
        let consumer_source = r#"
            ns.data.Get("options")
        "#;
        let consumer_id = write(&mut ws, &consumer_uri, consumer_source);

        expect_that!(
            codes_in(&ws, consumer_id),
            not(contains(eq(DiagnosticCode::UndefinedField.get_name())))
        );

        // Remove the member, then re-analyse the consumer against the missing
        // slot: its `ns.data.Get` read misses and must be filed as a
        // missed-member read on the owner.
        ws.analysis
            .update_file_text_only(&provider_uri, without_get.to_string())
            .expect("staged provider text");
        ws.analysis.reindex_files(vec![provider_id]);
        ws.analysis
            .update_file_text_only(&consumer_uri, consumer_source.to_string())
            .expect("staged consumer text");
        ws.analysis.reindex_files(vec![consumer_id]);
        expect_that!(
            codes_in(&ws, consumer_id),
            contains(eq(DiagnosticCode::UndefinedField.get_name()))
        );

        // Restore it: the missed read must reach the consumer.
        ws.analysis
            .update_file_text_only(&provider_uri, with_get.to_string())
            .expect("staged provider text");
        let affected = ws.analysis.reindex_files(vec![provider_id]);
        expect_that!(affected, contains(eq(&consumer_id)));
        expect_that!(
            codes_in(&ws, consumer_id),
            not(contains(eq(DiagnosticCode::UndefinedField.get_name())))
        );
    }

    /// The same missed read through a declaration initializer — the
    /// shape the CityRP consumers use (`local options = ns.data.Get(...)`).
    /// Restoring the member must reach the reader.
    #[gtest]
    fn re_added_member_reaches_initializer_readers_that_missed_it() {
        let mut ws = workspace_with(vec![DiagnosticCode::UndefinedField]);
        let with_get = r#"
            ns = ns or {}
            ns.data = ns.data or {}
            function ns.data.Get(key, default)
                return default
            end
        "#;
        let without_get = r#"
            ns = ns or {}
            ns.data = ns.data or {}
        "#;
        let provider_uri = ws.virtual_url_generator.new_uri("lua/missprovider.lua");
        let provider_id = write(&mut ws, &provider_uri, with_get);
        let consumer_uri = ws.virtual_url_generator.new_uri("lua/missconsumer.lua");
        let consumer_source = r#"
            local options = ns.data.Get("options", nil)
            return options
        "#;
        let consumer_id = write(&mut ws, &consumer_uri, consumer_source);

        expect_that!(
            codes_in(&ws, consumer_id),
            not(contains(eq(DiagnosticCode::UndefinedField.get_name())))
        );

        ws.analysis
            .update_file_text_only(&provider_uri, without_get.to_string())
            .expect("staged provider text");
        ws.analysis.reindex_files(vec![provider_id]);
        ws.analysis
            .update_file_text_only(&consumer_uri, consumer_source.to_string())
            .expect("staged consumer text");
        ws.analysis.reindex_files(vec![consumer_id]);
        expect_that!(
            codes_in(&ws, consumer_id),
            contains(eq(DiagnosticCode::UndefinedField.get_name()))
        );

        ws.analysis
            .update_file_text_only(&provider_uri, with_get.to_string())
            .expect("staged provider text");
        let affected = ws.analysis.reindex_files(vec![provider_id]);
        expect_that!(affected, contains(eq(&consumer_id)));
        expect_that!(
            codes_in(&ws, consumer_id),
            not(contains(eq(DiagnosticCode::UndefinedField.get_name())))
        );
    }

    /// Renaming a member in place must reach the files whose read missed
    /// the old name while it was gone.
    ///
    /// `ns.data.Get` -> `ns.data.GetDisabled` keeps the index-expression site,
    /// so the member id survives the edit; the rename back must still be seen
    /// as the `Get` slot reappearing by the readers that missed it.
    #[gtest]
    fn renamed_member_reaches_readers_that_missed_it() {
        let mut ws = workspace_with(vec![DiagnosticCode::UndefinedField]);
        let with_get = r#"
            ns = ns or {}
            ns.data = ns.data or {}
            function ns.data.Get(key, default)
                if key then
                    return default or {}
                end
            end
        "#;
        let renamed = r#"
            ns = ns or {}
            ns.data = ns.data or {}
            function ns.data.GetDisabled(key, default)
                if key then
                    return default or {}
                end
            end
        "#;
        let provider_uri = ws.virtual_url_generator.new_uri("lua/missprovider.lua");
        let provider_id = write(&mut ws, &provider_uri, with_get);
        let consumer_uri = ws.virtual_url_generator.new_uri("lua/missconsumer.lua");
        let consumer_source = r#"
            local options = ns.data.Get("options", nil)
            return options
        "#;
        let consumer_id = write(&mut ws, &consumer_uri, consumer_source);

        expect_that!(
            codes_in(&ws, consumer_id),
            not(contains(eq(DiagnosticCode::UndefinedField.get_name())))
        );

        // Rename the member away, then re-analyse the consumer against the
        // missing slot so its `ns.data.Get` miss is filed on the owner.
        ws.analysis
            .update_file_text_only(&provider_uri, renamed.to_string())
            .expect("staged provider text");
        ws.analysis.reindex_files(vec![provider_id]);
        ws.analysis
            .update_file_text_only(&consumer_uri, consumer_source.to_string())
            .expect("staged consumer text");
        ws.analysis.reindex_files(vec![consumer_id]);
        expect_that!(
            codes_in(&ws, consumer_id),
            contains(eq(DiagnosticCode::UndefinedField.get_name()))
        );

        // Rename it back: the missed read must reach the consumer.
        ws.analysis
            .update_file_text_only(&provider_uri, with_get.to_string())
            .expect("staged provider text");
        let affected = ws.analysis.reindex_files(vec![provider_id]);
        expect_that!(affected, contains(eq(&consumer_id)));
        expect_that!(
            codes_in(&ws, consumer_id),
            not(contains(eq(DiagnosticCode::UndefinedField.get_name())))
        );
    }

    /// A dependency cycle must revisit the file the edit started from.
    ///
    /// `cycle_a` reads `cycle_b`'s inferred return while `cycle_b` reads
    /// `cycle_a`'s field. Editing `cycle_a` re-analyses it against
    /// `cycle_b`'s still-current signature, then `cycle_b` re-analyses and
    /// its signature moves — naming `cycle_a` as a dependent again. An edit
    /// that settles each file at most once drops that second visit, so `res`
    /// keeps the type the pre-edit `cycle_b` reported. A cold workspace over
    /// the same sources resolves the post-edit type.
    #[test]
    fn edit_in_a_cycle_revisits_the_edited_file() {
        const A_BEFORE: &str = "CycleA = { input = 1 }\nlocal res = CycleB.Compute()";
        const A_AFTER: &str = "CycleA = { input = \"text\" }\nlocal res = CycleB.Compute()";
        const B: &str = "CycleB = {}\nfunction CycleB.Compute() return CycleA.input end";

        let mut warm = VirtualWorkspace::new_with_init_std_lib();
        let a_uri = warm.virtual_url_generator.new_uri("lua/cycle_a.lua");
        let b_uri = warm.virtual_url_generator.new_uri("lua/cycle_b.lua");
        warm.analysis.update_files_by_uri(vec![
            (a_uri.clone(), Some(A_BEFORE.to_string())),
            (b_uri.clone(), Some(B.to_string())),
        ]);

        let a_id = write_deferred(&mut warm, &a_uri, A_AFTER);

        let mut cold = VirtualWorkspace::new_with_init_std_lib();
        let cold_a_uri = cold.virtual_url_generator.new_uri("lua/cycle_a.lua");
        let cold_b_uri = cold.virtual_url_generator.new_uri("lua/cycle_b.lua");
        cold.analysis.update_files_by_uri(vec![
            (cold_a_uri.clone(), Some(A_AFTER.to_string())),
            (cold_b_uri, Some(B.to_string())),
        ]);
        let cold_a_id = cold
            .analysis
            .get_file_id(&cold_a_uri)
            .expect("cold a file id");

        let warm_res = decl_type(&warm, a_id, "res");
        assert!(
            matches!(
                &warm_res,
                LuaType::String | LuaType::StringConst(_) | LuaType::DocStringConst(_)
            ),
            "the edited file's own read should follow its reader's new return, got {warm_res:?}"
        );
        assert_eq!(warm_res, decl_type(&cold, cold_a_id, "res"));
    }

    /// A file the batch path has never seen has no dependency edges pointing
    /// at it, yet the readers whose lookups missed before it existed must be
    /// re-analysed when the provider arrives.
    #[gtest]
    fn batch_ingested_global_reaches_readers_indexed_before_it() {
        let mut ws = workspace_with(vec![DiagnosticCode::UndefinedGlobalAssignment]);
        let reader_uri = ws.virtual_url_generator.new_uri("lua/batch_reader.lua");
        let reader_id = write(&mut ws, &reader_uri, "local observed = BatchGlobal\n");
        let before = decl_type(&ws, reader_id, "observed");
        assert!(
            before.is_unknown(),
            "reader should miss the not-yet-ingested global, got {before:?}"
        );
        expect_that!(
            codes_in(&ws, reader_id),
            contains(eq(DiagnosticCode::UndefinedGlobalAssignment.get_name()))
        );

        let provider_uri = ws.virtual_url_generator.new_uri("lua/batch_provider.lua");
        let reindexed = ws.analysis.update_files_by_uri(vec![(
            provider_uri,
            Some("BatchGlobal = \"provided\"\n".to_string()),
        )]);
        assert!(
            reindexed.contains(&reader_id),
            "the missed reader must be reindexed, got {reindexed:?}"
        );

        let mut cold = workspace_with(vec![DiagnosticCode::UndefinedGlobalAssignment]);
        let cold_reader_uri = cold.virtual_url_generator.new_uri("lua/batch_reader.lua");
        let cold_provider_uri = cold.virtual_url_generator.new_uri("lua/batch_provider.lua");
        cold.analysis.update_files_by_uri(vec![
            (
                cold_reader_uri.clone(),
                Some("local observed = BatchGlobal\n".to_string()),
            ),
            (
                cold_provider_uri,
                Some("BatchGlobal = \"provided\"\n".to_string()),
            ),
        ]);
        let cold_reader_id = cold
            .analysis
            .get_file_id(&cold_reader_uri)
            .expect("cold reader file id");

        let after = decl_type(&ws, reader_id, "observed");
        assert!(
            matches!(&after, LuaType::StringConst(value) if value.as_str() == "provided"),
            "the new global should reach the earlier reader, got {after:?}"
        );
        assert_eq!(after, decl_type(&cold, cold_reader_id, "observed"));
        expect_that!(
            codes_in(&ws, reader_id),
            not(contains(eq(
                DiagnosticCode::UndefinedGlobalAssignment.get_name()
            )))
        );
    }

    /// A `---@type` name that resolved to nothing is filed under the would-be
    /// global type id, so the new declaration's readers are found through the
    /// type-reference index, not the name index.
    #[gtest]
    fn batch_ingested_class_reaches_unresolved_doc_type_readers() {
        let mut ws = workspace_with(vec![DiagnosticCode::TypeNotFound]);
        let reader_uri = ws.virtual_url_generator.new_uri("lua/batch_doc_reader.lua");
        let reader_id = write(
            &mut ws,
            &reader_uri,
            "---@type BatchDocType\nlocal observed\nlocal field = observed.value\n",
        );
        expect_that!(
            codes_in(&ws, reader_id),
            contains(eq(DiagnosticCode::TypeNotFound.get_name()))
        );

        let provider_uri = ws
            .virtual_url_generator
            .new_uri("lua/batch_doc_provider.lua");
        let reindexed = ws.analysis.update_files_by_uri(vec![(
            provider_uri,
            Some("---@class BatchDocType\n---@field value string\n".to_string()),
        )]);
        assert!(
            reindexed.contains(&reader_id),
            "the unresolved doc-type reader must be reindexed, got {reindexed:?}"
        );

        let mut cold = workspace_with(vec![DiagnosticCode::TypeNotFound]);
        let cold_reader_uri = cold
            .virtual_url_generator
            .new_uri("lua/batch_doc_reader.lua");
        let cold_provider_uri = cold
            .virtual_url_generator
            .new_uri("lua/batch_doc_provider.lua");
        cold.analysis.update_files_by_uri(vec![
            (
                cold_reader_uri.clone(),
                Some(
                    "---@type BatchDocType\nlocal observed\nlocal field = observed.value\n"
                        .to_string(),
                ),
            ),
            (
                cold_provider_uri,
                Some("---@class BatchDocType\n---@field value string\n".to_string()),
            ),
        ]);
        let cold_reader_id = cold
            .analysis
            .get_file_id(&cold_reader_uri)
            .expect("cold reader file id");

        let field = decl_type(&ws, reader_id, "field");
        assert_eq!(field, decl_type(&cold, cold_reader_id, "field"));
        assert!(
            matches!(&field, LuaType::String),
            "the new class should reach the earlier doc-type reader, got {field:?}"
        );
        expect_that!(
            codes_in(&ws, reader_id),
            not(contains(eq(DiagnosticCode::TypeNotFound.get_name())))
        );
    }

    /// A member that joins an owner another file already failed to read is
    /// found through the missed-member-reader edges, the same index the
    /// single-file edit path consults for structural adds.
    #[gtest]
    fn batch_ingested_member_reaches_missed_member_readers() {
        let mut ws = workspace_with(vec![DiagnosticCode::UndefinedField]);
        ws.def("ns = ns or {}\nns.data = ns.data or {}\n");
        let reader_uri = ws
            .virtual_url_generator
            .new_uri("lua/batch_member_reader.lua");
        let reader_id = write(
            &mut ws,
            &reader_uri,
            "local options = ns.data.Get(\"options\")\n",
        );
        expect_that!(
            codes_in(&ws, reader_id),
            contains(eq(DiagnosticCode::UndefinedField.get_name()))
        );

        let provider_uri = ws
            .virtual_url_generator
            .new_uri("lua/batch_member_provider.lua");
        let reindexed = ws.analysis.update_files_by_uri(vec![(
            provider_uri,
            Some("function ns.data.Get(key) return key end\n".to_string()),
        )]);
        assert!(
            reindexed.contains(&reader_id),
            "the missed member reader must be reindexed, got {reindexed:?}"
        );

        let options = decl_type(&ws, reader_id, "options");
        assert!(
            matches!(&options, LuaType::StringConst(value) if value.as_str() == "options"),
            "the new member should reach the earlier reader, got {options:?}"
        );
        expect_that!(
            codes_in(&ws, reader_id),
            not(contains(eq(DiagnosticCode::UndefinedField.get_name())))
        );
    }

    /// A `require` that resolved to nothing files an unresolved-path edge
    /// keyed by the module path, which the batch expansion already consults:
    /// the reader must be reindexed exactly once, through that edge, not
    /// through a second scheduling pass.
    #[gtest]
    fn batch_ingested_file_resolves_unresolved_require_readers() {
        let mut ws = workspace_with(vec![]);
        let reader_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/batch_req_reader.lua");
        let reader_id = write(&mut ws, &reader_uri, "local inc = require(\"batchmod\")\n");
        codes_in(&ws, reader_id);

        let provider_uri = ws.virtual_url_generator.new_uri("lua/batchmod.lua");
        let reindexed = ws.analysis.update_files_by_uri(vec![(
            provider_uri,
            Some("return \"loaded\"\n".to_string()),
        )]);
        assert!(
            reindexed.contains(&reader_id),
            "the unresolved require reader must be reindexed, got {reindexed:?}"
        );

        let mut cold = workspace_with(vec![]);
        let cold_reader_uri = cold
            .virtual_url_generator
            .new_uri("lua/autorun/batch_req_reader.lua");
        let cold_provider_uri = cold.virtual_url_generator.new_uri("lua/batchmod.lua");
        cold.analysis.update_files_by_uri(vec![
            (
                cold_reader_uri.clone(),
                Some("local inc = require(\"batchmod\")\n".to_string()),
            ),
            (cold_provider_uri, Some("return \"loaded\"\n".to_string())),
        ]);
        let cold_reader_id = cold
            .analysis
            .get_file_id(&cold_reader_uri)
            .expect("cold reader file id");

        let inc = decl_type(&ws, reader_id, "inc");
        assert_eq!(inc, decl_type(&cold, cold_reader_id, "inc"));
        assert!(
            matches!(&inc, LuaType::StringConst(value) if value.as_str() == "loaded"),
            "the resolved require should reach the earlier reader, got {inc:?}"
        );
    }

    /// The unrelated-file control: a file that never read anything the new
    /// file provides keeps its index — its reference revision does not move —
    /// while the missed reader in the same batch does get reindexed.
    #[gtest]
    fn batch_ingested_file_leaves_unrelated_files_untouched() {
        let mut ws = workspace_with(vec![DiagnosticCode::UndefinedGlobalAssignment]);
        let unrelated_uri = ws.virtual_url_generator.new_uri("lua/batch_unrelated.lua");
        let unrelated_id = write(
            &mut ws,
            &unrelated_uri,
            "local untouched = 1\nlocal doubled = untouched + untouched\n",
        );
        let reader_uri = ws.virtual_url_generator.new_uri("lua/batch_reader.lua");
        let reader_id = write(&mut ws, &reader_uri, "local observed = BatchGlobal\n");
        codes_in(&ws, unrelated_id);
        codes_in(&ws, reader_id);
        let unrelated_revision = revision_of(&ws, unrelated_id);
        let unrelated_observed = decl_type(&ws, unrelated_id, "doubled");

        let provider_uri = ws.virtual_url_generator.new_uri("lua/batch_provider.lua");
        let reindexed = ws.analysis.update_files_by_uri(vec![(
            provider_uri,
            Some("BatchGlobal = \"provided\"\n".to_string()),
        )]);

        assert!(
            reindexed.contains(&reader_id),
            "the missed reader must be reindexed, got {reindexed:?}"
        );
        assert!(
            !reindexed.contains(&unrelated_id),
            "an unrelated file must not be reindexed, got {reindexed:?}"
        );
        assert_eq!(
            revision_of(&ws, unrelated_id),
            unrelated_revision,
            "an unrelated file's references must not be rewritten"
        );
        assert_eq!(decl_type(&ws, unrelated_id, "doubled"), unrelated_observed);
    }

    /// A batch deletion leaves a tombstone, so re-adding the same URI is an
    /// appearance, not an edit: the readers that missed while it was gone
    /// must be reached again.
    #[gtest]
    fn batch_deleted_then_readded_file_restores_its_readers() {
        let mut ws = workspace_with(vec![DiagnosticCode::UndefinedGlobalAssignment]);
        let provider_uri = ws
            .virtual_url_generator
            .new_uri("lua/batch_reopen_provider.lua");
        write(&mut ws, &provider_uri, "ReopenGlobal = \"first\"\n");
        let reader_uri = ws
            .virtual_url_generator
            .new_uri("lua/batch_reopen_reader.lua");
        let reader_id = write(&mut ws, &reader_uri, "local observed = ReopenGlobal\n");
        expect_that!(
            codes_in(&ws, reader_id),
            not(contains(eq(
                DiagnosticCode::UndefinedGlobalAssignment.get_name()
            )))
        );

        ws.analysis
            .update_files_by_uri(vec![(provider_uri.clone(), None)]);
        expect_that!(
            codes_in(&ws, reader_id),
            contains(eq(DiagnosticCode::UndefinedGlobalAssignment.get_name()))
        );

        let reindexed = ws.analysis.update_files_by_uri(vec![(
            provider_uri,
            Some("ReopenGlobal = \"second\"\n".to_string()),
        )]);
        assert!(
            reindexed.contains(&reader_id),
            "the reader of a re-added file must be reindexed, got {reindexed:?}"
        );
        let observed = decl_type(&ws, reader_id, "observed");
        assert!(
            matches!(&observed, LuaType::StringConst(value) if value.as_str() == "second"),
            "the re-added file should reach its reader, got {observed:?}"
        );
        expect_that!(
            codes_in(&ws, reader_id),
            not(contains(eq(
                DiagnosticCode::UndefinedGlobalAssignment.get_name()
            )))
        );
    }

    /// A declaration introduced into a file that was already indexed is a
    /// structural addition too: the readers that missed the name while the
    /// provider lacked it recorded no edge the expansion can follow.
    #[gtest]
    fn batch_edit_introducing_a_declaration_reaches_missed_readers() {
        let mut ws = workspace_with(vec![DiagnosticCode::UndefinedGlobalAssignment]);
        let provider_uri = ws
            .virtual_url_generator
            .new_uri("lua/batch_edit_provider.lua");
        write(&mut ws, &provider_uri, "local keep = 1\n");
        let reader_uri = ws
            .virtual_url_generator
            .new_uri("lua/batch_edit_reader.lua");
        let reader_id = write(&mut ws, &reader_uri, "local observed = LateGlobal\n");
        expect_that!(
            codes_in(&ws, reader_id),
            contains(eq(DiagnosticCode::UndefinedGlobalAssignment.get_name()))
        );

        let reindexed = ws.analysis.update_files_by_uri(vec![(
            provider_uri,
            Some("local keep = 1\nLateGlobal = \"late\"\n".to_string()),
        )]);
        assert!(
            reindexed.contains(&reader_id),
            "the missed reader must be reindexed, got {reindexed:?}"
        );
        let observed = decl_type(&ws, reader_id, "observed");
        assert!(
            matches!(&observed, LuaType::StringConst(value) if value.as_str() == "late"),
            "the introduced declaration should reach its reader, got {observed:?}"
        );
        expect_that!(
            codes_in(&ws, reader_id),
            not(contains(eq(
                DiagnosticCode::UndefinedGlobalAssignment.get_name()
            )))
        );
    }

    /// file → the call-site records it currently owns, rendered for an exact
    /// set comparison. Presence is part of the contract: the index stores an
    /// entry only while the file holds records, so a file that recollects
    /// nothing must come back with no entry, not an empty vec.
    fn call_site_records(ws: &VirtualWorkspace) -> Vec<(FileId, Vec<String>)> {
        let mut records = ws
            .analysis
            .compilation
            .get_db()
            .get_call_site_param_index()
            .iter_file_contributions()
            .map(|(file_id, contributions)| {
                (
                    *file_id,
                    contributions
                        .into_iter()
                        .map(|contribution| format!("{contribution:?}"))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        records.sort_by_key(|(file_id, _)| *file_id);
        records
    }

    /// An edit that is then reverted must restore the call-site index exactly:
    /// every file's contribution records, including which files hold an entry
    /// at all. The bystander below owns no records, so its
    /// presence in the maps is the canary for the removal/recollection
    /// lifecycle leaving empty entries behind.
    #[gtest]
    fn edit_revert_restores_call_site_record_sets() {
        let mut ws = workspace_with(vec![DiagnosticCode::AssignTypeMismatch]);
        let provider_uri = ws
            .virtual_url_generator
            .new_uri("lua/call_site_provider.lua");
        let provider_v1 = r#"
            provider = provider or {}
            ---@return string
            ---@return number
            function provider.Describe() end
            function provider.Apply(value) end
        "#;
        let provider_v2 = r#"
            provider = provider or {}
            ---@return number
            ---@return number
            function provider.Describe() end
            function provider.Apply(value) end
        "#;
        write(&mut ws, &provider_uri, provider_v1);

        // A reader of the edited export: the provider's return-type change
        // re-analyses it.
        let consumer_uri = ws
            .virtual_url_generator
            .new_uri("lua/call_site_consumer.lua");
        write(
            &mut ws,
            &consumer_uri,
            r#"
            local name, count = provider.Describe()
            provider.Apply("literal")
            "#,
        );

        // Owns no call-site records and reads nothing the edit moves, so the
        // edit never re-analyses it.
        let bystander_uri = ws
            .virtual_url_generator
            .new_uri("lua/call_site_bystander.lua");
        write(&mut ws, &bystander_uri, "local untouched = 1\n");

        let before = call_site_records(&ws);

        write(&mut ws, &provider_uri, provider_v2);
        write(&mut ws, &provider_uri, provider_v1);

        let after = call_site_records(&ws);
        assert_eq!(
            after, before,
            "call-site contributions differ after an edit-revert"
        );
    }

    /// A same-batch reader of a pre-existing file whose export the batch
    /// repairs is a downstream dependent of that repair, not a missed reader:
    /// the consumer indexed `Bridge` while it still resolved to nothing, so
    /// once the provider lands and the bridge is re-analysed, the consumer's
    /// cached read is stale even though it arrived in the same batch.
    #[gtest]
    fn batch_ingested_provider_repairs_same_batch_consumers_of_a_bridge() {
        let mut ws = workspace_with(vec![]);
        let bridge_uri = ws.virtual_url_generator.new_uri("lua/batch_bridge.lua");
        write(&mut ws, &bridge_uri, "Bridge = LateGlobal\n");

        let provider_uri = ws
            .virtual_url_generator
            .new_uri("lua/batch_bridge_provider.lua");
        let consumer_uri = ws
            .virtual_url_generator
            .new_uri("lua/batch_bridge_consumer.lua");
        ws.analysis.update_files_by_uri(vec![
            (provider_uri, Some("LateGlobal = \"ready\"\n".to_string())),
            (
                consumer_uri.clone(),
                Some("local observed = Bridge\n".to_string()),
            ),
        ]);

        let consumer_id = ws
            .analysis
            .get_file_id(&consumer_uri)
            .expect("consumer file id");

        let mut cold = workspace_with(vec![]);
        let cold_bridge_uri = cold.virtual_url_generator.new_uri("lua/batch_bridge.lua");
        let cold_provider_uri = cold
            .virtual_url_generator
            .new_uri("lua/batch_bridge_provider.lua");
        let cold_consumer_uri = cold
            .virtual_url_generator
            .new_uri("lua/batch_bridge_consumer.lua");
        cold.analysis.update_files_by_uri(vec![
            (cold_bridge_uri, Some("Bridge = LateGlobal\n".to_string())),
            (
                cold_provider_uri,
                Some("LateGlobal = \"ready\"\n".to_string()),
            ),
            (
                cold_consumer_uri.clone(),
                Some("local observed = Bridge\n".to_string()),
            ),
        ]);
        let cold_consumer_id = cold
            .analysis
            .get_file_id(&cold_consumer_uri)
            .expect("cold consumer file id");

        let observed = decl_type(&ws, consumer_id, "observed");
        assert!(
            matches!(&observed, LuaType::StringConst(value) if value.as_str() == "ready"),
            "the consumer should see the repaired bridge's type, got {observed:?}"
        );
        assert_eq!(observed, decl_type(&cold, cold_consumer_id, "observed"));
    }

    /// Text stored without re-analysis is not taken in until a re-analysis
    /// takes it, so nothing diagnoses it against the facts of the old text.
    #[gtest]
    fn stored_text_counts_as_taken_once_a_reanalysis_takes_it() {
        let mut ws = VirtualWorkspace::new();
        let path = "lua/autorun/sh_taken.lua";
        let file_id = ws.def_file(
            path,
            "local a = 1
",
        );
        expect_that!(ws.analysis.compilation.has_taken_text(file_id), eq(true));

        let uri = ws.virtual_url_generator.new_uri(path);
        ws.analysis.update_file_text_only(
            &uri,
            "local a = 2
"
            .to_string(),
        );
        expect_that!(ws.analysis.compilation.has_taken_text(file_id), eq(false));

        ws.analysis.reindex_files(vec![file_id]);
        expect_that!(ws.analysis.compilation.has_taken_text(file_id), eq(true));
    }

    const CANCEL_PRODUCER: &str = "lua/autorun/sh_cancel_producer.lua";
    const CANCEL_CONSUMER: &str = "lua/autorun/sh_cancel_consumer.lua";
    const CANCEL_CONSUMER_TEXT: &str = "---@type number\nlocal n = CancelValue\n";

    /// A diagnosis cancelled after one that finished leaves that report's
    /// reads: changing what it read still reports the consumer affected, and
    /// it settles to what a cold build reports.
    #[gtest]
    fn a_cancelled_diagnosis_keeps_the_last_reports_reads() {
        let codes = vec![DiagnosticCode::AssignTypeMismatch];
        let mut ws = workspace_with(codes.clone());
        let producer_uri = ws.virtual_url_generator.new_uri(CANCEL_PRODUCER);
        write(
            &mut ws,
            &producer_uri,
            "---@type string\nCancelValue = 's'\n",
        );
        let consumer_uri = ws.virtual_url_generator.new_uri(CANCEL_CONSUMER);
        let consumer_id = write(&mut ws, &consumer_uri, CANCEL_CONSUMER_TEXT);
        expect_that!(
            codes_in(&ws, consumer_id),
            contains(eq(DiagnosticCode::AssignTypeMismatch.get_name()))
        );

        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(ws.analysis.diagnose_file(consumer_id, cancel).is_none());

        let producer_text = "---@type number\nCancelValue = 1\n";
        let affected = ws
            .analysis
            .apply_file_system_changes(vec![(producer_uri, Some(producer_text.to_string()))]);
        expect_that!(affected, contains(eq(&consumer_id)));
        expect_that!(
            codes_in(&ws, consumer_id),
            eq(&cold_codes(
                codes,
                &[
                    (CANCEL_PRODUCER, producer_text),
                    (CANCEL_CONSUMER, CANCEL_CONSUMER_TEXT)
                ],
                CANCEL_CONSUMER
            ))
        );
    }
}
