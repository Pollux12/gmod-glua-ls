use lsp_server::Message;
use std::collections::HashSet;

use lsp_types::Uri;
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    Notification as LspNotification,
};
use lsp_types::{
    DidChangeTextDocumentParams, TextDocumentContentChangeEvent, VersionedTextDocumentIdentifier,
};

use crate::context::EditorDocuments;

pub(super) struct PendingInitQueue {
    messages: Vec<Message>,
    documents: EditorDocuments,
    fold_boundaries: HashSet<Uri>,
}

impl PendingInitQueue {
    pub(super) fn new() -> Self {
        Self {
            messages: Vec::new(),
            documents: EditorDocuments::default(),
            fold_boundaries: HashSet::new(),
        }
    }

    pub(super) fn queue(&mut self, msg: Message) {
        let Message::Notification(notification) = msg else {
            self.messages.push(msg);
            return;
        };
        match notification.method.as_str() {
            DidOpenTextDocument::METHOD => {
                let Ok(params) = serde_json::from_value::<lsp_types::DidOpenTextDocumentParams>(
                    notification.params.clone(),
                ) else {
                    self.messages.push(Message::Notification(notification));
                    return;
                };
                let uri = params.text_document.uri;
                let window = self.document_window_start();
                self.remove_document_entries(window, &uri, false);
                self.fold_boundaries.remove(&uri);
                self.documents.open(uri, params.text_document.text);
                self.messages.push(Message::Notification(notification));
            }
            DidChangeTextDocument::METHOD => {
                let Ok(params) = serde_json::from_value::<DidChangeTextDocumentParams>(
                    notification.params.clone(),
                ) else {
                    self.messages.push(Message::Notification(notification));
                    return;
                };
                let DidChangeTextDocumentParams {
                    text_document: VersionedTextDocumentIdentifier { uri, version },
                    content_changes,
                } = params;
                let window = self.document_window_start();
                match self.documents.change(&uri, content_changes) {
                    Ok(text) => {
                        let folded = folded_change(&uri, version, text);
                        let mut tail = self.messages.split_off(window);
                        let chain_broken = self.fold_boundaries.remove(&uri);
                        let folded_position = if chain_broken {
                            None
                        } else {
                            tail.iter().rposition(|msg| {
                                queued_doc(msg).is_some_and(|(method, doc_uri)| {
                                    method == DidChangeTextDocument::METHOD
                                        && doc_uri == uri.as_str()
                                })
                            })
                        };
                        match folded_position {
                            Some(position) => tail[position] = folded,
                            None => tail.push(folded),
                        }
                        self.messages.append(&mut tail);
                    }
                    Err(_) => {
                        self.fold_boundaries.insert(uri);
                        self.messages.push(Message::Notification(notification));
                    }
                }
            }
            DidCloseTextDocument::METHOD => {
                let Ok(params) = serde_json::from_value::<lsp_types::DidCloseTextDocumentParams>(
                    notification.params.clone(),
                ) else {
                    self.messages.push(Message::Notification(notification));
                    return;
                };
                let uri = params.text_document.uri;
                let window = self.document_window_start();
                self.remove_document_entries(window, &uri, true);
                self.fold_boundaries.remove(&uri);
                self.documents.close(&uri);
                self.messages.push(Message::Notification(notification));
            }
            DidSaveTextDocument::METHOD => {
                let Ok(params) = serde_json::from_value::<lsp_types::DidSaveTextDocumentParams>(
                    notification.params.clone(),
                ) else {
                    self.messages.push(Message::Notification(notification));
                    return;
                };
                let uri = params.text_document.uri;
                let window = self.document_window_start();
                let mut tail = self.messages.split_off(window);
                let last_save = tail.iter().rposition(|msg| {
                    matches!(
                        queued_doc(msg),
                        Some((DidSaveTextDocument::METHOD, doc_uri)) if doc_uri == uri.as_str()
                    )
                });
                if let Some(position) = last_save {
                    tail.remove(position);
                }
                tail.push(Message::Notification(notification));
                self.messages.append(&mut tail);
            }
            _ => self.messages.push(Message::Notification(notification)),
        }
    }

    pub(super) fn take(&mut self) -> Vec<Message> {
        self.documents = EditorDocuments::default();
        self.fold_boundaries.clear();
        std::mem::take(&mut self.messages)
    }

