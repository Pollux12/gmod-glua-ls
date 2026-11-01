#[cfg(test)]
mod test {
    use crate::db_index::derivation::FactKey;
    use crate::{FileId, LuaTypeDeclId, VirtualWorkspace};

    const BASE_FILE: &str = "lua/autorun/sh_base.lua";
    const CLASS_FILE: &str = "lua/autorun/sh_child.lua";
    const READER_FILE: &str = "lua/autorun/sh_reader.lua";
    const MORE_CLASS_FILE: &str = "lua/autorun/sh_z_more_child.lua";

    const BASE: &str = "---@class Base\n---@field speed number\n";
    const CLASS_BEFORE: &str = "print(1)\nprint(2)\nprint(3)\n\n---@class Child\nprint(4)\n";
    const CLASS_AFTER: &str = "---@class Child\nprint(4)\n\nprint(1)\nprint(2)\nprint(3)\n";
    const DERIVED_BEFORE: &str =
        "print(1)\nprint(2)\nprint(3)\n\n---@class Child : Base\nprint(4)\n";
    const DERIVED_AFTER: &str =
        "---@class Child : Base\nprint(4)\n\nprint(1)\nprint(2)\nprint(3)\n";
    const DERIVED_IN_BRANCH: &str =
        "if SERVER then\n    ---@class Child : Base\n    local child = {}\nend\n";
    const READER: &str = "---@param child Child
local function read(child)
    local value = child.speed
    return value
end
";

    fn evaluated_files(ws: &VirtualWorkspace) -> Vec<FileId> {
        ws.analysis
            .compilation
            .last_evaluated_files()
            .iter()
            .copied()
            .filter(|file_id| *file_id != FileId::VIRTUAL)
            .collect()
    }

    /// Whether some derivation of `file_id` recorded a read of a key `wanted` accepts.
    fn reads(ws: &VirtualWorkspace, file_id: FileId, wanted: impl Fn(&FactKey) -> bool) -> bool {
        let graph = ws.analysis.compilation.get_db().reader_graph();
        graph
            .derivations()
            .filter(|id| id.file_id == file_id)
            .any(|id| graph.reads_of(id).any(|read| wanted(read.key())))
    }

    /// Asserts `ws` has no derivation left stale and settles as a cold build
    /// of `files`, which are in the order of their paths so their ids match.
    fn settles_as_cold(ws: &VirtualWorkspace, files: Vec<(&str, &str)>) {
        let mut cold = VirtualWorkspace::new();
        cold.def_files(files);
        assert_eq!(ws.stale_derivations(), Vec::new());
        assert_eq!(ws.settled_digest(), cold.settled_digest());
    }

    /// A class that moves within its file, where no edit maps its declaration
    /// across, is still declared in the same file with the same flags: a
    /// member lookup on it in another file reads that and nothing else of
    /// where it is declared, so the lookup does not run again.
    #[test]
    fn a_class_moving_in_its_file_re_runs_no_member_lookup_on_it() {
        let mut ws = VirtualWorkspace::new();
        let class_file = ws.def_file(CLASS_FILE, CLASS_BEFORE);
        let reader = ws.def_file(READER_FILE, READER);
        let child = LuaTypeDeclId::global("Child");
        assert!(reads(&ws, reader, |key| {
            *key == FactKey::TypeDeclFiles(child.clone())
        }));

        ws.def_file(CLASS_FILE, CLASS_AFTER);

        assert_eq!(evaluated_files(&ws), vec![class_file]);
        settles_as_cold(&ws, vec![(CLASS_FILE, CLASS_AFTER), (READER_FILE, READER)]);
    }

    /// Declaring the class in a further file changes the files it is declared
    /// in, which the lookup reads: it runs again and settles as a cold build.
    #[test]
    fn declaring_a_class_in_a_further_file_re_runs_a_member_lookup_on_it() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(CLASS_FILE, CLASS_AFTER);
        let reader = ws.def_file(READER_FILE, READER);

