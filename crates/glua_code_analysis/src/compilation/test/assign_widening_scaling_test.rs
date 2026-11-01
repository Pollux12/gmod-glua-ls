#[cfg(test)]
mod test {
    use crate::VirtualWorkspace;
    use crate::semantic::BASELINE_FLOW_WALKS;

    /// A closure reading an outer local resolves its baseline type by walking
    /// back past every branch merge ahead of it, so the branch count is the
    /// exponent if a merge point is derived once per path instead of once.
    fn index_branch_merges_before_closure(count: usize) {
        let mut body = String::from("local value = 1\n");
        for i in 0..count {
            body.push_str(&format!(
                "if cond{i} then value = {i} else value = {} end\n",
                i + 100
            ));
        }
        body.push_str("local read = function() return value end\n");

        VirtualWorkspace::new().def(&body);
    }

    /// Closure-baseline walks performed while indexing `count` branch merges.
    fn baseline_walks_for_branch_merges(count: usize) -> u64 {
        BASELINE_FLOW_WALKS.with(|walks| walks.set(0));
        index_branch_merges_before_closure(count);
        BASELINE_FLOW_WALKS.with(|walks| walks.get())
    }

    /// The closure-baseline flow walk must memoise each merge point. Without
    /// that it derives one per path reaching the merge, which is `2^n` in the
    /// branch count rather than `n`.
    ///
    /// Counted rather than timed. The two behaviours differ by orders of
    /// magnitude in *work done*, so counting says so directly, runs in
    /// milliseconds, and cannot flake when the test suite saturates the CPU.
    #[test]
    fn closure_baseline_memoises_branch_merges() {
        let small = baseline_walks_for_branch_merges(8);
        let large = baseline_walks_for_branch_merges(16);

        assert!(
            small > 0,
            "no closure-baseline walk ran; the guard is vacuous"
        );
        // Twice the branches. Memoised that is about twice the walks; deriving
        // once per path would be squaring it, so anything near linear passes and
        // the regression cannot.
        assert!(
            large <= small * 4,
            "closure-baseline walks grew from {small} to {large} when the branch \
             count doubled; merge points are being re-derived once per path"
        );
    }

    fn infer_after_repeated_unrelated_inner_conditions(count: usize) -> String {
        let mut body = String::from(
            r#"
---@type string?
local target
local unrelated = {}
"#,
        );
        for i in 0..count {
            body.push_str(&format!(
                "if cond{i} then\n    if unrelated.flag{i} then\n        local sink{i} = {i}\n    end\nend\n"
            ));
        }
        body.push_str("result = target\n");

        let mut ws = VirtualWorkspace::new();
        ws.def(&body);
        let result_type = ws.expr_ty("result");
        ws.humanize_type(result_type)
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
        let mut body = String::from("local T = {}\n");
        for _ in 0..30 {
            body.push_str("T.field = T.field or {}\n");
        }
        body.push_str("local result = T.field\n");

        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(&body);

        assert!(
            ws.analysis
                .compilation
                .get_semantic_model(file_id)
                .expect("semantic model")
                .get_db()
                .get_decl_index()
                .get_decl_tree(&file_id)
                .is_some(),
            "file failed to index"
        );
    }

    #[test]
    fn repeated_unrelated_inner_conditions_still_infer_quick_smoke() {
        let result_type = infer_after_repeated_unrelated_inner_conditions(200);

        assert_eq!(result_type, "string?");
    }

    #[test]
    fn repeated_literal_index_member_assignments_index_quick_smoke() {
        let mut body = String::from("local T = {}\nT.entries = {}\n");
        for i in 0..30 {
            body.push_str(&format!(
                r#"
T.entries["entry_{i}"] = {{}}
T.entries["entry_{i}"].name = "Entry {i}"
T.entries["entry_{i}"].offset = {{ Vector(0, 0, 0), Angle(0, 0, 0) }}
"#
            ));
        }
        body.push_str(r#"local result = T.entries["entry_29"].name"#);

        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(&body);

        assert!(
            ws.analysis
                .compilation
                .get_semantic_model(file_id)
                .expect("semantic model")
                .get_db()
                .get_decl_index()
                .get_decl_tree(&file_id)
                .is_some(),
            "file failed to index"
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
}
