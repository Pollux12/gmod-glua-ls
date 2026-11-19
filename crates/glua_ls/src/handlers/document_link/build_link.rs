use std::path::PathBuf;

use glua_code_analysis::{DbIndex, FileId, LuaDocument, file_path_to_uri};
use glua_parser::{
    LuaAstNode, LuaAstToken, LuaCallArgList, LuaCallExpr, LuaLiteralExpr, LuaStringToken,
    LuaSyntaxNode,
};
use lsp_types::DocumentLink;

pub fn build_links(
    db: &DbIndex,
    root: LuaSyntaxNode,
    document: &LuaDocument,
) -> Option<Vec<DocumentLink>> {
    let string_tokens = root
        .descendants_with_tokens()
        .filter_map(|it| it.into_token())
        .filter_map(LuaStringToken::cast);

    let mut result = vec![];
    for token in string_tokens {
        try_build_file_link(db, token, document, &mut result);
    }

    Some(result)
}

fn try_build_file_link(
    db: &DbIndex,
    token: LuaStringToken,
    document: &LuaDocument,
    result: &mut Vec<DocumentLink>,
) -> Option<()> {
    if let Some(target_file_id) = linked_file(db, document.get_file_id(), &token) {
        result.push(DocumentLink {
            target: Some(db.get_vfs().get_uri(&target_file_id)?.clone()),
            range: document.to_lsp_range(token.get_range())?,
            tooltip: None,
            data: None,
        });
        return Some(());
    }

    let file_path = token.get_value();
    if file_path.find(['\\', '/']).is_some() && has_linkable_path_component(&file_path) {
        let suffix_path = PathBuf::from(file_path);
        if suffix_path.exists() {
            if let Some(uri) = file_path_to_uri(&suffix_path) {
                let document_link = DocumentLink {
                    target: Some(uri),
                    range: document.to_lsp_range(token.get_range())?,
                    tooltip: None,
                    data: None,
                };

                result.push(document_link);
            }
            return Some(());
        }

        let resource_paths = db.get_effective_resource_paths();
        for resource_path in resource_paths {
            let full_path = resource_path.join(&suffix_path);
            if full_path.exists() {
                if let Some(uri) = file_path_to_uri(&full_path) {
                    let document_link = DocumentLink {
                        target: Some(uri),
                        range: document.to_lsp_range(token.get_range())?,
                        tooltip: None,
                        data: None,
                    };

                    result.push(document_link);
                }
                return Some(());
            }
        }
    }

    Some(())
}

fn has_linkable_path_component(path: &str) -> bool {
    path.split(['\\', '/'])
        .any(|component| !component.is_empty() && component != "." && component != "..")
}

/// The file the path string `token` of `file_id` names: the one a load call
/// taking it loads, or the module a `require` of it finds.
pub fn linked_file(db: &DbIndex, file_id: FileId, token: &LuaStringToken) -> Option<FileId> {
    let range = token.get_range();
    let loaded = db
        .get_file_dependencies_index()
        .get_dependency_sites(&file_id)
        .unwrap_or_default()
        .iter()
        .find(|site| site.range == range)
        .and_then(|site| site.target_file_id);
    if loaded.is_some() {
        return loaded;
    }
    if !is_require_path(token.clone()).unwrap_or(false) {
        return None;
    }
    Some(
        db.get_module_index()
            .find_module(&token.get_value())?
            .file_id,
    )
}

fn is_require_path(token: LuaStringToken) -> Option<bool> {
    let call_expr = token
        .get_parent::<LuaLiteralExpr>()?
        .get_parent::<LuaCallArgList>()?
        .get_parent::<LuaCallExpr>()?;

    Some(call_expr.is_require())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::test_lib::{ProviderVirtualWorkspace, check};
    use googletest::prelude::*;

    #[gtest]
    fn separator_only_paths_are_not_linkable() {
        for path in [r"\", r"\\", "/"] {
            expect_that!(has_linkable_path_component(path), eq(false));
        }
    }

    #[gtest]
    fn paths_with_real_components_are_linkable() {
        for path in ["materials/icon.png", r"materials\icon.png", "/lua/autorun"] {
            expect_that!(has_linkable_path_component(path), eq(true));
        }
    }

    #[gtest]
    fn escaped_backslash_string_does_not_create_document_link() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let file_id = ws.def(r#"local value = "\\\\""#);
        let semantic_model = check!(ws.analysis.compilation.get_semantic_model(file_id));
        let links = check!(build_links(
            semantic_model.get_db(),
            semantic_model.get_root().syntax().clone(),
            &semantic_model.get_document(),
        ));

        expect_that!(links, is_empty());
        Ok(())
    }

    fn link_targets(
        ws: &ProviderVirtualWorkspace,
        file_id: glua_code_analysis::FileId,
    ) -> Vec<String> {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        build_links(
            semantic_model.get_db(),
            semantic_model.get_root().syntax().clone(),
            &semantic_model.get_document(),
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|link| Some(link.target?.as_str().to_string()))
        .collect()
    }

    /// A path a load call takes links to the file it loads, wherever the
    /// workspace root is, including one relative to the loading file.
    #[gtest]
    fn a_load_path_links_to_the_file_it_loads() {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.def_file("lua/mylib/ids.lua", "return {}");
        ws.def_file("lua/entities/thing/shared.lua", "return {}");
        let init = ws.def_file(
            "lua/entities/thing/init.lua",
            "include(\"shared.lua\")\nAddCSLuaFile(\"mylib/ids.lua\")\n",
        );

        let targets = link_targets(&ws, init);
        expect_that!(
            targets,
            elements_are![
                ends_with("lua/entities/thing/shared.lua"),
                ends_with("lua/mylib/ids.lua")
            ]
        );
    }

    /// A path given to a loader that picks `include` or `AddCSLuaFile` by
    /// realm links to the file it loads.
    #[gtest]
    fn a_path_given_to_a_realm_dispatching_loader_links_to_the_file() {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.def_file("lua/mylib/apiclient.lua", "return {}");
        ws.def_file(
            "lua/autorun/sh_util.lua",
            r#"function IncludeFile(fileName, realm)
    if (realm == "server" or fileName:find("sv_")) and SERVER then
        return include(fileName)
    elseif realm == "client" or fileName:find("cl_") then
        if SERVER then
            AddCSLuaFile(fileName)
        else
            return include(fileName)
        end
    end
end
"#,
        );
        let boot = ws.def_file(
            "lua/autorun/sv_boot.lua",
            "IncludeFile(\"mylib/apiclient.lua\", \"server\")\n",
        );

        let targets = link_targets(&ws, boot);
        expect_that!(targets, elements_are![ends_with("lua/mylib/apiclient.lua")]);
    }
}
