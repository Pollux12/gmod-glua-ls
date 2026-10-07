mod build_annotator;
mod emmy_annotator_request;

use std::str::FromStr;

use build_annotator::build_annotators;
pub use emmy_annotator_request::*;
use lsp_types::{Uri, request::Request};
use tokio_util::sync::CancellationToken;

use crate::context::{Freshness, ServerContextSnapshot};

pub async fn on_emmy_annotator_handler(
    context: ServerContextSnapshot,
    params: EmmyAnnotatorParams,
    cancel_token: CancellationToken,
) -> Option<EmmyAnnotatorResult> {
    if cancel_token.is_cancelled() {
        return None;
    }

    let uri = Uri::from_str(&params.uri).ok()?;
    let references = params.references.unwrap_or(true);

    // Wait until this document's entries match its text so we compute
    // against a consistent tree and index. Cancel-aware: bails out when a
    // new didChange to this document cancels its requests.
    if context
        .debounced_analysis()
        .wait_until_file_fresh_for(&cancel_token, EmmyAnnotatorRequest::METHOD, &uri)
        .await
        != Freshness::Fresh
    {
        return None;
    }

    let result = {
        // While we hold this read lock, no writes (VFS updates, reindex)
        // can proceed, so tree and index are guaranteed consistent.
        let analysis = context.read_analysis(&cancel_token).await?;

        if cancel_token.is_cancelled() {
            return None;
        }

        let file_id = analysis.get_file_id(&uri)?;
        let semantic_model = analysis.compilation.get_semantic_model(file_id)?;
        let annotators = build_annotators(&semantic_model, references);
        EmmyAnnotatorResult { annotators }
    };

    Some(result)
}
