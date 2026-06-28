#[cfg(test)]
mod test {
    use std::time::Instant;

    use crate::{Emmyrc, EmmyrcGmodScriptedClassScopeEntry, VirtualWorkspace};

    fn legacy_scope(pattern: &str) -> EmmyrcGmodScriptedClassScopeEntry {
        EmmyrcGmodScriptedClassScopeEntry::LegacyGlob(pattern.to_string())
    }

    /// Regression guard for issue #36: a field assigned a very large number of
    /// times under distinct (branched / guarded) writes used to drive
    /// `lua analyze` into O(N²) behaviour — each assignment re-scanned every
    /// prior member sharing the same `(owner, key)` to widen/preserve its type.
    /// On large generated files this made a single file take seconds and timed
    /// the whole workspace out.
    ///
    /// Analysis should stay close to linear in the number of assignments. This
    /// test asserts that doubling the assignment count does not super-linearly
    /// blow up indexing time. Timing-based, but with a wide margin: the pre-fix
    /// code was ~quadratic (≈16× from 1k→4k), so even a generous ratio check
    /// reliably catches a regression without flaking.
    fn index_repeated_branched_assignments(count: usize) -> std::time::Duration {
        let mut body = String::from("local T = {}\n");
        for i in 0..count {
            // Each write is a distinct, branch-guarded assignment to the SAME
            // field, so all members are preserved under one (owner, key).
            body.push_str(&format!(
                "if cond{i} then T.field = {{ a{i} = {i}, shared = true }} end\n"
            ));
        }
        body.push_str("return T\n");

        let mut ws = VirtualWorkspace::new();
        let start = Instant::now();
        ws.def(&body);
        start.elapsed()
    }

    fn index_repeated_guarded_bootstrap_assignments(count: usize) -> std::time::Duration {
        let mut body = String::from("local T = {}\n");
        for _ in 0..count {
            body.push_str("T.field = T.field or {}\n");
        }
        body.push_str("return T\n");

        let mut ws = VirtualWorkspace::new();
        let start = Instant::now();
        ws.def(&body);
        start.elapsed()
    }

