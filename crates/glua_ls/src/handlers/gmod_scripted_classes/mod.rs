mod build_gmod_scripted_classes;
mod gmod_scripted_classes_request;

use tokio_util::sync::CancellationToken;

use crate::context::ServerContextSnapshot;

use self::build_gmod_scripted_classes::build_gmod_scripted_classes;
pub use gmod_scripted_classes_request::*;

pub async fn on_gmod_scripted_classes_handler(
    context: ServerContextSnapshot,
    _params: GmodScriptedClassesParams,
    cancel_token: CancellationToken,
) -> Option<Vec<GmodScriptedClassEntry>> {
    let analysis = context.read_analysis(&cancel_token).await?;
    let db = analysis.compilation.get_db();
    build_gmod_scripted_classes(db, &cancel_token)
}
