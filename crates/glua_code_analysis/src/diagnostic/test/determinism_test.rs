#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use crate::test_lib::DiagnosticSnapshot;
    use crate::{
        DiagnosticCode, Emmyrc, FileId, RenderLevel, VirtualWorkspace, WorkspaceId, humanize_type,
    };
    use glua_parser::{LuaAstNode, LuaExpr, LuaIndexExpr};
    use googletest::prelude::*;
    use tokio_util::sync::CancellationToken;

    fn synthetic_files() -> BTreeMap<&'static str, &'static str> {
        BTreeMap::from([
            (
                "lua/autorun/shared/assign_case.lua",
                r#"
                ---@type integer
                local value = 1
                value = "oops"
                "#,
            ),
            (
                "lua/autorun/shared/param_case.lua",
                r#"
                ---@param x integer
                local function takes_int(x) end
                takes_int("oops")
                "#,
            ),
            (
                "lua/autorun/shared/nil_case.lua",
                r#"
                ---@type { value: integer }?
                local maybe_tbl
                local _ = maybe_tbl.value
                "#,
            ),
            (
                "lua/autorun/shared/undefined_case.lua",
                r#"
                ---@class DeterminismUndefinedCase
                ---@field ok integer
                ---@type DeterminismUndefinedCase
                local value = {}
                local _ = value.missing
                "#,
            ),
        ])
    }

    fn file_order(files: &BTreeMap<&'static str, &'static str>) -> Vec<&'static str> {
        files.keys().copied().collect()
    }

    fn register_files_in_order(
        ws: &mut VirtualWorkspace,
        files: &BTreeMap<&'static str, &'static str>,
        order: &[&'static str],
    ) -> BTreeMap<&'static str, FileId> {
        let mut file_ids = BTreeMap::new();
        for file_path in order {
            let file_content = files
                .get(file_path)
                .expect("synthetic determinism file content should exist");
            let file_id = ws.def_file(file_path, file_content);
            file_ids.insert(*file_path, file_id);
        }
        file_ids
    }

    fn collect_code_snapshots(
        diagnostic_code: DiagnosticCode,
        registration_order: &[&'static str],
    ) -> BTreeSet<DiagnosticSnapshot> {
        let mut ws = VirtualWorkspace::new();
        ws.analysis.diagnostic.enable_only(diagnostic_code);

        let files = synthetic_files();
        let file_ids = register_files_in_order(&mut ws, &files, registration_order);
        let selected_ids: Vec<FileId> = file_ids.values().copied().collect();
        let mut reversed_selected_ids = selected_ids.clone();
        reversed_selected_ids.reverse();

        let forward_snapshots = ws.run_diagnostics_with_shared_snapshots(&selected_ids);
        let reverse_snapshots = ws.run_diagnostics_with_shared_snapshots(&reversed_selected_ids);
        assert_eq!(
            reverse_snapshots, forward_snapshots,
            "selected file iteration order should not change shared-snapshot diagnostic sets"
        );

        let target_code = Some(diagnostic_code.get_name().to_string());
        forward_snapshots
            .into_iter()
            .filter(|snapshot| snapshot.code == target_code)
            .collect()
    }

    fn assert_deterministic_for_code(diagnostic_code: DiagnosticCode) {
        let files = synthetic_files();
        let baseline_order = file_order(&files);
        let mut shuffled_order = baseline_order.clone();
        shuffled_order.reverse();

        let baseline = collect_code_snapshots(diagnostic_code, &baseline_order);
        let shuffled = collect_code_snapshots(diagnostic_code, &shuffled_order);

        assert_that!(baseline.is_empty(), eq(false));
        assert_eq!(shuffled, baseline);
    }

    fn collect_dual_unresolve_snapshots(
        registration_order: &[&'static str],
    ) -> BTreeSet<DiagnosticSnapshot> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.diagnostics.enables =
            vec![DiagnosticCode::NeedCheckNil, DiagnosticCode::UndefinedField];
        ws.update_emmyrc(emmyrc);

        let files = synthetic_files();
        let file_ids = register_files_in_order(&mut ws, &files, registration_order);
        let selected_ids: Vec<FileId> = file_ids.values().copied().collect();
        let mut reversed_selected_ids = selected_ids.clone();
        reversed_selected_ids.reverse();

        let forward_snapshots = ws.run_diagnostics_with_shared_snapshots(&selected_ids);
        let reverse_snapshots = ws.run_diagnostics_with_shared_snapshots(&reversed_selected_ids);
        assert_eq!(reverse_snapshots, forward_snapshots);

        forward_snapshots
            .into_iter()
            .filter(|snapshot| {
                snapshot.code == Some(DiagnosticCode::NeedCheckNil.get_name().to_string())
                    || snapshot.code == Some(DiagnosticCode::UndefinedField.get_name().to_string())
            })
            .collect()
    }

    fn collect_gmod_realm_mismatch_snapshots(
        registration_order: &[&'static str],
    ) -> BTreeSet<DiagnosticSnapshot> {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatchHeuristic);

        let files = BTreeMap::from([
            (
                "lua/autorun/server/sv_api.lua",
                r#"
                function ServerOnlyApi()
                    return true
                end
                "#,
            ),
            (
                "lua/autorun/client/cl_user.lua",
                r#"
                ServerOnlyApi()
                "#,
            ),
            (
                "lua/autorun/client/cl_user_2.lua",
                r#"
                ServerOnlyApi()
                "#,
            ),
        ]);

        let file_ids = register_files_in_order(&mut ws, &files, registration_order);
        let selected_ids: Vec<FileId> = file_ids.values().copied().collect();
        let mut reversed_selected_ids = selected_ids.clone();
        reversed_selected_ids.reverse();

        let forward_snapshots = ws.run_diagnostics_with_shared_snapshots(&selected_ids);
        let reverse_snapshots = ws.run_diagnostics_with_shared_snapshots(&reversed_selected_ids);
        assert_eq!(
            reverse_snapshots, forward_snapshots,
            "selected file iteration order should not change gmod shared-snapshot diagnostic sets"
        );

        let target_code = Some(
            DiagnosticCode::GmodRealmMismatchHeuristic
                .get_name()
                .to_string(),
        );
        forward_snapshots
            .into_iter()
            .filter(|snapshot| snapshot.code == target_code)
            .collect()
    }

    fn inferred_field_type(ws: &VirtualWorkspace, file_id: FileId, field_name: &str) -> String {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let root = semantic_model.get_root();
        let target = root
            .descendants::<LuaIndexExpr>()
            .find(|expr| {
                expr.get_index_key()
                    .is_some_and(|key| key.get_path_part() == field_name)
            })
            .expect("field access should exist");
        let ty = semantic_model
            .infer_expr(LuaExpr::IndexExpr(target))
            .unwrap_or(crate::LuaType::Unknown);
        humanize_type(ws.analysis.compilation.get_db(), &ty, RenderLevel::Detailed)
    }

    #[gtest]
    fn determinism_assign_type_mismatch_across_registration_and_parallel_diagnostics() {
        assert_deterministic_for_code(DiagnosticCode::AssignTypeMismatch);
    }

    #[gtest]
    fn determinism_param_type_mismatch_across_registration_and_parallel_diagnostics() {
        assert_deterministic_for_code(DiagnosticCode::ParamTypeMismatch);
    }

    #[gtest]
    fn determinism_need_check_nil_across_registration_and_parallel_diagnostics() {
        assert_deterministic_for_code(DiagnosticCode::NeedCheckNil);
    }

    #[gtest]
    fn determinism_undefined_field_across_registration_and_parallel_diagnostics() {
        assert_deterministic_for_code(DiagnosticCode::UndefinedField);
    }

    #[gtest]
    fn determinism_gmod_realm_mismatch_across_registration_and_parallel_diagnostics() {
        let baseline_order = [
            "lua/autorun/server/sv_api.lua",
            "lua/autorun/client/cl_user.lua",
            "lua/autorun/client/cl_user_2.lua",
        ];
        let mut reversed_order = baseline_order.to_vec();
        reversed_order.reverse();

        let baseline = collect_gmod_realm_mismatch_snapshots(&baseline_order);
        let shuffled = collect_gmod_realm_mismatch_snapshots(&reversed_order);

        assert_that!(baseline.is_empty(), eq(false));
        assert_eq!(shuffled, baseline);
    }

    #[gtest]
    fn determinism_unresolve_related_diagnostics_not_dropped_by_registration_order() {
        let files = synthetic_files();
        let baseline_order = file_order(&files);
        let mut shuffled_order = baseline_order.clone();
        shuffled_order.reverse();

        let baseline = collect_dual_unresolve_snapshots(&baseline_order);
        let shuffled = collect_dual_unresolve_snapshots(&shuffled_order);
        assert_eq!(shuffled, baseline);

        let emitted_codes: BTreeSet<String> = baseline
            .iter()
            .filter_map(|snapshot| snapshot.code.clone())
            .collect();
        assert_that!(
            emitted_codes.contains(DiagnosticCode::NeedCheckNil.get_name()),
            eq(true)
        );
        assert_that!(
            emitted_codes.contains(DiagnosticCode::UndefinedField.get_name()),
            eq(true)
        );
    }

    #[gtest]
    fn shared_data_precompute_contains_sorted_workspace_files_and_callee_realms() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodRealmMismatchHeuristic);

        let server_id = ws.def_file(
            "lua/autorun/server/sv_api.lua",
            r#"
            function ServerOnlyApi()
                return true
            end
            "#,
        );
        let client_id = ws.def_file(
            "lua/autorun/client/cl_user.lua",
            r#"
            ServerOnlyApi()
            "#,
        );

        let mut expected_file_ids = vec![server_id, client_id];
        expected_file_ids.sort_unstable();

        let shared_data = ws.analysis.precompute_diagnostic_shared_data();
        assert_eq!(shared_data.workspace_file_ids.as_ref(), &expected_file_ids);

        let workspace_id = ws
            .analysis
            .compilation
            .get_db()
            .get_module_index()
            .get_workspace_id(client_id)
            .unwrap_or(WorkspaceId::MAIN);
        let precomputed = shared_data
            .callee_realms_by_workspace
            .get(&workspace_id)
            .cloned()
            .unwrap_or_default();
        assert_that!(precomputed.is_empty(), eq(false));
    }

    #[gtest]
    fn gmod_dynamic_table_named_field_fallback_does_not_leak_expr_key_values_after_noop_edit() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        emmyrc.diagnostics.enables = vec![DiagnosticCode::ParamTypeMismatch];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/glide/client/network.lua",
            r#"
            Glide = Glide or {}
            Glide.DebugSnapshots = Glide.DebugSnapshots or {}

            ---@return string
            local function readKey()
            end

            local function readValue()
                if unknown then
                    return "text"
                end
                return 1
            end

            local rec = { data = {}, t = 0 }
            local data = rec.data
            local key = readKey()
            data[key] = readValue()
            Glide.DebugSnapshots[1] = rec
            "#,
        );

        let debug_uri = ws
            .virtual_url_generator
            .new_uri("lua/glide/client/debugging.lua");
        let original = r#"
            ---@param value number
            local function takesNumber(value) end
            takesNumber("still a stable mismatch")

            local snaps = Glide.DebugSnapshots or {}
            for entId, rec in pairs(snaps) do
                local d = rec.data
                local engineTypeId = d.engineTypeId or 1
                local engineTypeNames = {}
                local _ = engineTypeNames[engineTypeId]
                Glide.DebugSnapshots[entId] = nil
            end
            "#;
        let debug_id = ws
            .analysis
            .update_file_by_uri(&debug_uri, Some(original.to_string()))
            .expect("debug file id");

        fn param_snapshots(ws: &VirtualWorkspace, file_id: FileId) -> BTreeSet<DiagnosticSnapshot> {
            let diagnostics = ws
                .analysis
                .diagnose_file(file_id, CancellationToken::new())
                .unwrap_or_default();
            ws.diagnostic_snapshots_for_file(file_id, diagnostics)
                .into_iter()
                .filter(|snapshot| {
                    snapshot.code == Some(DiagnosticCode::ParamTypeMismatch.get_name().to_string())
                })
                .collect()
        }

        fn line_independent_snapshots(
            snapshots: &BTreeSet<DiagnosticSnapshot>,
        ) -> BTreeSet<(u32, u32, Option<i32>, Option<String>, String)> {
            snapshots
                .iter()
                .map(|snapshot| {
                    (
                        snapshot.range_start_character,
                        snapshot.range_end_character,
                        snapshot.severity,
                        snapshot.code.clone(),
                        snapshot.message.clone(),
                    )
                })
                .collect()
        }

        let baseline = param_snapshots(&ws, debug_id);
        let baseline_engine_type = inferred_field_type(&ws, debug_id, "engineTypeId");

        ws.analysis
            .update_file_by_uri(&debug_uri, Some(format!("\n{original}")));
        let after_add = param_snapshots(&ws, debug_id);
        let after_add_engine_type = inferred_field_type(&ws, debug_id, "engineTypeId");

        ws.analysis
            .update_file_by_uri(&debug_uri, Some(original.to_string()));
        let after_remove = param_snapshots(&ws, debug_id);
        let after_remove_engine_type = inferred_field_type(&ws, debug_id, "engineTypeId");

        assert_that!(baseline.is_empty(), eq(false));
        assert_eq!(baseline, after_remove);
        assert_eq!(
            line_independent_snapshots(&baseline),
            line_independent_snapshots(&after_add)
        );
        assert_that!(
            after_add
                .iter()
                .all(|snapshot| !snapshot.message.contains("engineTypeId")),
            eq(true)
        );
        assert_that!(
            after_remove
                .iter()
                .all(|snapshot| !snapshot.message.contains("engineTypeId")),
            eq(true)
        );
        for engine_type in [
            &baseline_engine_type,
            &after_add_engine_type,
            &after_remove_engine_type,
        ] {
            assert_that!(engine_type.contains("text"), eq(false));
            assert_that!(engine_type.contains('1'), eq(false));
        }
    }

    #[gtest]
    fn gmod_dynamic_table_named_field_fallback_respects_non_global_dynamic_fields() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        emmyrc.gmod.dynamic_fields_global = false;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/glide/client/network.lua",
            r#"
            Glide = Glide or {}
            Glide.DebugSnapshots = Glide.DebugSnapshots or {}

            ---@return string
            local function readKey()
            end

            local rec = { data = {} }
            local data = rec.data
            local key = readKey()
            data[key] = 1
            Glide.DebugSnapshots[1] = rec
            "#,
        );

        let debug_file = ws.def_file(
            "lua/glide/client/debugging.lua",
            r#"
            local snaps = Glide.DebugSnapshots or {}
            for _, rec in pairs(snaps) do
                local d = rec.data
                local engineTypeId = d.engineTypeId
            end
            "#,
        );

        let engine_type = inferred_field_type(&ws, debug_file, "engineTypeId");
        assert_that!(engine_type.contains("any"), eq(false));
    }

    #[gtest]
    fn gmod_dynamic_table_named_field_fallback_respects_realm_visibility() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        emmyrc.gmod.dynamic_fields_global = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/glide/server/network.lua",
            r#"
            Glide = Glide or {}
            Glide.DebugSnapshots = Glide.DebugSnapshots or {}

            ---@return string
            local function readKey()
            end

            local rec = { data = {} }
            local data = rec.data
            local key = readKey()
            data[key] = 1
            Glide.DebugSnapshots[1] = rec
            "#,
        );

        let debug_file = ws.def_file(
            "lua/glide/client/debugging.lua",
            r#"
            local snaps = Glide.DebugSnapshots or {}
            for _, rec in pairs(snaps) do
                local d = rec.data
                local engineTypeId = d.engineTypeId
            end
            "#,
        );

        let engine_type = inferred_field_type(&ws, debug_file, "engineTypeId");
        assert_that!(engine_type.contains("any"), eq(false));
    }
}