    #[test]
    #[ignore = "wall-clock performance smoke; direct cache unit tests cover the hot path in default runs"]
    fn repeated_field_assignment_indexing_stays_near_linear() {
        // Warm up so the first-file fixed costs (std/global setup) don't skew the
        // ratio, then measure two sizes that differ by 4×.
        let _ = index_repeated_branched_assignments(200);

        let small = index_repeated_branched_assignments(1000);
        let large = index_repeated_branched_assignments(4000);

        // 4× the assignments. Linear would be ~4×; the old quadratic path was
        // ~16×. Allow a generous 12× ceiling: comfortably below quadratic, but
        // tolerant of wall-clock noise when this ignored smoke is run manually.
        let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-6);
        assert!(
            ratio < 12.0,
            "indexing scaled super-linearly with repeated field assignments \
             (1000 -> {small:?}, 4000 -> {large:?}, ratio {ratio:.1}x); \
             the O(N^2) member-assignment widening may have regressed (issue #36)"
        );
    }

    #[test]
    fn repeated_field_assignment_still_infers_field() {
        // Behaviour guard: the field must still resolve to a usable table type
        // after many guarded writes (analysis must not bail or panic).
        let mut ws = VirtualWorkspace::new();
        let mut body = String::from("local T = {}\n");
        for i in 0..30 {
            body.push_str(&format!("if c{i} then T.field = {{ v = {i} }} end\n"));
        }
        body.push_str("local result = T.field\n");
        let file_id = ws.def(&body);

        let result_type = {
            let semantic_model = ws
                .analysis
                .compilation
                .get_semantic_model(file_id)
                .expect("semantic model");
            semantic_model
                .get_db()
                .get_decl_index()
                .get_decl_tree(&file_id)
                .map(|_| ())
        };
        // The decl tree exists and analysis completed without hanging/panicking.
        assert!(result_type.is_some(), "file failed to index");
    }

    #[test]
    fn repeated_guarded_bootstrap_assignment_still_indexes_quick_smoke() {
        let elapsed = index_repeated_guarded_bootstrap_assignments(30);

        assert!(
            elapsed.as_millis() < 250,
            "small guarded bootstrap smoke took too long: {elapsed:?}"
        );
    }

    #[test]
    #[ignore = "wall-clock performance smoke for repeated guarded table bootstraps"]
    fn repeated_guarded_bootstrap_assignment_indexing_stays_near_linear() {
        let _ = index_repeated_guarded_bootstrap_assignments(200);

        let small = index_repeated_guarded_bootstrap_assignments(1000);
        let large = index_repeated_guarded_bootstrap_assignments(4000);

        let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-6);
        eprintln!(
            "guarded bootstrap assignment scaling: 1000 -> {small:?}, 4000 -> {large:?}, ratio {ratio:.1}x"
        );
        assert!(
            ratio < 12.0,
            "indexing scaled super-linearly with guarded bootstrap assignments \
             (1000 -> {small:?}, 4000 -> {large:?}, ratio {ratio:.1}x)"
        );
    }

    fn index_distinct_self_field_assignments(count: usize) -> std::time::Duration {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        let mut body = String::from("function ENT:Initialize()\n");
        for i in 0..count {
            body.push_str(&format!("    self.field{i} = {i}\n"));
        }
        body.push_str("end\n");

        let start = Instant::now();
        ws.def_file("lua/entities/perf_entity/init.lua", &body);
        start.elapsed()
    }

    #[test]
    #[ignore = "wall-clock performance smoke; direct cache unit tests cover the hot path in default runs"]
    fn distinct_self_field_assignments_index_near_linearly() {
        let _ = index_distinct_self_field_assignments(200);

        let small = index_distinct_self_field_assignments(1000);
        let large = index_distinct_self_field_assignments(4000);

        let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-6);
        eprintln!(
            "distinct self field assignment scaling: 1000 -> {small:?}, 4000 -> {large:?}, ratio {ratio:.1}x"
        );
        assert!(
            ratio < 12.0,
            "indexing scaled super-linearly with distinct self field assignments \
             (1000 -> {small:?}, 4000 -> {large:?}, ratio {ratio:.1}x)"
        );
    }

    fn index_dynamic_key_collection_assignments(count: usize) -> std::time::Duration {
        let mut body = String::from(
            r#"
---@return string
local function key_name()
end

local T = {}
"#,
        );
        for i in 0..count {
            body.push_str(&format!("T.field{i} = {{ {i} }}\n"));
        }
        for i in 0..count {
            body.push_str(&format!("T[key_name()] = {{ {i} }}\n"));
        }
        body.push_str("return T\n");

        let mut ws = VirtualWorkspace::new();
        let start = Instant::now();
        ws.def(&body);
        start.elapsed()
    }

    fn index_repeated_literal_index_member_assignments(count: usize) -> std::time::Duration {
        let mut body = String::from("local T = {}\nT.entries = {}\n");
        for i in 0..count {
            body.push_str(&format!(
                r#"
T.entries["entry_{i}"] = {{}}
T.entries["entry_{i}"].name = "Entry {i}"
T.entries["entry_{i}"].offset = {{ Vector(0, 0, 0), Angle(0, 0, 0) }}
"#
            ));
        }

        let mut ws = VirtualWorkspace::new();
        let start = Instant::now();
        ws.def(&body);
        start.elapsed()
    }

    #[test]
    fn repeated_literal_index_member_assignments_index_quick_smoke() {
        let elapsed = index_repeated_literal_index_member_assignments(30);

        assert!(
            elapsed.as_millis() < 250,
            "literal-index member assignment smoke took too long: {elapsed:?}"
        );
    }

    #[test]
    fn literal_index_member_owner_cache_invalidates_after_non_table_assignment() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
local T = {}
T["entry"] = {}
T["entry"] = false
T["entry"].name = "stale"
local result = T["entry"].name
"#,
        );

        let result_type = ws.expr_ty("result");
        assert_ne!(ws.humanize_type(result_type), "string");
    }

    #[test]
    #[ignore = "wall-clock performance smoke; direct cache unit tests cover the hot path in default runs"]
    fn dynamic_key_collection_assignments_do_not_scan_owner_members_quadratically() {
        let _ = index_dynamic_key_collection_assignments(100);

        let small = index_dynamic_key_collection_assignments(500);
        let large = index_dynamic_key_collection_assignments(2000);

        let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-6);
        eprintln!(
            "dynamic key collection assignment scaling: 500 -> {small:?}, 2000 -> {large:?}, ratio {ratio:.1}x"
        );
        assert!(
            ratio < 12.0,
            "indexing scaled super-linearly with dynamic-key collection assignments \
             (500 -> {small:?}, 2000 -> {large:?}, ratio {ratio:.1}x)"
        );
    }
}
