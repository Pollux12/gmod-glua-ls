//! The server process driven over stdio as an editor drives it: it exits
//! after `shutdown` and `exit`, whether its workspace diagnostics have
//! settled or are still being computed, and it analyses an edit on the same
//! stack depth as the workspace load and `glua_check`.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lsp_server::{Message, Notification, Request, RequestId, Response};
use serde_json::{Value, json};

/// Long enough for a debug build to load a small workspace.
const STARTUP: Duration = Duration::from_secs(60);

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: mpsc::Receiver<Message>,
    next_id: i32,
    /// Whether the server reported its workspace loaded yet.
    loaded: bool,
}

impl Server {
    /// Starts the server on `workspace` with one diagnostic worker.
    fn start(workspace: &Path, annotations: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_glua_ls"))
            .args(["--communication", "stdio", "--gmod-annotations-path"])
            .arg(annotations)
            .env("GLUALS_WORKSPACE_DIAGNOSTIC_PARALLELISM", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the server starts");
        let stdout = child.stdout.take().expect("the server's stdout is piped");
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(message)) = Message::read(&mut reader) {
                if sender.send(message).is_err() {
                    return;
                }
            }
        });
        let stdin = child.stdin.take();
        let mut server = Self {
            child,
            stdin,
            messages,
            next_id: 0,
            loaded: false,
        };
        let root =
            glua_code_analysis::file_path_to_uri(&workspace.to_path_buf()).expect("workspace URI");
        server.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root,
                "workspaceFolders": [{ "uri": root, "name": "workspace" }],
                "capabilities": {},
            }),
        );
        server.notify("initialized", json!({}));
        server
    }

    fn send(&mut self, message: Message) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        message.write(stdin).expect("the message is sent");
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(Message::Notification(Notification::new(
            method.to_string(),
            params,
        )));
    }

    fn request(&mut self, method: &str, params: Value) -> Response {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        self.send(Message::Request(Request::new(
            id.clone(),
            method.to_string(),
            params,
        )));
        self.receive_until(STARTUP, |message| match message {
            Message::Response(response) if response.id == id => Some(response.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no response to {method}"))
    }

    fn wait_for_status(&mut self, state: &str) {
        self.receive_until(STARTUP, |message| match message {
            Message::Notification(notification)
                if notification.method == "gluals/serverStatus"
                    && notification.params["state"] == state =>
            {
                Some(())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("the server never reported {state}"));
    }

    /// Reads messages until `found` picks one, answering the server's own
    /// requests as an editor would.
    fn receive_until<T>(
        &mut self,
        timeout: Duration,
        mut found: impl FnMut(&Message) -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            let message = self.messages.recv_timeout(left).ok()?;
            if let Message::Notification(notification) = &message
                && notification.method == "gluals/serverStatus"
                && notification.params["state"] == "workspaceLoaded"
            {
                self.loaded = true;
            }
            if let Some(value) = found(&message) {
                return Some(value);
            }
            if let Message::Request(request) = message {
                let result = match request.method.as_str() {
                    "workspace/configuration" => {
                        let items = request.params["items"].as_array().map_or(0, Vec::len);
                        Value::Array(vec![Value::Null; items])
                    }
                    _ => Value::Null,
                };
                self.send(Message::Response(Response::new_ok(request.id, result)));
            }
        }
    }

    /// Sends `shutdown` and `exit`, closes stdin, and reports whether the
    /// process then exits within `timeout`.
    fn shuts_down_within(mut self, timeout: Duration) -> bool {
        self.request("shutdown", Value::Null);
        self.exits_within(timeout)
    }

    /// Sends `exit`, closes stdin, and reports whether the process then exits
    /// within `timeout`.
    fn exits_within(mut self, timeout: Duration) -> bool {
        self.notify("exit", Value::Null);
        drop(self.stdin.take());
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A workspace of `files` files that each have diagnostics to compute, the
/// annotations folder to start with, and the folder holding both.
fn workspace(name: &str, files: usize) -> (PathBuf, PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
    let workspace = root.join("workspace");
    let autorun = workspace.join("lua/autorun");
    let annotations = root.join("annotations");
    std::fs::create_dir_all(&autorun).expect("the workspace is created");
    std::fs::create_dir_all(&annotations).expect("the annotations folder is created");
    std::fs::write(annotations.join("meta.lua"), "---@meta\n").expect("annotations are written");
    for index in 0..files {
        std::fs::write(
            autorun.join(format!("sh_busy_{index}.lua")),
            format!("local unused_{index} = MissingGlobal_{index}\n").repeat(200),
        )
        .expect("a workspace file is written");
    }
    (root, workspace, annotations)
}

#[test]
fn a_settled_server_exits_after_shutdown() {
    let (root, workspace, annotations) = workspace("gmod_glua_ls_exit_settled", 4);
    let mut server = Server::start(&workspace, &annotations);
    server.wait_for_status("startupComplete");

    let exited = server.shuts_down_within(Duration::from_secs(10));
    let _ = std::fs::remove_dir_all(root);
    assert!(exited, "the server did not exit");
}

/// Shut down as soon as the client initialized, while the workspace is still
/// loading: `shutdown` is answered before the load finishes, and the process
/// ends at `exit` though the load is still running.
#[test]
fn a_server_still_loading_answers_shutdown_and_exits() {
    let (root, workspace, annotations) = workspace("gmod_glua_ls_exit_loading", 400);
    let mut server = Server::start(&workspace, &annotations);

    let answered = server.request("shutdown", Value::Null);
    let loaded_first = server.loaded;
    let exited = server.exits_within(Duration::from_secs(10));
    let _ = std::fs::remove_dir_all(root);
    assert!(answered.error.is_none(), "{answered:?}");
    assert!(!loaded_first, "shutdown waited for the workspace to load");
    assert!(exited, "the server did not exit");
}

/// Shut down while the workspace diagnostics still have files left and an
/// edit is settling.
#[test]
fn a_server_still_diagnosing_exits_after_shutdown() {
    let (root, workspace, annotations) = workspace("gmod_glua_ls_exit_busy", 60);
    let mut server = Server::start(&workspace, &annotations);
    server.wait_for_status("workspaceLoaded");
    let uri = glua_code_analysis::file_path_to_uri(&workspace.join("lua/autorun/sh_busy_0.lua"))
        .expect("document URI");
    server.notify(
        "textDocument/didOpen",
        json!({ "textDocument": { "uri": uri, "languageId": "lua", "version": 1, "text": "local a = 1\n" } }),
    );
    server.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "local other = Missing\n".repeat(20000) }],
        }),
    );

    let exited = server.shuts_down_within(Duration::from_secs(10));
    let _ = std::fs::remove_dir_all(root);
    assert!(exited, "the server did not exit");
}

