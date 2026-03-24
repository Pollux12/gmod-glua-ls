use std::error::Error;
use std::str::FromStr;

use log::error;
use lsp_server::{Request, Response};
use lsp_types::Uri;
use lsp_types::request::{
    CallHierarchyIncomingCalls, CallHierarchyOutgoingCalls, CallHierarchyPrepare,
    CodeActionRequest, CodeLensRequest, CodeLensResolve, ColorPresentationRequest, Completion,
    DocumentColor, DocumentDiagnosticRequest, DocumentHighlightRequest, DocumentLinkRequest,
    DocumentLinkResolve, DocumentSymbolRequest, ExecuteCommand, FoldingRangeRequest, Formatting,
    GotoDefinition, GotoImplementation, HoverRequest, InlayHintRequest, InlayHintResolveRequest,
    InlineValueRequest, OnTypeFormatting, PrepareRenameRequest, RangeFormatting, References,
    Rename, Request as LspRequest, ResolveCompletionItem, SelectionRangeRequest,
    SemanticTokensFullRequest, SignatureHelpRequest, WorkspaceDiagnosticRequest,
    WorkspaceSymbolRequest,
};
use serde::Serialize;

use crate::{
    context::{RequestTaskMetadata, ServerContext},
    handlers::{
        diagnostic::{on_pull_document_diagnostic, on_pull_workspace_diagnostic},
        document_type_format::on_type_formatting_handler,
        emmy_gutter::{
            EmmyGutterDetailRequest, EmmyGutterRequest, on_emmy_gutter_detail_handler,
            on_emmy_gutter_handler,
        },
        emmy_syntax_tree::{EmmySyntaxTreeRequest, on_emmy_syntax_tree_handler},
    },
};

use super::{
    call_hierarchy::{
        on_incoming_calls_handler, on_outgoing_calls_handler, on_prepare_call_hierarchy_handler,
    },
    code_actions::on_code_action_handler,
    code_lens::{on_code_lens_handler, on_resolve_code_lens_handler},
    command::on_execute_command_handler,
    completion::{on_completion_handler, on_completion_resolve_handler},
    definition::on_goto_definition_handler,
    doc_search::{GluaDocSearchRequest, on_doc_search_handler},
    document_color::{on_document_color, on_document_color_presentation},
    document_formatting::on_formatting_handler,
    document_highlight::on_document_highlight_handler,
    document_link::{on_document_link_handler, on_document_link_resolve_handler},
    document_range_formatting::on_range_formatting_handler,
    document_selection_range::on_document_selection_range_handle,
    document_symbol::on_document_symbol,
    emmy_annotator::{EmmyAnnotatorRequest, on_emmy_annotator_handler},
    fold_range::on_folding_range_handler,
    gmod_scripted_classes::{
        GmodScriptedClassesRequest, GmodScriptedClassesV2Request, on_gmod_scripted_classes_handler,
        on_gmod_scripted_classes_v2_handler,
    },
    hover::on_hover,
    implementation::on_implementation_handler,
    inlay_hint::{on_inlay_hint_handler, on_resolve_inlay_hint},
    inline_values::on_inline_values_handler,
    references::on_references_handler,
    rename::{on_prepare_rename_handler, on_rename_handler},
    semantic_token::on_semantic_token_handler,
    signature_helper::on_signature_helper_handler,
    workspace_symbol::on_workspace_symbol_handler,
};

fn request_task_metadata<T: Serialize>(method: &'static str, params: &T) -> RequestTaskMetadata {
    let value = match serde_json::to_value(params) {
        Ok(value) => value,
        Err(_) => return RequestTaskMetadata::new(method, None),
    };

    RequestTaskMetadata::new(method, extract_uri_from_value(&value))
}

fn extract_uri_from_value(value: &serde_json::Value) -> Option<Uri> {
    [
        value
            .get("textDocument")
            .and_then(|text_document| text_document.get("uri")),
        value
            .get("textDocumentPosition")
            .and_then(|position| position.get("textDocument"))
            .and_then(|text_document| text_document.get("uri")),
        value
            .get("textDocumentPositionParams")
            .and_then(|position| position.get("textDocument"))
            .and_then(|text_document| text_document.get("uri")),
        value.get("item").and_then(|item| item.get("uri")),
        value.get("data").and_then(|data| data.get("uri")),
    ]
    .into_iter()
    .flatten()
    .find_map(|uri| Uri::from_str(uri.as_str()?).ok())
}

macro_rules! dispatch_request {
    ($request:expr, $context:expr, {
        $($req_type:ty => $handler:expr),* $(,)?
    }, content_modified_if_dirty: {
        $($dirty_req_type:ty => $dirty_handler:expr),* $(,)?
    }) => {
        match $request.method.as_str() {
            $(
                <$req_type>::METHOD => {
                    if let Ok((id, params)) = $request.extract::<<$req_type as LspRequest>::Params>(<$req_type>::METHOD) {
                        let snapshot = $context.snapshot();
                        let task_metadata = request_task_metadata(<$req_type>::METHOD, &params);
                        $context.task(id.clone(), task_metadata, |cancel_token| async move {
                            let result = $handler(snapshot, params, cancel_token).await;
                            Some(Response::new_ok(id, result))
                        }).await;
                        return Ok(());
                    }
                }
            )*
            $(
                <$dirty_req_type>::METHOD => {
                    if let Ok((id, params)) = $request.extract::<<$dirty_req_type as LspRequest>::Params>(<$dirty_req_type>::METHOD) {
                        let snapshot = $context.snapshot();
                        let task_metadata = request_task_metadata(<$dirty_req_type>::METHOD, &params);
                        $context.task(id.clone(), task_metadata, |cancel_token| async move {
                            // When changes are pending reindex, return ContentModified
                            // so the client keeps its previous results instead of
                            // clearing them (which causes flickering / layout shifts).
                            if snapshot.debounced_analysis().is_dirty() {
                                return Some(Response::new_err(
                                    id,
                                    lsp_server::ErrorCode::ContentModified as i32,
                                    "content modified".to_owned(),
                                ));
                            }
                            let result = $dirty_handler(snapshot, params, cancel_token).await;
                            Some(Response::new_ok(id, result))
                        }).await;
                        return Ok(());
                    }
                }
            )*
            method => {
                error!("handler not found for request: {}", method);
                let response = Response::new_err(
                    $request.id.clone(),
                    lsp_server::ErrorCode::MethodNotFound as i32,
                    "handler not found".to_string(),
                );
                $context.send(response);
            }
        }
    };
}

