#[cfg(test)]
mod test {
    use std::collections::BTreeSet;

    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{DiagnosticCode, GmodLoadStatus, VirtualWorkspace};

    fn undefined_global_names(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
    ) -> BTreeSet<String> {
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::UndefinedGlobal);
        let code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedGlobal.get_name().to_string(),
        ));

        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| diagnostic.code == code)
            .filter_map(|diagnostic| {
                diagnostic
                    .message
                    .strip_prefix("undefined global variable: ")
                    .map(str::to_string)
            })
            .collect()
    }

    fn file_id(ws: &VirtualWorkspace, path: &str) -> crate::FileId {
        ws.analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_all_local_file_ids()
            .into_iter()
            .find(|file_id| {
                ws.analysis
                    .compilation
                    .get_db()
                    .get_vfs()
                    .get_file_path(file_id)
                    .is_some_and(|file_path| {
                        file_path
                            .to_string_lossy()
                            .replace('\\', "/")
                            .ends_with(path)
                    })
            })
            .expect("test file exists")
    }

    #[test]
    fn compilefile_chunk_uses_environment_assigned_by_setfenv() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![
            (
                "lua/starfall/permissions/providers_sh/url_whitelist.lua",
                r#"
                local function runWhitelist(filename, func)
                    local env = {
                        pattern = function(txt) end,
                        simple = function(txt) end,
                        blacklist = function(txt) end,
                        blacklistpattern = function(txt) end,
                    }

                    setfenv(func, env)
                    pcall(func)
                end

                local function loadDefaultWhitelist()
                    local filename = "starfall/starfall_whitelist_default.lua"
                    local func = CompileFile(filename)
                    if func then
                        runWhitelist(filename, func)
                    end
                end

                loadDefaultWhitelist()
                "#,
            ),
            (
                "lua/starfall/starfall_whitelist_default.lua",
                r#"
                simple [[raw.githubusercontent.com]]
                pattern [[avatars(%d*)%.githubusercontent%.com/(.+)]]
                blacklist [[steamcommunity.com/linkfilter]]
                "#,
            ),
            (
                "lua/starfall/unrelated.lua",
                r#"
                simple [[this-file-was-not-compiled]]
                "#,
            ),
        ]);

        let target_id = file_id(&ws, "lua/starfall/starfall_whitelist_default.lua");
        let unrelated_id = file_id(&ws, "lua/starfall/unrelated.lua");
        let compiled_target_undefined = undefined_global_names(&mut ws, target_id);
        let unrelated_target_undefined = undefined_global_names(&mut ws, unrelated_id);

        assert_eq!(
            (compiled_target_undefined, unrelated_target_undefined),
            (BTreeSet::new(), BTreeSet::from(["simple".to_string()])),
        );

        let load_info = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_load_index()
            .get_file_info(&target_id)
            .expect("compiled target has fallback load metadata");
        assert_eq!(load_info.status, GmodLoadStatus::NoKnownLoadPath);
        assert!(load_info.incoming_edges.is_empty());
    }

    #[test]
    fn environment_suppresses_only_statically_present_fields() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![
            (
                "lua/loader.lua",
                r#"
                local chunk = CompileFile("target.lua")
                setfenv(chunk, { simple = true })
                "#,
            ),
            ("lua/target.lua", "simple()\nmissing()"),
        ]);

        let target_id = file_id(&ws, "lua/target.lua");
        assert_eq!(
            undefined_global_names(&mut ws, target_id),
            BTreeSet::from(["missing".to_string()]),
        );
    }

    #[test]
    fn same_spelling_without_metadata_does_not_define_an_environment() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_files(vec![
            (
                "lua/fake.lua",
                r#"
                function CompileFile(path) return function() end end
                function setfenv(target, environment) end
                local chunk = CompileFile("target.lua")
                setfenv(chunk, { simple = true })
                "#,
            ),
            ("lua/target.lua", "simple()"),
        ]);

        let target_id = file_id(&ws, "lua/target.lua");
        assert_eq!(
            undefined_global_names(&mut ws, target_id),
            BTreeSet::from(["simple".to_string()]),
        );
    }

    #[test]
    fn reassigned_or_ambiguous_chunk_is_not_propagated() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![
            (
                "lua/loader.lua",
                r#"
                local chunk = CompileFile("first.lua")
                chunk = CompileFile("second.lua")
                setfenv(chunk, { simple = true })
                "#,
            ),
            ("lua/first.lua", "simple()"),
            ("lua/second.lua", "simple()"),
        ]);

        let first_id = file_id(&ws, "lua/first.lua");
        let second_id = file_id(&ws, "lua/second.lua");
        assert_eq!(
            (
                undefined_global_names(&mut ws, first_id),
                undefined_global_names(&mut ws, second_id),
            ),
            (
                BTreeSet::from(["simple".to_string()]),
                BTreeSet::from(["simple".to_string()]),
            ),
        );
    }

    #[test]
    fn wrapper_parameter_receiving_multiple_chunks_is_not_propagated() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![
            (
                "lua/loader.lua",
                r#"
                local function applyEnvironment(chunk)
                    setfenv(chunk, { simple = true })
                end
                applyEnvironment(CompileFile("first.lua"))
                applyEnvironment(CompileFile("second.lua"))
                "#,
            ),
            ("lua/first.lua", "simple()"),
            ("lua/second.lua", "simple()"),
        ]);

        let first_id = file_id(&ws, "lua/first.lua");
        let second_id = file_id(&ws, "lua/second.lua");
        assert_eq!(
            (
                undefined_global_names(&mut ws, first_id),
                undefined_global_names(&mut ws, second_id),
            ),
            (
                BTreeSet::from(["simple".to_string()]),
                BTreeSet::from(["simple".to_string()]),
            ),
        );
    }

    #[test]
    fn compilefile_path_is_resolved_only_from_the_lua_root() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![
            (
                "lua/subdir/loader.lua",
                r#"
                local root_chunk = CompileFile("target.lua")
                setfenv(root_chunk, { root_field = true })

                local nested_chunk = CompileFile("subdir/target.lua")
                setfenv(nested_chunk, { nested_field = true })
                "#,
            ),
            ("lua/target.lua", "root_field()\nnested_field()"),
            ("lua/subdir/target.lua", "root_field()\nnested_field()"),
        ]);

        let root_target = file_id(&ws, "lua/target.lua");
        let nested_target = file_id(&ws, "lua/subdir/target.lua");
        assert_eq!(
            (
                undefined_global_names(&mut ws, root_target),
                undefined_global_names(&mut ws, nested_target),
            ),
            (
                BTreeSet::from(["nested_field".to_string()]),
                BTreeSet::from(["root_field".to_string()]),
            ),
        );
    }

    #[test]
    fn compilefile_path_does_not_fall_back_to_the_callers_directory() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![
            (
                "lua/subdir/loader.lua",
                r#"
                local chunk = CompileFile("target.lua")
                setfenv(chunk, { simple = true })
                "#,
            ),
            ("lua/subdir/target.lua", "simple()"),
        ]);

        let nested_target = file_id(&ws, "lua/subdir/target.lua");
        assert_eq!(
            undefined_global_names(&mut ws, nested_target),
            BTreeSet::from(["simple".to_string()]),
        );
    }

    #[test]
    fn reassigned_annotated_callees_and_wrappers_are_not_propagated() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![
            (
                "lua/loader.lua",
                r#"
                local CompileFile = CompileFile
                CompileFile = function(path) return function() end end
                local first = CompileFile("first.lua")
                setfenv(first, { simple = true })

                local second = _G.CompileFile("second.lua")
                local setfenv = setfenv
                setfenv = function(target, environment) end
                setfenv(second, { simple = true })

                ---@[call_arg("gmod.environment", "target")]
                ---@param target function
                ---@[call_arg("gmod.environment", "environment")]
                ---@param environment table
                local function applyEnvironment(target, environment) end
                applyEnvironment = function(target, environment) end
                applyEnvironment(_G.CompileFile("third.lua"), { simple = true })
                "#,
            ),
            ("lua/first.lua", "simple()"),
            ("lua/second.lua", "simple()"),
            ("lua/third.lua", "simple()"),
        ]);

        for path in ["lua/first.lua", "lua/second.lua", "lua/third.lua"] {
            let target = file_id(&ws, path);
            assert_eq!(
                undefined_global_names(&mut ws, target),
                BTreeSet::from(["simple".to_string()]),
                "reassigned call metadata leaked into {path}",
            );
        }
    }

    #[test]
    fn overload_only_call_roles_participate_in_candidate_prefiltering() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        ws.def_files(vec![
            (
                "lua/includes/overload_wrappers.lua",
                r#"
                ---@meta
                ---@[overload_call_arg(0, "gmod.load", "compilefile")]
                ---@overload fun(path: string): function
                function OverloadCompile(...) end

                ---@[overload_call_arg(0, "gmod.environment", "target")]
                ---@[overload_call_arg(1, "gmod.environment", "environment")]
                ---@overload fun(target: function, environment: table): function
                function OverloadSetEnvironment(...) end
                "#,
            ),
            (
                "lua/loader.lua",
                r#"
                local chunk = OverloadCompile("target.lua")
                OverloadSetEnvironment(chunk, { simple = true })
                "#,
            ),
            ("lua/target.lua", "simple()"),
        ]);

        let target = file_id(&ws, "lua/target.lua");
        assert!(undefined_global_names(&mut ws, target).is_empty());
    }

    #[test]
    fn source_edit_delete_and_reopen_rebuild_environment_mapping() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_call_arg_builtins();
        let loader_uri = ws.virtual_url_generator.new_uri("lua/loader.lua");
        ws.analysis.update_file_by_uri(
            &loader_uri,
            Some(
                r#"
                local chunk = CompileFile("target.lua")
                setfenv(chunk, { simple = true })
                "#
                .to_string(),
            ),
        );
        let target_uri = ws.virtual_url_generator.new_uri("lua/target.lua");
        let target_id = ws
            .analysis
            .update_file_by_uri(&target_uri, Some("simple()".to_string()))
            .expect("target file is created");
        assert!(undefined_global_names(&mut ws, target_id).is_empty());

        ws.analysis.update_file_by_uri(
            &loader_uri,
            Some(
                r#"
                local chunk = CompileFile("target.lua")
                setfenv(chunk, { other = true })
                "#
                .to_string(),
            ),
        );
        assert_eq!(
            undefined_global_names(&mut ws, target_id),
            BTreeSet::from(["simple".to_string()]),
        );

        ws.analysis.update_file_by_uri(&loader_uri, None);
        assert_eq!(
            undefined_global_names(&mut ws, target_id),
            BTreeSet::from(["simple".to_string()]),
        );

        ws.analysis.update_file_by_uri(
            &loader_uri,
            Some(
                r#"
                local chunk = CompileFile("target.lua")
                setfenv(chunk, { simple = true })
                "#
                .to_string(),
            ),
        );
        assert!(undefined_global_names(&mut ws, target_id).is_empty());

        ws.analysis.update_file_by_uri(&target_uri, None);
        let reopened_target_id = ws
            .analysis
            .update_file_by_uri(&target_uri, Some("simple()".to_string()))
            .expect("target file is reopened");
        assert!(undefined_global_names(&mut ws, reopened_target_id).is_empty());
    }
}
