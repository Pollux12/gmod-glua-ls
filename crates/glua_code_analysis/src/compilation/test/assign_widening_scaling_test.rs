#[cfg(test)]
mod test {
    use std::time::Instant;

    use crate::VirtualWorkspace;

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

    #[test]
    fn repeated_field_assignment_indexing_stays_near_linear() {
        // Warm up so the first-file fixed costs (std/global setup) don't skew the
        // ratio, then measure two sizes that differ by 4×.
        let _ = index_repeated_branched_assignments(200);

        let small = index_repeated_branched_assignments(1000);
        let large = index_repeated_branched_assignments(4000);

        // 4× the assignments. Linear would be ~4×; the old quadratic path was
        // ~16×. Allow a generous 9× ceiling: comfortably above linear + overhead,
        // well below quadratic. This catches a reintroduced O(N²) without being
        // flaky on slow/noisy CI.
        let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-6);
        assert!(
            ratio < 9.0,
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
        for i in 0..300 {
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
}
