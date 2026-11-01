use std::error::Error;
use std::str::FromStr;

use log::error;
use lsp_server::{Request, Response};
use lsp_types::Uri;
use lsp_types::request::{
    CallHierarchyIncomingCalls, CallHierarchyOutgoingCalls, CallHierarchyPrepare,
    CodeActionRequest, CodeLensRequest, CodeLensResolve, ColorPresentationRequest, Completion,
    DocumentColor, DocumentHighlightRequest, DocumentLinkRequest, DocumentLinkResolve,
    DocumentSymbolRequest, ExecuteCommand, FoldingRangeRequest, Formatting, GotoDefinition,
    GotoImplementation, HoverRequest, InlayHintRequest, InlayHintResolveRequest,
    InlineValueRequest, OnTypeFormatting, PrepareRenameRequest, RangeFormatting, References,
    Rename, Request as LspRequest, ResolveCompletionItem, SelectionRangeRequest,
    SemanticTokensFullRequest, SignatureHelpRequest, WillRenameFiles, WorkspaceSymbolRequest,
};
use serde::Serialize;

use crate::{
    context::{Freshness, RequestTaskMetadata, ServerContext},
    handlers::{
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
    hover::hover_expand::on_hover_expand_handler,
    hover::hover_expand_request::GluaHoverExpandRequest,
    hover::on_hover,
    implementation::on_implementation_handler,
    inlay_hint::{on_inlay_hint_handler, on_resolve_inlay_hint},
    inline_values::on_inline_values_handler,
    references::on_references_handler,
    rename::{on_prepare_rename_handler, on_rename_handler},
    semantic_token::on_semantic_token_handler,
    signature_helper::on_signature_helper_handler,
    workspace::on_will_rename_files_handler,
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
        value.get("uri"),
    ]
    .into_iter()
    .flatten()
    .find_map(|uri| Uri::from_str(uri.as_str()?).ok())
}

/// Whether a request's result points into documents other than its own: any
/// document mid-edit, not only its own, would leave those places wrong.
fn result_spans_documents(method: &str) -> bool {
    matches!(
        method,
        <References as LspRequest>::METHOD
            | <Rename as LspRequest>::METHOD
            | <GotoImplementation as LspRequest>::METHOD
            | <CallHierarchyIncomingCalls as LspRequest>::METHOD
            | <CallHierarchyOutgoingCalls as LspRequest>::METHOD
    )
}

fn content_modified(id: lsp_server::RequestId) -> Option<Response> {
    Some(Response::new_err(
        id,
        lsp_server::ErrorCode::ContentModified as i32,
        "content modified".to_owned(),
    ))
}

