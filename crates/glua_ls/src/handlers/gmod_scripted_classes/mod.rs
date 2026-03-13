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
) -> Option<Vec<LegacyGmodScriptedClassEntry>> {
    let analysis = context.read_analysis(&cancel_token).await?;
    let db = analysis.compilation.get_db();
    build_gmod_scripted_classes(db, &cancel_token).map(|result| {
        result
            .entries
            .into_iter()
            .map(|entry| LegacyGmodScriptedClassEntry {
                uri: entry.uri,
                class_type: entry.class_type,
                class_name: entry.class_name,
                range: entry.range,
            })
            .collect()
    })
}

pub async fn on_gmod_scripted_classes_v2_handler(
    context: ServerContextSnapshot,
    _params: GmodScriptedClassesParams,
    cancel_token: CancellationToken,
) -> Option<GmodScriptedClassesResult> {
    let analysis = context.read_analysis(&cancel_token).await?;
    let db = analysis.compilation.get_db();
    build_gmod_scripted_classes(db, &cancel_token)
}
