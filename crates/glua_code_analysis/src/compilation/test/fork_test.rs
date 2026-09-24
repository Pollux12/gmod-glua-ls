#[cfg(test)]
mod tests {
    use crate::{EmmyLuaAnalysis, FileId, LuaType, VirtualWorkspace};
    use googletest::prelude::*;

    const PROVIDER: &str = r#"
        provider = provider or {}
        ---@return string
        function provider.Describe() end
    "#;
    const EDITED_PROVIDER: &str = r#"
        provider = provider or {}
        ---@return number
        function provider.Describe() end
    "#;
    const CONSUMER: &str = r#"
        local described = provider.Describe()
    "#;

    fn workspace(provider: &str) -> (VirtualWorkspace, FileId) {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_file("lua/provider.lua", provider);
        let consumer = ws.def_file("lua/consumer.lua", CONSUMER);
        (ws, consumer)
    }

    fn described_type(analysis: &EmmyLuaAnalysis, consumer: FileId) -> LuaType {
        let db = analysis.compilation.get_db();
        let decl_id = db
            .get_decl_index()
            .get_decl_tree(&consumer)
            .and_then(|tree| {
                tree.get_decls()
                    .values()
                    .find(|decl| decl.get_name() == "described")
                    .map(|decl| decl.get_id())
            })
            .expect("declaration");
        db.get_type_index()
            .get_type_cache(&decl_id.into())
            .map(|cache| cache.as_type().clone())
            .expect("declaration type")
    }

    #[gtest]
    fn fork_edited_incrementally_leaves_base_untouched_and_matches_cold_build() {
        let (ws, consumer) = workspace(PROVIDER);
        let base = &ws.analysis;
        let base_type = described_type(base, consumer);

        let mut fork = base.fork();
        let provider_uri = ws.virtual_url_generator.new_uri("lua/provider.lua");
        let provider = fork
            .update_file_text_only(&provider_uri, EDITED_PROVIDER.to_string())
            .expect("provider file id");
        fork.reindex_files(vec![provider]);

        let (cold, cold_consumer) = workspace(EDITED_PROVIDER);
        expect_that!(described_type(base, consumer), eq(&base_type));
        expect_that!(
            base.compilation
                .get_db()
                .get_vfs()
                .get_file_content(&provider),
            some(eq(PROVIDER))
        );
        expect_that!(
            described_type(&fork, consumer),
            eq(&described_type(&cold.analysis, cold_consumer))
        );
        expect_that!(described_type(&fork, consumer), not(eq(&base_type)));
    }
}