macro_rules! dispatch_request {
    ($request:expr, $context:expr, {
        $($req_type:ty => $handler:expr),* $(,)?
    }, wait_for_taken_text: {
        $($text_req_type:ty => $text_handler:expr),* $(,)?
    }, wait_for_fresh_index: {
        $($fresh_req_type:ty => $fresh_handler:expr),* $(,)?
    }, content_modified_if_client_retries: {
        $($retry_req_type:ty => $retry_handler:expr),* $(,)?
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
                <$text_req_type>::METHOD => {
                    if let Ok((id, params)) = $request.extract::<<$text_req_type as LspRequest>::Params>(<$text_req_type>::METHOD) {
                        let snapshot = $context.snapshot();
                        let task_metadata = request_task_metadata(<$text_req_type>::METHOD, &params);
                        let target_uri = task_metadata.uri.clone();
                        $context.task(id.clone(), task_metadata, |cancel_token| async move {
                            // The analysis reads a document's text once a take
                            // installs it; positions sent for newer text would
                            // not line up with the older one.
                            if let Some(uri) = target_uri.as_ref() {
                                match snapshot
                                    .debounced_analysis()
                                    .wait_until_text_taken_for(
                                        &cancel_token,
                                        <$text_req_type>::METHOD,
                                        uri,
                                    )
                                    .await
                                {
                                    Freshness::Fresh => {}
                                    Freshness::Cancelled => return None,
                                    Freshness::LoopDead => {
                                        cancel_token.cancel();
                                        return None;
                                    }
                                }
                            }
                            let result = $text_handler(snapshot, params, cancel_token).await;
                            Some(Response::new_ok(id, result))
                        }).await;
                        return Ok(());
                    }
                }
            )*
            $(
                <$fresh_req_type>::METHOD => {
                    if let Ok((id, params)) = $request.extract::<<$fresh_req_type as LspRequest>::Params>(<$fresh_req_type>::METHOD) {
                        let snapshot = $context.snapshot();
                        let task_metadata = request_task_metadata(<$fresh_req_type>::METHOD, &params);
                        let target_uri = task_metadata
                            .uri
                            .clone()
                            .filter(|_| !result_spans_documents(<$fresh_req_type>::METHOD));
                        $context.task(id.clone(), task_metadata, |cancel_token| async move {
                            if !crate::util::ls_profile::ls_profile_enabled() {
                                // Symbol resolution against a stale index silently
                                // returns empty; wait for the reindex. A request
                                // whose result stays in its own file waits only
                                // for that file.
                                let freshness = match target_uri.as_ref() {
                                    Some(uri) => {
                                        snapshot
                                            .debounced_analysis()
                                            .wait_until_file_fresh_for(
                                                &cancel_token,
                                                <$fresh_req_type>::METHOD,
                                                uri,
                                            )
                                            .await
                                    }
                                    None => {
                                        snapshot
                                            .debounced_analysis()
                                            .wait_until_fresh_for(
                                                &cancel_token,
                                                <$fresh_req_type>::METHOD,
                                            )
                                            .await
                                    }
                                };
                                match freshness {
                                    Freshness::Fresh => {}
                                    Freshness::Cancelled => return None,
                                    // Freshness can never arrive: fail as
                                    // cancelled so the client re-sends instead
                                    // of recording an internal error.
                                    Freshness::LoopDead => {
                                        cancel_token.cancel();
                                        return None;
                                    }
                                }
                                let result = $fresh_handler(snapshot, params, cancel_token).await;
                                Some(Response::new_ok(id, result))
                            } else {
                                // Profiled path: identical wake-up behavior, plus
                                // one `[profile] ls_request` line
                                // accumulating fresh-wait by observed state.
                                // `analysis_read_wait` is contributed by
                                // `read_analysis` via the scoped accumulator;
                                // `handler_ms` comes from the completion
                                // handler's guard when present, else from the
                                // dispatch-side measurement.
                                let seq = crate::util::ls_profile::next_ls_seq();
                                let total_start = std::time::Instant::now();
                                let id_string = format!("{:?}", id);
                                let method = <$fresh_req_type>::METHOD;
                                let accum =
                                    std::sync::Arc::new(crate::util::ls_profile::LsRequestAccum::new());
                                let accum_scope = accum.clone();
                                crate::util::ls_profile::LS_REQUEST_ACCUM
                                    .scope(accum_scope, async move {
                                        let (freshness, breakdown) = match target_uri.as_ref() {
                                            Some(uri) => {
                                                snapshot
                                                    .debounced_analysis()
                                                    .wait_until_file_fresh_for_profiled(
                                                        &cancel_token,
                                                        <$fresh_req_type>::METHOD,
                                                        uri,
                                                    )
                                                    .await
                                            }
                                            None => {
                                                snapshot
                                                    .debounced_analysis()
                                                    .wait_until_fresh_for_profiled(
                                                        &cancel_token,
                                                        <$fresh_req_type>::METHOD,
                                                    )
                                                    .await
                                            }
                                        };
                                        if freshness != Freshness::Fresh {
                                            let total = total_start.elapsed();
                                            let read_wait = accum.take_read_wait();
                                            let handler_ms =
                                                accum.take_handler_time().unwrap_or_default();
                                            crate::util::ls_profile::emit_ls_request(
                                                seq,
                                                &id_string,
                                                method,
                                                total,
                                                breakdown.total,
                                                breakdown.in_flight,
                                                breakdown.blocked,
                                                read_wait,
                                                handler_ms,
                                            );
                                            if freshness == Freshness::LoopDead {
                                                cancel_token.cancel();
                                            }
                                            return None;
                                        }
                                        let handler_start = std::time::Instant::now();
                                        let result =
                                            $fresh_handler(snapshot, params, cancel_token).await;
                                        let handler_measured = handler_start.elapsed();
                                        let handler_ms = accum
                                            .take_handler_time()
                                            .unwrap_or(handler_measured);
                                        let read_wait = accum.take_read_wait();
                                        let total = total_start.elapsed();
                                        crate::util::ls_profile::emit_ls_request(
                                            seq,
                                            &id_string,
                                            method,
                                            total,
                                            breakdown.total,
                                            breakdown.in_flight,
                                            breakdown.blocked,
                                            read_wait,
                                            handler_ms,
                                        );
                                        Some(Response::new_ok(id, result))
                                    })
                                    .await
                            }
                        }).await;
                        return Ok(());
                    }
                }
            )*
            $(
                <$retry_req_type>::METHOD => {
                    if let Ok((id, params)) = $request.extract::<<$retry_req_type as LspRequest>::Params>(<$retry_req_type>::METHOD) {
                        let snapshot = $context.snapshot();
                        let task_metadata = request_task_metadata(<$retry_req_type>::METHOD, &params);
                        let target_uri = task_metadata.uri.clone();
                        $context.task(id.clone(), task_metadata, |cancel_token| async move {
                            let debounced = snapshot.debounced_analysis_arc();
                            // Aimed at one document, so its own entries are what
                            // it needs: another file mid-edit must not refuse it.
                            let stale = || async {
                                match target_uri.as_ref() {
                                    Some(uri) => !debounced.file_is_answerable(uri).await,
                                    None => debounced.is_dirty(),
                                }
                            };

                            // A client that doesn't retry ContentModified must
                            // get a real result: wait for freshness instead.
                            if !snapshot
                                .lsp_features()
                                .retries_on_content_modified(<$retry_req_type>::METHOD)
                            {
                                let freshness = match target_uri.as_ref() {
                                    Some(uri) => {
                                        debounced
                                            .wait_until_file_fresh_for(
                                                &cancel_token,
                                                <$retry_req_type>::METHOD,
                                                uri,
                                            )
                                            .await
                                    }
                                    None => {
                                        debounced
                                            .wait_until_fresh_for(
                                                &cancel_token,
                                                <$retry_req_type>::METHOD,
                                            )
                                            .await
                                    }
                                };
                                match freshness {
                                    Freshness::Fresh => {}
                                    Freshness::Cancelled => return None,
                                    // Freshness can never arrive: fail as
                                    // cancelled so the client re-sends instead
                                    // of recording an internal error.
                                    Freshness::LoopDead => {
                                        cancel_token.cancel();
                                        return None;
                                    }
                                }
                                let result = $retry_handler(snapshot, params, cancel_token).await;
                                return Some(Response::new_ok(id, result));
                            }

                            if stale().await {
                                debounced.request_refused().await;
                                return content_modified(id);
                            }

                            let result =
                                $retry_handler(snapshot.clone(), params, cancel_token).await;

                            // An edit landed while we worked.
                            if stale().await {
                                return content_modified(id);
                            }

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
        // Reads the index but performs its own wait to control the cancel
        // response.
        EmmyAnnotatorRequest => on_emmy_annotator_handler,
        CodeLensRequest => on_code_lens_handler,
        InlayHintRequest => on_inlay_hint_handler,
    }, wait_for_taken_text: {
        // Must not resolve declarations/members/globals through the index —
        // those need the `wait_for_fresh_index` arm.
        FoldingRangeRequest => on_folding_range_handler,
        EmmySyntaxTreeRequest => on_emmy_syntax_tree_handler,
        SelectionRangeRequest => on_document_selection_range_handle,
        Formatting => on_formatting_handler,
        RangeFormatting => on_range_formatting_handler,
        OnTypeFormatting => on_type_formatting_handler,
    }, wait_for_fresh_index: {
        Completion => on_completion_handler,
        ResolveCompletionItem => on_completion_resolve_handler,
        HoverRequest => on_hover,
        GluaHoverExpandRequest => on_hover_expand_handler,
        GotoDefinition => on_goto_definition_handler,
        GotoImplementation => on_implementation_handler,
        References => on_references_handler,
        Rename => on_rename_handler,
        PrepareRenameRequest => on_prepare_rename_handler,
        SignatureHelpRequest => on_signature_helper_handler,
        DocumentHighlightRequest => on_document_highlight_handler,
        DocumentSymbolRequest => on_document_symbol,
        WorkspaceSymbolRequest => on_workspace_symbol_handler,
        CodeActionRequest => on_code_action_handler,
        InlineValueRequest => on_inline_values_handler,
        DocumentColor => on_document_color,
        ColorPresentationRequest => on_document_color_presentation,
        DocumentLinkRequest => on_document_link_handler,
        DocumentLinkResolve => on_document_link_resolve_handler,
        CodeLensResolve => on_resolve_code_lens_handler,
        InlayHintResolveRequest => on_resolve_inlay_hint,
        EmmyGutterRequest => on_emmy_gutter_handler,
        EmmyGutterDetailRequest => on_emmy_gutter_detail_handler,
        CallHierarchyPrepare => on_prepare_call_hierarchy_handler,
        CallHierarchyIncomingCalls => on_incoming_calls_handler,
        CallHierarchyOutgoingCalls => on_outgoing_calls_handler,
        GmodScriptedClassesRequest => on_gmod_scripted_classes_handler,
        GmodScriptedClassesV2Request => on_gmod_scripted_classes_v2_handler,
        ExecuteCommand => on_execute_command_handler,
        WillRenameFiles => on_will_rename_files_handler,
    }, content_modified_if_client_retries: {
        SemanticTokensFullRequest => on_semantic_token_handler,
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::extract_uri_from_value;
    use glua_code_analysis::LuaDeclId;
    use googletest::prelude::*;
    use rowan::TextSize;
    use serde_json::json;
    use std::str::FromStr;

    use lsp_types::Uri;

    use crate::handlers::{
        code_lens::{CodeLensData, CodeLensResolveData},
        completion::{CompletionData, CompletionDataType},
    };

    #[gtest]
    fn fresh_index_requests_do_not_answer_until_analysis_settles() -> Result<()> {
        use super::{Completion, LspRequest, on_request_handler};
        use crate::context::ServerContext;
        use lsp_server::{Connection, Message};
        use lsp_types::ClientCapabilities;
        use std::time::Duration;

        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let (server_connection, peer) = Connection::memory();

        runtime.block_on(async {
            let mut context = ServerContext::new(server_connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            let debounced_analysis = snapshot.debounced_analysis_arc();

            // Mark analysis dirty exactly as a didChange does, before the
            // request arrives.
            let in_flight = debounced_analysis.begin_in_flight_change();

            let request = lsp_server::Request::new(
                1.into(),
                Completion::METHOD.to_string(),
                json!({
                    "textDocument": { "uri": "file:///test.lua" },
                    "position": { "line": 0, "character": 0 }
                }),
            );
            on_request_handler(request, &mut context)
                .await
                .expect("dispatch should succeed");

            // Wait for the condition, not for a deadline: once the handler is
            // inside the freshness wait it cannot leave while the change is
            // in flight, so an empty channel here is not a race.
            tokio::time::timeout(Duration::from_secs(5), async {
                while debounced_analysis.freshness_wait_count() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the handler should reach the freshness wait");
            verify_that!(peer.receiver.try_recv().is_err(), eq(true))?;

            // Settling the change releases the wait.
            in_flight.finish().await;

            let message = peer
                .receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("a response must arrive once analysis is fresh");
            verify_that!(matches!(message, Message::Response(_)), eq(true))?;
            Ok(())
        })
    }

    /// Only what the user waits on while typing ends an edit's debounce, and
    /// the policy names each by its real method.
    #[test]
    fn only_interactive_requests_expedite_the_debounce() {
        use super::{
            CodeLensRequest, Completion, DocumentSymbolRequest, EmmyAnnotatorRequest,
            EmmySyntaxTreeRequest, FoldingRangeRequest, Formatting, GluaHoverExpandRequest,
            GotoDefinition, HoverRequest, InlayHintRequest, LspRequest, OnTypeFormatting,
            RangeFormatting, ResolveCompletionItem, SelectionRangeRequest,
            SemanticTokensFullRequest, SignatureHelpRequest,
        };
        use crate::context::expedites_debounce;

        for method in [
            Completion::METHOD,
            ResolveCompletionItem::METHOD,
            HoverRequest::METHOD,
            GluaHoverExpandRequest::METHOD,
            SignatureHelpRequest::METHOD,
            GotoDefinition::METHOD,
            SemanticTokensFullRequest::METHOD,
            SelectionRangeRequest::METHOD,
            Formatting::METHOD,
            RangeFormatting::METHOD,
            OnTypeFormatting::METHOD,
        ] {
            assert!(expedites_debounce(method), "{method} should expedite");
        }
        for method in [
            FoldingRangeRequest::METHOD,
            EmmySyntaxTreeRequest::METHOD,
            DocumentSymbolRequest::METHOD,
            CodeLensRequest::METHOD,
            InlayHintRequest::METHOD,
            EmmyAnnotatorRequest::METHOD,
        ] {
            assert!(
                !expedites_debounce(method),
                "{method} should wait passively"
            );
        }
    }

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

    /// The custom document requests (annotator, gutter, syntax tree) carry
    /// their document as a top-level `uri`, which an edit to it must cancel.
    #[test]
    fn extracts_top_level_uri() {
        let uri = Uri::from_str("file:///annotated.lua").expect("uri should parse");
        let value = json!({ "uri": uri.clone() });

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
                color: None,
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
