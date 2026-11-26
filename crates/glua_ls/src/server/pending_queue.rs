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

/// Messages that arrived while the workspace was still initializing, held
/// until initialization completes.
///
/// Editor notification bursts are coalesced per document so the queue stays
/// bounded: with incremental sync a keystroke burst is one `didChange` per
/// keystroke, and folding each document's queued changes into one full-text
/// change keeps the queue at a few entries per open document instead of one
/// entry per keystroke. A `didClose` supersedes the `didOpen` and
/// `didChange` notifications queued before it for the same document - and,
/// within the rewrite window, the closes before it, so an open/close
/// churn loop cannot grow the queue either - and a run of `didSave`
/// notifications keeps only the last.
///
/// Only the trailing run of document notifications is ever rewritten: every
/// message before the last queued request stays exactly as the client sent
/// it, so a request deferred behind initialization still observes the
/// document state it was made against.
pub(super) struct PendingInitQueue {
    messages: Vec<Message>,
    /// The folded text of each document the queue holds notifications for:
    /// the `didOpen` text with every queued `didChange` applied, the same
    /// way the replayed notifications will update the editor's documents.
    documents: EditorDocuments,
    /// Documents whose queued `didChange` notifications include one the
    /// fold could not take. That raw notification stays in the queue, so
    /// the next successful fold starts a new chain appended after it
    /// instead of rewriting the change queued before the failure: replay
    /// must apply the rejected change before the full-text change that
    /// recovered from it, or it ends out of sync again.
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

    /// Defers `msg` until initialization completes, coalescing document
    /// notification bursts per document.
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
                        // A change that could not fold broke this document's
                        // chain: the raw notification stayed in the queue,
                        // so this fold starts a new chain appended after it
                        // rather than rewriting the one before the failure.
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
                    // Nothing to fold against: no `didOpen` was ever queued,
                    // or a change the editor text could not take. Keep the
                    // notification as the client sent it, and mark the
                    // chain broken for the next change that folds.
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
                let last_save = tail
                    .iter()
                    .rposition(|msg| {
                        queued_doc(msg).is_some_and(|(_, doc_uri)| doc_uri == uri.as_str())
                    })
                    .filter(|position| {
                        matches!(
                            queued_doc(&tail[*position]),
                            Some((DidSaveTextDocument::METHOD, _))
                        )
                    });
                match last_save {
                    // A run of saves keeps only the last one: a save carries
                    // no edits, and the workspace recheck it schedules is
                    // idempotent while it is still delayed.
                    Some(position) => tail[position] = Message::Notification(notification),
                    None => tail.push(Message::Notification(notification)),
                }
                self.messages.append(&mut tail);
            }
            _ => self.messages.push(Message::Notification(notification)),
        }
    }

    /// Takes every queued message, resetting the per-document fold state.
    pub(super) fn take(&mut self) -> Vec<Message> {
        self.documents = EditorDocuments::default();
        self.fold_boundaries.clear();
        std::mem::take(&mut self.messages)
    }

    /// Where the trailing run of queued document notifications starts: only
    /// this run may be rewritten, since no queued request observes it.
    fn document_window_start(&self) -> usize {
        let mut start = self.messages.len();
        while start > 0 && queued_doc(&self.messages[start - 1]).is_some() {
            start -= 1;
        }
        start
    }

    /// Removes the window's queued document notifications for `uri`.
    ///
    /// `didOpen` and `didChange` entries always go: a close or a reopen
    /// supersedes them. A `didClose` before them stays for a reopen - it
    /// still runs for its own side effects - but a later close drops it,
    /// so an open/close churn loop cannot grow the window; a `didSave`'s
    /// workspace recheck is superseded by neither.
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

/// One full-text `didChange` standing in for every queued change to `uri`:
/// replaying it applies the same text the queued incremental changes would
/// have, without depending on the ranges of the edits before it.
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

/// The method and URI of a queued document notification, without
/// deserializing its params: a folded `didChange` holds the document's full
/// text, which no scan needs to copy.
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

    /// One incremental change replacing `range` with `text`, as an editor
    /// sends with `TextDocumentSyncKind::INCREMENTAL`.
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

    /// A burst of incremental changes during initialization folds into one
    /// full-text change carrying the text the whole burst produced.
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

    /// Incremental changes keep applying to the text of the edits before
    /// them, exactly as the editor applied them.
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

    /// A didClose supersedes the didOpen and didChange notifications queued
    /// before it for the same document.
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

    /// A didOpen replaces the document's prior queued edits, as a reopen
    /// restarts the document's state.
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

        // The new session's edits fold from the new open text.
        queue.queue(open(1, "local reopened = true\n"));
        queue.queue(change(2, Some((0, 6, 0, 7)), "b"));
        assert_eq!(
            queued_changes_of(&queue.take()),
            vec![(2, "local beopened = true\n".to_string())]
        );
    }

    /// A run of saves keeps only the last one.
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

    /// Every message before the last queued request stays exactly as the
    /// client sent it, so the request observes the state it was made
    /// against; only the trailing run is rewritten.
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

    /// A notification that is not a document notification ends the
    /// rewriteable run.
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

    /// A didChange the editor text cannot take (no didOpen was queued for
    /// the document) is kept exactly as the client sent it.
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

    /// A full-text change recovers a document a rejected change left out of
    /// sync: the recovered fold starts a new chain after the raw change, so
    /// replay applies the rejection and then the resync, ending in sync with
    /// the editor's text.
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

        // Replay as the real handlers do: each didChange applies to the
        // editor's documents, a rejected one is reported and skipped.
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

        // An empty change returns the current text, and fails when the
        // document is out of sync: the replay must end in sync, at the
        // resyncing change's text.
        assert_eq!(
            replayed.change(&uri(), Vec::new()),
            Ok("local fixed\n".to_string()),
            "the replayed queue must resync the document, not end out of sync"
        );
    }

    /// An open/close churn loop for one document leaves the rewrite window
    /// at one close (and at most one open), not one entry per cycle.
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
