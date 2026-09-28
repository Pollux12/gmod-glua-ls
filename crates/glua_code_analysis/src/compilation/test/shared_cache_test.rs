#[cfg(test)]
mod test {
    use crate::VirtualWorkspace;
    use crate::db_index::derivation::DerivationKind;
    use crate::profile::work::{Work, counted};

    /// How many flow walks the file's local statements computed, and how many
    /// distinct walks those were.
    fn local_statement_walks(ws: &mut VirtualWorkspace, path: &str, text: &str) -> (u64, usize) {
        let (_, counts) = counted(|| ws.def_file(path, text));
        counts
            .get(&(Work::Flow, Some(DerivationKind::LocalStat)))
            .copied()
            .unwrap_or_default()
    }

    /// A field's walk starts from its member type, and is kept for that origin:
    /// every statement reading the field after a branch reuses the walks of the
    /// branch instead of walking them again.
    #[test]
    fn a_field_walk_from_its_member_type_is_shared_across_statements() {
        let mut ws = VirtualWorkspace::new();
        let reads = (0..8)
            .map(|read| format!("local read{read} = cfg.value\n"))
            .collect::<String>();
        let (computed, distinct) = local_statement_walks(
            &mut ws,
            "lua/autorun/field_walks.lua",
            &format!(
                "local cfg = {{}}\ncfg.value = 1\nif math.random() > 0.5 then\n    cfg.value = 2\nend\n{reads}"
            ),
        );
        assert!(distinct > 0, "the reads walked no flow");
        assert_eq!(
            computed, distinct as u64,
            "a walk from the field's member type was computed again by another statement"
        );
    }
}
