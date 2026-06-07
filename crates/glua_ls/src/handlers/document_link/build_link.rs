use std::path::PathBuf;

use glua_code_analysis::{DbIndex, LuaDocument, file_path_to_uri};
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
    if is_require_path(token.clone()).unwrap_or(false) {
        try_build_module_link(db, token, document, result);
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

fn try_build_module_link(
    db: &DbIndex,
    token: LuaStringToken,
    document: &LuaDocument,
    result: &mut Vec<DocumentLink>,
) -> Option<()> {
    let module_path = token.get_value();
    let module_index = db.get_module_index();
    let founded_module = module_index.find_module(&module_path)?;
    let file_id = founded_module.file_id;
    let vfs = db.get_vfs();
    let uri = vfs.get_uri(&file_id)?;
    let range = token.get_range();
    let lsp_range = document.to_lsp_range(range)?;
    let document_link = DocumentLink {
        target: Some(uri.clone()),
        range: lsp_range,
        tooltip: None,
        data: None,
    };

    result.push(document_link);

    Some(())
}

pub fn is_require_path(token: LuaStringToken) -> Option<bool> {
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
}
