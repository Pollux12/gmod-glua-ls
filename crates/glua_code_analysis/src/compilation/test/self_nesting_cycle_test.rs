#[cfg(test)]
mod test {
    use std::sync::mpsc;
    use std::time::Duration;

    use crate::{DiagnosticCode, VirtualWorkspace};

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
        assert_eq!(types, ["G = ((table))", "G = ((table))", "c = ((table))"]);
    }

    /// A row that holds no summary of the literal keeps its positions, so an
    /// index through it reads the exact value.
    #[test]
    fn a_row_nested_without_a_cycle_keeps_its_positions() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "lua/autorun/sh_rows.lua",
            r#"
            local row = { 1, "x" }
            local rows = { row }
            local value = rows[1][2]
            "#,
        );

        let value = ws.local_ty(file_id, "value");
        assert_eq!(ws.humanize_type(value), "\"x\"");
    }

    #[test]
    fn a_value_read_through_a_nested_row_is_checked_against_its_annotation() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local row = { 1, "x" }
            local rows = { row }
            ---@type number
            local value = rows[1][2]
            "#,
        ));
    }

    /// A local that caches the entity the function creates is assigned from a
    /// local that reads the cache first.
    #[test]
    fn a_cached_local_read_before_its_assignment_settles() {
        let digest = settled_digest(
            r#"
            ---@meta
            ---@class Entity
            local Entity = {}
            ---@return boolean
            ---@return_cast self Entity
            ---@[self_guard("gmod.entity")]
            function Entity:IsValid() end

            ---@class NULL : Entity
            ---@type NULL
            NULL = nil

            ents = {}
            ---@generic T : Entity
            ---@param class `T`
            ---@return (instance) T|NULL
            function ents.Create(class) end

            local cached = NULL
            local function make(typ)
                local ent = cached:IsValid() and cached or ents.Create("env_entity_dissolver")
                ent:Spawn()
                cached = ent
            end
            "#,
        );
        assert!(digest.is_some(), "the analysis settles");
    }

    #[test]
    fn a_local_written_after_a_guarded_write_of_its_own_comparison_settles() {
        let digest = settled_digest(
            r#"
            ---@class filelib
            file = {}
            ---@param name string
            ---@param content string
            function file.Write(name, content) end

            local last_backup
            function f(data)
                local str = pace.luadata.Encode(data)
                if str ~= last_backup then
                    file.Write("x", str)
                    last_backup = str
                end
            end
            "#,
        );
        assert!(digest.is_some(), "the analysis settles");
    }
}