pub async fn on_request_handler(
    req: Request,
    server_context: &mut ServerContext,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    dispatch_request!(req, server_context, {
        HoverRequest => on_hover,
        DocumentSymbolRequest => on_document_symbol,
        FoldingRangeRequest => on_folding_range_handler,
        DocumentColor => on_document_color,
        ColorPresentationRequest => on_document_color_presentation,
        DocumentLinkRequest => on_document_link_handler,
        DocumentLinkResolve => on_document_link_resolve_handler,
        EmmyGutterRequest => on_emmy_gutter_handler,
        EmmyGutterDetailRequest => on_emmy_gutter_detail_handler,
        EmmySyntaxTreeRequest => on_emmy_syntax_tree_handler,
        EmmyAnnotatorRequest => on_emmy_annotator_handler,
        SelectionRangeRequest => on_document_selection_range_handle,
        Completion => on_completion_handler,
        ResolveCompletionItem => on_completion_resolve_handler,
        InlayHintResolveRequest => on_resolve_inlay_hint,
        CodeLensRequest => on_code_lens_handler,
        GotoDefinition => on_goto_definition_handler,
        GotoImplementation => on_implementation_handler,
        References => on_references_handler,
        Rename => on_rename_handler,
        PrepareRenameRequest => on_prepare_rename_handler,
        CodeLensResolve => on_resolve_code_lens_handler,
        SignatureHelpRequest => on_signature_helper_handler,
        DocumentHighlightRequest => on_document_highlight_handler,
        ExecuteCommand => on_execute_command_handler,
        CodeActionRequest => on_code_action_handler,
        InlineValueRequest => on_inline_values_handler,
        WorkspaceSymbolRequest => on_workspace_symbol_handler,
        GluaDocSearchRequest => on_doc_search_handler,
        GmodScriptedClassesRequest => on_gmod_scripted_classes_handler,
        GmodScriptedClassesV2Request => on_gmod_scripted_classes_v2_handler,
        InlayHintRequest => on_inlay_hint_handler,
        Formatting => on_formatting_handler,
        RangeFormatting => on_range_formatting_handler,
        OnTypeFormatting => on_type_formatting_handler,
        CallHierarchyPrepare => on_prepare_call_hierarchy_handler,
        CallHierarchyIncomingCalls => on_incoming_calls_handler,
        CallHierarchyOutgoingCalls => on_outgoing_calls_handler,
        SemanticTokensFullRequest => on_semantic_token_handler,
        DocumentDiagnosticRequest => on_pull_document_diagnostic,
        WorkspaceDiagnosticRequest => on_pull_workspace_diagnostic,
    }, content_modified_if_dirty: {
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::extract_uri_from_value;
    use glua_code_analysis::LuaDeclId;
    use rowan::TextSize;
    use serde_json::json;
    use std::str::FromStr;

    use lsp_types::Uri;

    use crate::handlers::{
        code_lens::{CodeLensData, CodeLensResolveData},
        completion::{CompletionData, CompletionDataType},
    };

    #[test]
    fn extracts_text_document_uri() {
        let uri = Uri::from_str("file:///document.lua").expect("uri should parse");
        let value = json!({
            "textDocument": {
                "uri": uri.clone(),
            }
        });

        assert_eq!(extract_uri_from_value(&value), Some(uri));
    }

    #[test]
    fn extracts_nested_text_document_position_uri() {
        let uri = Uri::from_str("file:///completion.lua").expect("uri should parse");
        let value = json!({
            "textDocumentPosition": {
                "textDocument": {
                    "uri": uri.clone(),
                }
            }
        });

        assert_eq!(extract_uri_from_value(&value), Some(uri));
    }

    #[test]
    fn returns_none_without_known_uri_shape() {
        let value = json!({
            "item": {
                "label": "no-uri",
            }
        });

        assert_eq!(extract_uri_from_value(&value), None);
    }

    #[test]
    fn resolves_completion_item_uri_from_completion_data() {
        let uri = Uri::from_str("file:///resolve_completion.lua").expect("uri should parse");
        let params = json!({
            "label": "foo",
            "data": serde_json::to_value(CompletionData {
                field_id: 1_u32.into(),
                uri: Some(uri.clone()),
                typ: CompletionDataType::Module("foo".to_string()),
                overload_count: None,
            })
            .expect("completion data should serialize"),
        });

        assert_eq!(extract_uri_from_value(&params), Some(uri));
    }

    #[test]
    fn resolves_code_lens_uri_from_code_lens_data() {
        let uri = Uri::from_str("file:///resolve_code_lens.lua").expect("uri should parse");
        let params = json!({
            "data": serde_json::to_value(CodeLensResolveData {
                uri: Some(uri.clone()),
                payload: CodeLensData::DeclId(LuaDeclId::new(1_u32.into(), TextSize::new(0))),
            })
            .expect("code lens data should serialize"),
        });

        assert_eq!(extract_uri_from_value(&params), Some(uri));
    }
}
