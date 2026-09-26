#[cfg(test)]
mod test {
    use std::sync::mpsc;
    use std::time::Duration;

    use crate::VirtualWorkspace;

    /// The semantic digest of `source` analysed on its own, or `None` when the
    /// analysis does not settle within a generous bound.
    fn settled_digest(source: &str) -> Option<Vec<String>> {
        let source = source.to_string();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(move || {
                let mut ws = VirtualWorkspace::new();
                ws.def(&source);
                let _ = tx.send(ws.semantic_digest());
            })
            .expect("analysis thread");
        rx.recv_timeout(Duration::from_secs(30)).ok()
    }

    /// `argumentsTable` copies the parameter before the parameter is rewrapped
    /// in a table holding the copy. The rewrap is a later write: the copy reads
    /// what was passed, so neither statement's type feeds the other's.
    #[test]
    fn a_parameter_rewrapped_in_a_table_of_its_own_copy_settles() {
        let digest = settled_digest(
            r#"
            local function run(arguments)
                local argumentsTable = arguments
                arguments = { argumentsTable }
                return arguments
            end
            run({ 10 })
            "#,
        )
        .expect("the analysis settles");
        let copy = digest
            .iter()
            .find(|line| line.contains(": argumentsTable = "))
            .expect("the copy is typed");
        assert!(
            !copy.contains('('),
            "the copy holds what was passed: {copy}"
        );
    }

    /// `c` copies the global before the global is assigned a table holding
    /// the copy, so each statement's type feeds the other's.
    #[test]
    fn a_global_assigned_a_table_of_its_own_copy_settles() {
        let digest = settled_digest("local c = G\nG = { c }\n").expect("the analysis settles");
        let types = digest
            .iter()
            .filter_map(|line| line.split_once(": ").map(|(_, typed)| typed))
            .collect::<Vec<_>>();
        assert_eq!(types, ["G = (table)", "G = (table)", "c = (table)"]);
    }
}
