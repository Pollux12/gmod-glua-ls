mod build_annotator;
mod emmy_annotator_request;

use std::str::FromStr;

use build_annotator::build_annotators;
pub use emmy_annotator_request::*;
use lsp_types::Uri;
use tokio_util::sync::CancellationToken;

use crate::context::ServerContextSnapshot;

pub async fn on_emmy_annotator_handler(
    context: ServerContextSnapshot,
    params: EmmyAnnotatorParams,
    cancel_token: CancellationToken,
) -> Option<Vec<EmmyAnnotator>> {
    if cancel_token.is_cancelled() {
        return None;
    }
    let uri = Uri::from_str(&params.uri).ok()?;
    let analysis = context.read_analysis(&cancel_token).await?;

    if cancel_token.is_cancelled() {
        return None;
    }

    let file_id = analysis.get_file_id(&uri)?;
    let semantic_model = analysis.compilation.get_semantic_model(file_id)?;
    Some(build_annotators(&semantic_model))
}
