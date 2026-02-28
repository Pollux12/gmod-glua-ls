mod emmy_syntax_tree_request;

use std::str::FromStr;

use glua_parser::LuaAstNode;
use lsp_types::Uri;
use tokio_util::sync::CancellationToken;

use crate::{
    context::ServerContextSnapshot,
    handlers::emmy_syntax_tree::emmy_syntax_tree_request::{
        EmmySyntaxTreeParams, SyntaxTreeResponse,
    },
};
pub use emmy_syntax_tree_request::*;

pub async fn on_emmy_syntax_tree_handler(
    context: ServerContextSnapshot,
    params: EmmySyntaxTreeParams,
    _: CancellationToken,
) -> Option<SyntaxTreeResponse> {
    let uri = Uri::from_str(&params.uri).ok()?;
    let analysis = context.analysis().read().await;
    let file_id = analysis.get_file_id(&uri)?;
    let semantic_model = analysis.compilation.get_semantic_model(file_id)?;

    let root = semantic_model.get_root();
    let content = format!("{:#?}", root.syntax());
    Some(SyntaxTreeResponse { content })
}