        ws.def_file(MORE_CLASS_FILE, "---@class Child\nprint(5)\n");

        assert!(evaluated_files(&ws).contains(&reader));
        settles_as_cold(
            &ws,
            vec![
                (CLASS_FILE, CLASS_AFTER),
                (READER_FILE, READER),
                (MORE_CLASS_FILE, "---@class Child\nprint(5)\n"),
            ],
        );
    }

    /// A super edge that moves within its file, where no edit maps its
    /// position across, is still the same type from the same file with the
    /// same realm: a member lookup through it in another file reads those and
    /// not the position, so the lookup does not run again.
    #[test]
    fn a_super_edge_moving_in_its_file_re_runs_no_member_lookup_through_it() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(BASE_FILE, BASE);
        let class_file = ws.def_file(CLASS_FILE, DERIVED_BEFORE);
        let reader = ws.def_file(READER_FILE, READER);
        let child = LuaTypeDeclId::global("Child");
        assert!(reads(&ws, reader, |key| {
            *key == FactKey::TypeSuperRealms(child.clone(), class_file)
        }));
        assert!(!reads(&ws, reader, |key| {
            matches!(key, FactKey::GmodRealmAt(file_id, _) if *file_id == class_file)
        }));

        ws.def_file(CLASS_FILE, DERIVED_AFTER);

        assert_eq!(evaluated_files(&ws), vec![class_file]);
        settles_as_cold(
            &ws,
            vec![
                (BASE_FILE, BASE),
                (CLASS_FILE, DERIVED_AFTER),
                (READER_FILE, READER),
            ],
        );
    }

    /// The realm at a super edge is read through the edge: the edge landing in
    /// a server branch changes it, so the lookup runs again.
    #[test]
    fn a_super_edge_entering_a_realm_branch_re_runs_a_member_lookup_through_it() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(BASE_FILE, BASE);
        let class_file = ws.def_file(CLASS_FILE, "---@class Child : Base\n");
        let reader = ws.def_file(READER_FILE, READER);
        let realms = FactKey::TypeSuperRealms(LuaTypeDeclId::global("Child"), class_file);
        let before = ws.analysis.compilation.get_db().fact_fingerprint(&realms);

        ws.def_file(CLASS_FILE, DERIVED_IN_BRANCH);

        let after = ws.analysis.compilation.get_db().fact_fingerprint(&realms);
        assert_ne!(after, before);
        assert!(evaluated_files(&ws).contains(&reader));
        settles_as_cold(
            &ws,
            vec![
                (BASE_FILE, BASE),
                (CLASS_FILE, DERIVED_IN_BRANCH),
                (READER_FILE, READER),
            ],
        );
    }

    /// A type declared with two kinds is of the kind its first declaration
    /// in canonical order gives it, so once that declaration goes it is of
    /// the kind the rest give it, as a cold build of what is left is.
    #[test]
    fn a_type_takes_the_kind_of_the_declarations_left_when_one_goes() {
        let foo = LuaTypeDeclId::global("Foo");
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "lua/autorun/sh_a.lua",
            "---@alias Foo string
",
        );
        ws.def_file(
            "lua/autorun/sh_b.lua",
            "---@class Foo
local Foo = {}
",
        );
        let kind = |ws: &VirtualWorkspace| {
            ws.analysis
                .compilation
                .get_db()
                .get_type_index()
                .get_type_decl(&foo)
                .map(|decl| decl.kind())
        };
        let with_alias = kind(&ws);

        ws.def_file(
            "lua/autorun/sh_a.lua",
            "print(1)
",
        );

        assert_eq!(
            (with_alias, kind(&ws)),
            (
                Some(crate::LuaDeclTypeKind::Alias),
                Some(crate::LuaDeclTypeKind::Class)
            )
        );
    }
}
