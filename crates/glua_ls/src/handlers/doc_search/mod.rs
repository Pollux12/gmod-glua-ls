mod build_doc_search;
mod doc_search_request;

use tokio_util::sync::CancellationToken;

use crate::context::ServerContextSnapshot;

use self::build_doc_search::build_doc_search;
pub use doc_search_request::*;

pub async fn on_doc_search_handler(
    context: ServerContextSnapshot,
    params: GluaDocSearchParams,
    cancel_token: CancellationToken,
) -> Option<GluaDocSearchResponse> {
    let analysis = context.analysis().read().await;
    let db = analysis.compilation.get_db();
    let items = build_doc_search(
        db,
        params.query.as_str(),
        params.limit.unwrap_or(20),
        &cancel_token,
    )?;

    Some(GluaDocSearchResponse { items })
}