/// An edit is parsed on a runtime thread, which nests as deep as the analysis
/// pool that loaded the workspace: text the load parses cleanly stays clean.
#[test]
fn an_edit_nests_as_deep_as_the_workspace_load() {
    let (root, workspace, annotations) = workspace("gmod_glua_ls_deep_edit", 1);
    let mut server = Server::start(&workspace, &annotations);
    server.wait_for_status("workspaceLoaded");
    let uri = glua_code_analysis::file_path_to_uri(&workspace.join("lua/autorun/sh_busy_0.lua"))
        .expect("document URI");
    let depth = 20_000;
    let deep = format!("{}x = 1{}\n", "do ".repeat(depth), " end".repeat(depth));
    server.notify(
        "textDocument/didOpen",
        json!({ "textDocument": { "uri": uri, "languageId": "lua", "version": 1, "text": "local a = 1\n" } }),
    );
    server.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": deep }],
        }),
    );

    let messages = server.receive_until(STARTUP, |message| match message {
        Message::Notification(notification)
            if notification.method == "textDocument/publishDiagnostics"
                && notification.params["uri"] == json!(uri)
                && notification.params["version"] == 2 =>
        {
            Some(
                notification.params["diagnostics"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|diagnostic| diagnostic["message"].as_str().map(str::to_string))
                    .collect::<Vec<_>>(),
            )
        }
        _ => None,
    });
    drop(server);
    let _ = std::fs::remove_dir_all(root);
    let messages = messages.expect("the edit's diagnostics are published");
    let too_deep = messages
        .iter()
        .filter(|message| message.contains("too deeply nested"))
        .count();
    assert_eq!(
        too_deep,
        0,
        "first diagnostics: {:?}",
        &messages[..messages.len().min(3)]
    );
}
