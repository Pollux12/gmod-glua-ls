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
    cancel_token: CancellationToken,
) -> Option<SyntaxTreeResponse> {
    let uri = Uri::from_str(&params.uri).ok()?;

    // Ranges are offsets into this document's tree, so the tree must be the one
    // the client is asking about — the same gate the formatting handlers use.
    // Index freshness is not needed here.
    if !context
        .wait_until_latest_document_version_applied(&uri, &cancel_token)
        .await
    {
        return None;
    }

    let analysis = context.read_analysis(&cancel_token).await?;
    let file_id = analysis.get_file_id(&uri)?;
    let semantic_model = analysis.compilation.get_semantic_model(file_id)?;

    let root = semantic_model.get_root();
    let content = format!("{:#?}", root.syntax());
    Some(SyntaxTreeResponse { content })
}
