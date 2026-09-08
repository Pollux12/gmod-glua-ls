#[cfg(test)]
mod test {
    use crate::VirtualWorkspace;

    #[test]
    fn test_1() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "A.lua",
            r#"

            ---@export
            return {
               newField = 1
            }
        "#,
        );

        ws.def(
            r#"
            local A = require("A")
            A.newField = 2
        "#,
        );
    }

    /// A module whose export settles late (its dependency lands after its own
    /// walk) still publishes the export through the versioned epoch path.
    #[test]
    fn deferred_module_export_resolves() {
        let mut ws = VirtualWorkspace::new();
        let files = ws.def_files(vec![
            ("a_exp.lua", r#"return require("z_dep")"#),
            ("z_dep.lua", r#"return { answer = 42 }"#),
        ]);
        let (exporter_id, dep_id) = (files[0], files[1]);

        let db = ws.analysis.compilation.get_db();
        let exporter = db
            .get_module_index()
            .get_module(exporter_id)
            .expect("expected the exporter module entry");
        assert!(
            exporter.export_type.is_some(),
            "the deferred module export must resolve"
        );
        let dep = db
            .get_module_index()
            .get_module(dep_id)
            .expect("expected the dependency module entry");
        assert!(
            dep.export_type.is_some(),
            "the dependency module export must resolve"
        );
    }
}