    fn document_window_start(&self) -> usize {
        let mut start = self.messages.len();
        while start > 0 && queued_doc(&self.messages[start - 1]).is_some() {
            start -= 1;
        }
        start
    }

    fn remove_document_entries(&mut self, window: usize, uri: &Uri, include_closes: bool) {
        let uri = uri.as_str();
        let tail = self.messages.split_off(window);
        self.messages.extend(tail.into_iter().filter(|msg| {
            queued_doc(msg).is_some_and(|(method, doc_uri)| {
                doc_uri != uri
                    || match method {
                        DidOpenTextDocument::METHOD | DidChangeTextDocument::METHOD => false,
                        DidCloseTextDocument::METHOD => !include_closes,
                        _ => true,
                    }
            })
        }));
    }
}

fn folded_change(uri: &Uri, version: i32, text: String) -> Message {
    let params = DidChangeTextDocumentParams {
        text_document: VersionedTextDocumentIdentifier {
            uri: uri.clone(),
            version,
        },
        content_changes: vec![TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text,
        }],
    };
    Message::Notification(lsp_server::Notification::new(
        DidChangeTextDocument::METHOD.to_string(),
        params,
    ))
}

fn queued_doc(msg: &Message) -> Option<(&'static str, &str)> {
    let Message::Notification(notification) = msg else {
        return None;
    };
    let method = match notification.method.as_str() {
        DidOpenTextDocument::METHOD => DidOpenTextDocument::METHOD,
        DidChangeTextDocument::METHOD => DidChangeTextDocument::METHOD,
        DidCloseTextDocument::METHOD => DidCloseTextDocument::METHOD,
        DidSaveTextDocument::METHOD => DidSaveTextDocument::METHOD,
        _ => return None,
    };
    let uri = notification
        .params
        .get("textDocument")?
        .get("uri")?
        .as_str()?;
    Some((method, uri))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use lsp_server::{Notification, Request, RequestId};
    use lsp_types::notification::Notification as LspNotification;
    use lsp_types::{
        DidChangeTextDocumentParams, DidOpenTextDocumentParams, Position, Range,
        TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
        VersionedTextDocumentIdentifier,
    };

    use super::PendingInitQueue;

    fn uri() -> lsp_types::Uri {
        lsp_types::Uri::from_str("file:///workspace/lua/autorun/init.lua").unwrap()
    }

    fn open(version: i32, text: &str) -> lsp_server::Message {
        lsp_server::Message::Notification(Notification::new(
            lsp_types::notification::DidOpenTextDocument::METHOD.to_string(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri(),
                    language_id: "lua".to_string(),
                    version,
                    text: text.to_string(),
                },
            },
        ))
    }

    fn change(
        version: i32,
        range: Option<(u32, u32, u32, u32)>,
        text: &str,
    ) -> lsp_server::Message {
        lsp_server::Message::Notification(Notification::new(
            lsp_types::notification::DidChangeTextDocument::METHOD.to_string(),
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri(),
                    version,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: range.map(|(sl, sc, el, ec)| {
                        Range::new(Position::new(sl, sc), Position::new(el, ec))
                    }),
                    range_length: None,
                    text: text.to_string(),
                }],
            },
        ))
    }

    fn close() -> lsp_server::Message {
        lsp_server::Message::Notification(Notification::new(
            lsp_types::notification::DidCloseTextDocument::METHOD.to_string(),
            lsp_types::DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri() },
            },
        ))
    }

    fn save(text: Option<&str>) -> lsp_server::Message {
        lsp_server::Message::Notification(Notification::new(
            lsp_types::notification::DidSaveTextDocument::METHOD.to_string(),
            lsp_types::DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri() },
                text: text.map(str::to_string),
            },
        ))
    }

    fn request(id: i32) -> lsp_server::Message {
        lsp_server::Message::Request(Request::new(
            RequestId::from(id),
            "textDocument/definition".to_string(),
            serde_json::Value::Null,
        ))
    }

    fn other_notification() -> lsp_server::Message {
        lsp_server::Message::Notification(Notification::new(
            "$/setTrace".to_string(),
            serde_json::Value::Null,
        ))
    }

    fn method_of(msg: &lsp_server::Message) -> String {
        let lsp_server::Message::Notification(notification) = msg else {
            panic!("expected a notification, got {msg:?}");
        };
        notification.method.clone()
    }

    #[test]
    fn a_change_burst_folds_into_one_full_text_change() {
        let mut queue = PendingInitQueue::new();
        queue.queue(open(1, "local a = 1\nlocal b = 2\n"));
        queue.queue(change(2, Some((0, 7, 0, 8)), "x\n"));
        queue.queue(change(3, Some((1, 0, 1, 0)), "-- edited\n"));
        queue.queue(change(4, None, "local a = 11\n"));

        let messages = queue.take();
        assert_eq!(messages.len(), 2, "didOpen plus one folded change");
        assert_eq!(method_of(&messages[0]), "textDocument/didOpen");
        assert_eq!(
            queued_changes_of(&messages),
            vec![(4, "local a = 11\n".to_string())],
            "the folded change carries the burst's final text at its latest version"
        );
    }

    #[test]
    fn folded_changes_preserve_incremental_edits() {
        let mut queue = PendingInitQueue::new();
        queue.queue(open(1, "local a = 1\n"));
        queue.queue(change(2, Some((0, 6, 0, 7)), "b"));
        queue.queue(change(3, Some((0, 10, 0, 11)), "22"));

        let messages = queue.take();
        assert_eq!(
            queued_changes_of(&messages),
            vec![(3, "local b = 22\n".to_string())],
            "each incremental edit applies to the text the one before it produced"
        );
    }

    #[test]
    fn a_close_supersedes_queued_edits() {
        let mut queue = PendingInitQueue::new();
        queue.queue(open(1, "local a = 1\n"));
        queue.queue(change(2, Some((0, 6, 0, 7)), "b"));
        queue.queue(save(None));
        queue.queue(close());

        let messages = queue.take();
        assert_eq!(
            methods_of(&messages),
            vec![
                "textDocument/didSave".to_string(),
                "textDocument/didClose".to_string()
            ],
            "the open and the folded change are superseded; the save's recheck is not"
        );
    }

    #[test]
    fn a_reopen_replaces_prior_queued_edits() {
        let mut queue = PendingInitQueue::new();
        queue.queue(open(1, "local a = 1\n"));
        queue.queue(change(2, Some((0, 6, 0, 7)), "b"));
        queue.queue(open(1, "local reopened = true\n"));

        let messages = queue.take();
        assert_eq!(
            methods_of(&messages),
            vec!["textDocument/didOpen".to_string()],
            "only the reopening didOpen remains"
        );

        queue.queue(open(1, "local reopened = true\n"));
        queue.queue(change(2, Some((0, 6, 0, 7)), "b"));
        assert_eq!(
            queued_changes_of(&queue.take()),
            vec![(2, "local beopened = true\n".to_string())]
        );
    }

    #[test]
    fn a_run_of_saves_keeps_only_the_last() {
        let mut queue = PendingInitQueue::new();
        queue.queue(save(Some("local a = 1\n")));
        queue.queue(save(None));

        let messages = queue.take();
        assert_eq!(
            methods_of(&messages),
            vec!["textDocument/didSave".to_string()],
            "the earlier save is superseded by the later one"
        );
    }

    #[test]
    fn a_request_pins_the_edits_queued_before_it() {
        let mut first = PendingInitQueue::new();
        first.queue(open(1, "local a = 1\n"));
        first.queue(change(2, Some((0, 6, 0, 7)), "b"));
        first.queue(request(1));
        first.queue(change(3, Some((0, 10, 0, 11)), "2"));
        first.queue(request(2));
        first.queue(change(4, Some((0, 12, 0, 13)), "3"));

        let messages = first.take();
        assert_eq!(
            methods_of(&messages),
            vec![
                "textDocument/didOpen".to_string(),
                "textDocument/didChange".to_string(),
                "textDocument/definition".to_string(),
                "textDocument/didChange".to_string(),
                "textDocument/definition".to_string(),
                "textDocument/didChange".to_string(),
            ]
        );
        assert_eq!(
            queued_changes_of(&messages),
            vec![
                (2, "local b = 1\n".to_string()),
                (3, "local b = 2\n".to_string()),
                (4, "local b = 23\n".to_string()),
            ],
            "each request keeps the change it was made against, and the tail folds"
        );
    }

    #[test]
    fn a_non_document_notification_pins_the_run_before_it() {
        let mut queue = PendingInitQueue::new();
        queue.queue(open(1, "local a = 1\n"));
        queue.queue(change(2, Some((0, 6, 0, 7)), "b"));
        queue.queue(other_notification());
        queue.queue(change(3, Some((0, 10, 0, 11)), "2"));

        let messages = queue.take();
        assert_eq!(
            queued_changes_of(&messages),
            vec![
                (2, "local b = 1\n".to_string()),
                (3, "local b = 2\n".to_string()),
            ],
            "the change before the notification stays as sent; the tail folds"
        );
    }

    #[test]
    fn an_unfoldable_change_is_kept_as_sent() {
        let mut queue = PendingInitQueue::new();
        queue.queue(change(2, Some((0, 6, 0, 7)), "b"));

        let messages = queue.take();
        assert_eq!(
            queued_changes_of(&messages),
            vec![(2, "b".to_string())],
            "a change with nothing to fold against keeps its own text"
        );
    }

    #[test]
    fn a_full_text_resync_after_a_rejected_change_replays_in_sync() {
        let mut queue = PendingInitQueue::new();
        queue.queue(open(1, "local a = 1\n"));
        queue.queue(change(2, Some((0, 6, 0, 7)), "b"));
        queue.queue(change(3, Some((5, 0, 5, 0)), "x"));
        queue.queue(change(4, None, "local fixed\n"));

        let messages = queue.take();
        assert_eq!(
            methods_of(&messages),
            vec![
                "textDocument/didOpen".to_string(),
                "textDocument/didChange".to_string(),
                "textDocument/didChange".to_string(),
                "textDocument/didChange".to_string(),
            ],
            "the rejected change stays queued, with the recovered fold after it"
        );

        let mut replayed = crate::context::EditorDocuments::default();
        replayed.open(uri(), "local a = 1\n".to_string());
        for msg in &messages {
            let lsp_server::Message::Notification(notification) = msg else {
                continue;
            };
            if notification.method == lsp_types::notification::DidChangeTextDocument::METHOD {
                let params: DidChangeTextDocumentParams =
                    serde_json::from_value(notification.params.clone()).unwrap();
                let _ = replayed.change(&uri(), params.content_changes);
            }
        }

        assert_eq!(
            replayed.change(&uri(), Vec::new()),
            Ok("local fixed\n".to_string()),
            "the replayed queue must resync the document, not end out of sync"
        );
    }

    #[test]
    fn open_close_cycles_keep_the_window_bounded() {
        let mut queue = PendingInitQueue::new();
        for _ in 0..3 {
            queue.queue(open(1, "local a = 1\n"));
            queue.queue(close());
        }

        let messages = queue.take();
        assert_eq!(
            methods_of(&messages),
            vec!["textDocument/didClose".to_string()],
            "within the window a new close supersedes the opens and closes queued before it"
        );
    }

    #[test]
    fn saves_do_not_accumulate_across_open_close_cycles() {
        let mut queue = PendingInitQueue::new();
        for cycle in 0..2 {
            let version = cycle * 2;
            queue.queue(open(version + 1, "local a = 1\n"));
            queue.queue(change(version + 2, Some((0, 6, 0, 7)), "b"));
            queue.queue(save(None));
            queue.queue(close());
        }

        let messages = queue.take();
        assert_eq!(
            methods_of(&messages),
            vec![
                "textDocument/didSave".to_string(),
                "textDocument/didClose".to_string()
            ],
            "each cycle's close supersedes its edits, and its save is superseded by the next save"
        );
    }

    fn methods_of(messages: &[lsp_server::Message]) -> Vec<String> {
        messages
            .iter()
            .map(|msg| match msg {
                lsp_server::Message::Notification(notification) => notification.method.clone(),
                lsp_server::Message::Request(request) => request.method.clone(),
                lsp_server::Message::Response(response) => {
                    format!("response:{:?}", response.id)
                }
            })
            .collect()
    }

    fn queued_changes_of(messages: &[lsp_server::Message]) -> Vec<(i32, String)> {
        messages
            .iter()
            .filter_map(|msg| {
                let lsp_server::Message::Notification(notification) = msg else {
                    return None;
                };
                (notification.method == lsp_types::notification::DidChangeTextDocument::METHOD)
                    .then(|| {
                        let params: DidChangeTextDocumentParams =
                            serde_json::from_value(notification.params.clone()).unwrap();
                        let text = params
                            .content_changes
                            .into_iter()
                            .map(|change| change.text)
                            .collect::<Vec<_>>()
                            .join("");
                        (params.text_document.version, text)
                    })
            })
            .collect()
    }
}
