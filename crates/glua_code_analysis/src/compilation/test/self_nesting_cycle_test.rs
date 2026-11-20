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

    /// The digest of `files` analysed together, or `None` when the analysis
    /// does not settle within a generous bound.
    fn settled_files_digest(files: &[(&'static str, String)]) -> Option<Vec<String>> {
        let files = files.to_vec();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(move || {
                let mut ws = VirtualWorkspace::new();
                ws.def_files(
                    files
                        .iter()
                        .map(|(path, text)| (*path, text.as_str()))
                        .collect(),
                );
                let _ = tx.send(ws.semantic_digest());
            })
            .expect("analysis thread");
        rx.recv_timeout(Duration::from_secs(30)).ok()
    }

    const REGISTRY: &str = "lua/includes/modules/gamemode.lua";

    /// What generics in [`registry`] may return around their arguments: every
    /// kind of structure a template builds by value.
    const BUILD_RETURNS: [&str; 8] = [
        "T & B",
        "{ BaseClass: B }",
        "T & B & { BaseClass: B }",
        "(T | B)[]",
        "{ list: B[], self: T }",
        "{ get: fun(): B, me: fun(): T }",
        "[T, B]",
        "Wrapper<T | B>",
    ];

    /// A registry whose entries are built from one another by `build`, the
    /// shape of GMod's own `includes/modules/gamemode.lua`.
    fn registry(build_return: &str, build: &str) -> String {
        format!(
            r#"
            ---@class Wrapper<V>
            ---@field value V

            ---@generic T, B
            ---@param target T
            ---@param base B
            ---@return {build_return}
            local function Inherit(target, base) end

            local GameList = {{}}
            function Register(t, name, derived)
                local basetable = Get(derived)
                if basetable then
                    t = {build}
                end
                GameList[name] = t
            end
            function Get(name)
                return GameList[name]
            end
            "#
        )
    }

    /// An entry built from the entry it derives from settles whatever the
    /// generic returns around its base: the type does not nest a level deeper
    /// on every pass round the registry.
    #[test]
    fn a_registry_built_from_its_own_entries_settles() {
        for build_return in BUILD_RETURNS {
            let source = registry(build_return, "Inherit(t, basetable)");
            assert!(
                settled_files_digest(&[(REGISTRY, source)]).is_some(),
                "{build_return} settles"
            );
        }
    }

    /// The same registry split over two files, the caller registering into it
    /// from another file.
    #[test]
    fn a_registry_filled_from_another_file_settles() {
        let source = registry("T & B & { BaseClass: B }", "Inherit(t, basetable)");
        let caller =
            "Register({}, \"sandbox\", \"base\")\nlocal sandbox = Get(\"sandbox\")\n".to_string();
        assert!(
            settled_files_digest(&[(REGISTRY, source), ("lua/autorun/sh_caller.lua", caller)])
                .is_some()
        );
    }

    /// Editing the registry, deleting it and opening it again leaves what a
    /// cold build of the same files has.
    #[test]
    fn a_registry_edited_deleted_and_reopened_settles_as_a_cold_build() {
        for build_return in ["T & B & { BaseClass: B }", "(T | B)[]"] {
            registry_edited_deleted_and_reopened_settles_as_a_cold_build(build_return);
        }
    }

    fn registry_edited_deleted_and_reopened_settles_as_a_cold_build(build_return: &str) {
        let source = registry(build_return, "Inherit(t, basetable)");
        let edited = registry(build_return, "Inherit(basetable, t)");
        let caller = "local sandbox = Get(\"sandbox\")\n";
        let cold = |registry: Option<&str>| {
            let mut files = vec![("lua/autorun/sh_caller.lua", caller.to_string())];
            files.extend(registry.map(|text| (REGISTRY, text.to_string())));
            settled_files_digest(&files).expect("the cold build settles")
        };
        let (tx, rx) = mpsc::channel();
        let (source_c, edited_c) = (source.clone(), edited.clone());
        std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(move || {
                let mut ws = VirtualWorkspace::new();
                ws.def_file("lua/autorun/sh_caller.lua", caller);
                ws.def_file(REGISTRY, &source_c);
                ws.def_file(REGISTRY, &edited_c);
                let edited = ws.semantic_digest();
                let uri = ws.virtual_url_generator.new_uri(REGISTRY);
                ws.analysis.remove_file_by_uri(&uri);
                let deleted = ws.semantic_digest();
                ws.def_file(REGISTRY, &source_c);
                let _ = tx.send((edited, deleted, ws.semantic_digest()));
            })
            .expect("analysis thread");
        let warm = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the warm edits settle");
        assert_eq!(
            warm,
            (cold(Some(&edited)), cold(None), cold(Some(&source))),
            "{build_return}"
        );
    }

    /// A value that is not built from itself keeps the structure it nests
    /// through a generic's array and callback.
    #[test]
    fn a_nested_value_through_a_generic_keeps_its_fields() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@generic T
            ---@param value T
            ---@return T[], fun(): T
            local function wrap(value) end

            ---@type { a: { b: { c: { d: integer } } } }
            local deep
            local list, get = wrap(deep)
            fromList = list[1].a.b.c.d
            fromGet = get().a.b.c.d
            "#,
        );

        let types = (ws.expr_ty("fromList"), ws.expr_ty("fromGet"));

        assert_eq!(
            (ws.humanize_type(types.0), ws.humanize_type(types.1)),
            ("integer".to_string(), "integer".to_string())
        );
    }
}
