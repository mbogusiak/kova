//! KOVA-036 integration tests: a mock `jdt.ls` on `tokio::io::duplex` drives the full
//! JSON-RPC session, the `JdtlsClient` lifecycle and `DocumentSyncTracker`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "integration tests"
)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use kova_buffer::{Buffer, BufferId, EditLogConfig, TextEdit};
use kova_lsp_dap::LspDapError;
use kova_lsp_dap::lsp::{
    ClientRuntimeConfig, DocumentSyncTracker, JdtlsClient, LspEvent, LspLifecycleState, LspSession,
    SyncModeApplied, path_to_lsp_uri, read_lsp_frame, write_lsp_frame,
};
use serde_json::{Value, json};
use tokio::io::{BufReader, DuplexStream, ReadHalf, WriteHalf};

const STEP: Duration = Duration::from_secs(5);

/// Server side of a duplex pipe speaking framed JSON-RPC.
struct MockServer {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
}

impl MockServer {
    async fn recv(&mut self) -> Value {
        let frame = tokio::time::timeout(STEP, read_lsp_frame(&mut self.reader))
            .await
            .expect("mock server timed out waiting for a frame")
            .expect("valid frame")
            .expect("stream open");
        serde_json::from_slice(&frame).expect("valid JSON")
    }

    async fn send(&mut self, message: Value) {
        let bytes = serde_json::to_vec(&message).unwrap();
        write_lsp_frame(&mut self.writer, &bytes).await.unwrap();
    }

    async fn respond(&mut self, id: &Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
            .await;
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await;
    }

    /// Sends a server request and returns the client's response.
    async fn server_request(&mut self, id: Value, method: &str, params: Value) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        let response = self.recv().await;
        assert_eq!(response["id"], id, "response to {method}");
        response
    }
}

fn pipes() -> (
    (ReadHalf<DuplexStream>, WriteHalf<DuplexStream>),
    MockServer,
) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server);
    (
        tokio::io::split(client),
        MockServer {
            reader: BufReader::new(server_read),
            writer: server_write,
        },
    )
}

fn project_root() -> PathBuf {
    std::env::temp_dir().join("kova-036 projekt")
}

fn runtime_config() -> ClientRuntimeConfig {
    let mut config = ClientRuntimeConfig::new(project_root());
    config.initialization_options = json!({ "bundles": [] });
    config.workspace_settings = json!({ "java": { "home": "/opt/jdk-21" } });
    config.initialize_timeout = STEP;
    config.request_timeout = STEP;
    config
}

/// Answers `initialize` (with a `workspace/configuration` round-trip in the middle) and
/// consumes `initialized`.
async fn serve_handshake(server: &mut MockServer) {
    let init = server.recv().await;
    assert_eq!(init["method"], "initialize");
    assert_eq!(
        init["params"]["rootUri"],
        path_to_lsp_uri(&project_root()).unwrap().as_str()
    );
    assert_eq!(
        init["params"]["initializationOptions"],
        json!({ "bundles": [] })
    );
    assert_eq!(
        init["params"]["capabilities"]["workspace"]["applyEdit"],
        json!(true)
    );

    // jdt.ls asks for configuration before answering `initialize`: must not deadlock.
    let config = server
        .server_request(
            json!("cfg-1"),
            "workspace/configuration",
            json!({ "items": [{ "section": "java.home" }, { "section": "unknown.section" }] }),
        )
        .await;
    assert_eq!(config["result"], json!(["/opt/jdk-21", null]));

    server
        .respond(
            &init["id"],
            json!({ "capabilities": { "textDocumentSync": 2, "hoverProvider": true } }),
        )
        .await;
    let initialized = server.recv().await;
    assert_eq!(initialized["method"], "initialized");
}

async fn connect() -> (JdtlsClient, MockServer) {
    let ((reader, writer), mut server) = pipes();
    let (client, ()) = tokio::join!(
        JdtlsClient::start_with_streams(reader, writer, runtime_config()),
        serve_handshake(&mut server)
    );
    (client.unwrap(), server)
}

#[tokio::test]
async fn handshake_language_status_and_server_requests() {
    let (client, mut server) = connect().await;
    assert_eq!(client.state(), LspLifecycleState::IndexingProject);
    assert!(
        client
            .capabilities()
            .is_some_and(|c| c.hover_provider.is_some())
    );
    assert_eq!(client.child_pid(), None);

    let mut events = client.subscribe_events();

    let reg = server
        .server_request(
            json!(10),
            "client/registerCapability",
            json!({ "registrations": [{ "id": "watch-1", "method": "workspace/didChangeWatchedFiles" }] }),
        )
        .await;
    assert_eq!(reg["result"], Value::Null);
    let progress = server
        .server_request(
            json!(11),
            "window/workDoneProgress/create",
            json!({ "token": "import-1" }),
        )
        .await;
    assert_eq!(progress["result"], Value::Null);
    let apply = server
        .server_request(json!(12), "workspace/applyEdit", json!({ "edit": {} }))
        .await;
    assert_eq!(apply["result"]["applied"], json!(false));
    let unknown = server
        .server_request(json!(13), "x/unknown", Value::Null)
        .await;
    assert_eq!(unknown["error"]["code"], json!(-32601));

    assert_eq!(
        client.server_request_handler().registrations(),
        vec![(
            "watch-1".to_owned(),
            "workspace/didChangeWatchedFiles".to_owned()
        )]
    );
    assert_eq!(
        client.server_request_handler().progress_tokens(),
        vec!["import-1".to_owned()]
    );

    // Not ready yet.
    assert!(matches!(
        client
            .wait_until_service_ready(Duration::from_millis(30))
            .await,
        Err(LspDapError::RequestTimeout { .. })
    ));

    server
        .notify(
            "$/progress",
            json!({ "token": "import-1", "value": { "kind": "begin", "title": "Importing", "percentage": 10 } }),
        )
        .await;
    server
        .notify(
            "textDocument/publishDiagnostics",
            json!({ "uri": "file:///a/B.java", "version": 3, "diagnostics": [] }),
        )
        .await;
    server
        .notify(
            "language/status",
            json!({ "type": "Starting", "message": "Init..." }),
        )
        .await;
    server
        .notify(
            "language/status",
            json!({ "type": "ServiceReady", "message": "ServiceReady" }),
        )
        .await;

    client.wait_until_service_ready(STEP).await.unwrap();
    assert_eq!(client.state(), LspLifecycleState::Ready);
    // Already ready: returns immediately (no race with the notification).
    client
        .wait_until_service_ready(Duration::from_millis(1))
        .await
        .unwrap();

    let mut seen = Vec::new();
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(200), events.recv()).await
    {
        seen.push(event);
    }
    assert!(seen.contains(&LspEvent::WorkDoneProgress {
        token: "import-1".to_owned(),
        title: Some("Importing".to_owned()),
        message: None,
        percentage: Some(10),
        finished: false,
    }));
    assert!(seen.contains(&LspEvent::DiagnosticsReceived {
        uri: "file:///a/B.java".to_owned(),
        version: Some(3),
        diagnostics_json: json!([]),
    }));
    assert!(seen.contains(&LspEvent::StateChanged {
        previous: LspLifecycleState::IndexingProject,
        current: LspLifecycleState::Ready,
    }));
    assert!(seen.contains(&LspEvent::LanguageStatus {
        status_type: "ServiceReady".to_owned(),
        message: "ServiceReady".to_owned(),
    }));

    let (result, ()) = tokio::join!(client.shutdown_graceful(STEP), async {
        let shutdown = server.recv().await;
        assert_eq!(shutdown["method"], "shutdown");
        assert!(shutdown.get("params").is_none());
        server.respond(&shutdown["id"], Value::Null).await;
        let exit = server.recv().await;
        assert_eq!(exit["method"], "exit");
    });
    result.unwrap();
    assert_eq!(client.state(), LspLifecycleState::Stopped);
}

#[tokio::test]
async fn language_status_error_and_transport_close_fail_the_session() {
    let (client, mut server) = connect().await;
    server
        .notify(
            "language/status",
            json!({ "type": "Error", "message": "Import failed" }),
        )
        .await;
    let err = client.wait_until_service_ready(STEP).await;
    assert!(
        matches!(err, Err(LspDapError::InvalidState { .. })),
        "{err:?}"
    );
    assert_eq!(
        client.state(),
        LspLifecycleState::Failed("Import failed".to_owned())
    );

    let (client, server) = connect().await;
    drop(server);
    let err = client.wait_until_service_ready(STEP).await;
    assert!(
        matches!(err, Err(LspDapError::InvalidState { .. })),
        "{err:?}"
    );
    assert!(matches!(client.state(), LspLifecycleState::Failed(_)));
    assert!(matches!(
        client.request::<_, Value>("x", Value::Null).await,
        Err(LspDapError::TransportClosed)
    ));
}

#[tokio::test]
async fn failed_initialize_reports_error() {
    let ((reader, writer), mut server) = pipes();
    let (client, ()) = tokio::join!(
        JdtlsClient::start_with_streams(reader, writer, runtime_config()),
        async {
            let init = server.recv().await;
            server
                .send(json!({ "jsonrpc": "2.0", "id": init["id"], "error": { "code": -32603, "message": "no JDK" } }))
                .await;
        }
    );
    assert_eq!(
        client.err(),
        Some(LspDapError::ProtocolError {
            code: -32603,
            message: "no JDK".to_owned()
        })
    );
}

#[tokio::test]
async fn session_out_of_order_responses_timeouts_and_cancellation() {
    let ((reader, writer), mut server) = pipes();
    let session = LspSession::from_streams(reader, writer, None);

    let (a, b, ()) = tokio::join!(
        session.request_value("test/a", json!({ "n": 1 }), STEP),
        session.request_value("test/b", json!({ "n": 2 }), STEP),
        async {
            let first = server.recv().await;
            let second = server.recv().await;
            // Answer in reverse order.
            server
                .respond(&second["id"], json!(second["method"].clone()))
                .await;
            server
                .respond(&first["id"], json!(first["method"].clone()))
                .await;
        }
    );
    assert_eq!(a, Ok(json!("test/a")));
    assert_eq!(b, Ok(json!("test/b")));

    // Timeout -> `$/cancelRequest` + no leaked pending entry.
    let (slow, cancelled_id) = tokio::join!(
        session.request_value("test/slow", Value::Null, Duration::from_millis(50)),
        async {
            let request = server.recv().await;
            assert_eq!(request["method"], "test/slow");
            let cancel = server.recv().await;
            assert_eq!(cancel["method"], "$/cancelRequest");
            assert_eq!(cancel["params"]["id"], request["id"]);
            request["id"].clone()
        }
    );
    assert_eq!(
        slow,
        Err(LspDapError::RequestTimeout {
            method: "test/slow".to_owned(),
            timeout_ms: 50
        })
    );
    assert_eq!(session.pending_requests(), 0);
    // A late response for the cancelled id is ignored.
    server.respond(&cancelled_id, json!(1)).await;

    // Dropping the future (e.g. losing a `select!`) cancels as well.
    tokio::select! {
        _ = session.request_value("test/dropped", Value::Null, STEP) => panic!("must not complete"),
        () = tokio::time::sleep(Duration::from_millis(50)) => {}
    }
    let request = server.recv().await;
    assert_eq!(request["method"], "test/dropped");
    let cancel = server.recv().await;
    assert_eq!(cancel["method"], "$/cancelRequest");
    assert_eq!(cancel["params"]["id"], request["id"]);
    assert_eq!(session.pending_requests(), 0);

    // Error responses and `RequestCancelled`.
    let (failed, cancelled, ()) = tokio::join!(
        session.request_value("test/fail", Value::Null, STEP),
        session.request_value("test/cancelled", Value::Null, STEP),
        async {
            for _ in 0..2 {
                let req = server.recv().await;
                let code = if req["method"] == "test/fail" {
                    -32602
                } else {
                    -32800
                };
                server
                    .send(json!({ "jsonrpc": "2.0", "id": req["id"], "error": { "code": code, "message": "nope" } }))
                    .await;
            }
        }
    );
    assert_eq!(
        failed,
        Err(LspDapError::ProtocolError {
            code: -32602,
            message: "nope".to_owned()
        })
    );
    assert_eq!(
        cancelled,
        Err(LspDapError::RequestCancelled {
            method: "test/cancelled".to_owned()
        })
    );

    // Notifications are broadcast; server requests without a handler get MethodNotFound.
    let mut notes = session.subscribe_notifications();
    server
        .notify("window/logMessage", json!({ "message": "hi" }))
        .await;
    let note = tokio::time::timeout(STEP, notes.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(note.method, "window/logMessage");
    let response = server
        .server_request(json!(99), "workspace/configuration", json!({ "items": [] }))
        .await;
    assert_eq!(response["error"]["code"], json!(-32601));

    // Closing the stream fails pending requests with `TransportClosed`.
    let (closed, ()) = tokio::join!(
        session.request_value("test/never", Value::Null, STEP),
        async {
            let _request = server.recv().await;
            drop(server);
        }
    );
    assert_eq!(closed, Err(LspDapError::TransportClosed));
    assert!(session.is_closed());
}

/// Applies LSP content changes (UTF-16 positions) to `text`, mirroring the server.
fn apply_changes(text: &mut String, changes: &[Value]) {
    fn offset(text: &str, position: &Value) -> usize {
        let line = usize::try_from(position["line"].as_u64().unwrap()).unwrap();
        let character = usize::try_from(position["character"].as_u64().unwrap()).unwrap();
        let mut byte = 0;
        for _ in 0..line {
            byte += text[byte..].find('\n').unwrap() + 1;
        }
        let mut units = 0;
        for (i, ch) in text[byte..].char_indices() {
            if units >= character {
                return byte + i;
            }
            units += ch.len_utf16();
        }
        assert_eq!(units, character);
        text.len()
    }
    for change in changes {
        if change.get("range").is_none() {
            change["text"].as_str().unwrap().clone_into(text);
            continue;
        }
        let start = offset(text, &change["range"]["start"]);
        let end = offset(text, &change["range"]["end"]);
        text.replace_range(start..end, change["text"].as_str().unwrap());
    }
}

fn doc_path() -> PathBuf {
    project_root().join("src/main/java/Zażółć.java")
}

#[tokio::test]
async fn incremental_utf16_sync_after_multi_cursor_edits() {
    let (client, mut server) = connect().await;
    let path = doc_path();
    let initial = "class A {\n  String s = \"zażółć 🦀\";\n  int x;\n}\n";
    let mut buffer = Buffer::with_id(BufferId::new(7), initial);
    let mut tracker = DocumentSyncTracker::new();

    let opened_snapshot = buffer.snapshot();
    let (opened, ()) = tokio::join!(
        tracker.open_document(&client, &path, "java", &opened_snapshot),
        async {
            let open = server.recv().await;
            assert_eq!(open["method"], "textDocument/didOpen");
            let doc = &open["params"]["textDocument"];
            assert_eq!(doc["uri"], path_to_lsp_uri(&path).unwrap().as_str());
            assert_eq!(doc["languageId"], "java");
            assert_eq!(doc["version"], json!(0));
            assert_eq!(doc["text"], initial);
        }
    );
    opened.unwrap();
    assert!(tracker.is_open(&path));

    // Multi-cursor edit after a surrogate pair + a single edit on another line.
    let crab_end = initial.find('🦀').unwrap() + '🦀'.len_utf8();
    let x_pos = initial.find("x;").unwrap();
    buffer
        .apply_edits(&[
            TextEdit::insert(crab_end, " 😀ę"),
            TextEdit::new(x_pos..x_pos + 1, "yż"),
        ])
        .unwrap();
    let a_pos = initial.find('A').unwrap();
    buffer.apply_edit(a_pos..a_pos + 1, "Ćma").unwrap();

    let mut mirror = initial.to_owned();
    let snapshot = buffer.snapshot();
    let (mode, ()) = tokio::join!(
        tracker.sync_changes(&client, &path, buffer.edit_log(), &snapshot),
        async {
            let change = server.recv().await;
            assert_eq!(change["method"], "textDocument/didChange");
            assert_eq!(change["params"]["textDocument"]["version"], json!(2));
            let changes = change["params"]["contentChanges"].as_array().unwrap();
            assert_eq!(changes.len(), 3);
            assert!(changes.iter().all(|c| c.get("range").is_some()));
            apply_changes(&mut mirror, changes);
        }
    );
    assert_eq!(mode, Ok(SyncModeApplied::Incremental));
    assert_eq!(mirror, buffer.snapshot().text());
    assert_eq!(
        tracker.document(&path).unwrap().synced_version,
        buffer.version()
    );

    // Nothing new -> no traffic.
    assert_eq!(
        tracker
            .sync_changes(&client, &path, buffer.edit_log(), &buffer.snapshot())
            .await,
        Ok(SyncModeApplied::NoOp)
    );

    let (saved, ()) = tokio::join!(
        tracker.save_document(&client, &path, Some("saved")),
        async {
            let save = server.recv().await;
            assert_eq!(save["method"], "textDocument/didSave");
            assert_eq!(save["params"]["text"], "saved");
        }
    );
    saved.unwrap();
    let (closed, ()) = tokio::join!(tracker.close_document(&client, &path), async {
        let close = server.recv().await;
        assert_eq!(close["method"], "textDocument/didClose");
    });
    closed.unwrap();
    assert!(tracker.is_empty());
    assert!(matches!(
        tracker
            .sync_changes(&client, &path, buffer.edit_log(), &buffer.snapshot())
            .await,
        Err(LspDapError::InvalidState { .. })
    ));
}

#[tokio::test]
async fn full_resync_fallback_after_history_window_overflow() {
    let (client, mut server) = connect().await;
    let path = doc_path();
    let mut buffer = Buffer::with_edit_log_config(
        BufferId::new(9),
        "ab\n",
        EditLogConfig {
            max_versions: 2,
            max_retained_bytes: 1024,
        },
    );
    let mut tracker = DocumentSyncTracker::new();
    let opened_snapshot = buffer.snapshot();
    let (opened, _) = tokio::join!(
        tracker.open_document(&client, &path, "java", &opened_snapshot),
        server.recv()
    );
    opened.unwrap();

    for i in 0..5 {
        buffer.apply_edit(0..0, &format!("ł{i}")).unwrap();
    }
    let snapshot = buffer.snapshot();
    let (mode, change) = tokio::join!(
        tracker.sync_changes(&client, &path, buffer.edit_log(), &snapshot),
        server.recv()
    );
    assert_eq!(mode, Ok(SyncModeApplied::FullFallback));
    let changes = change["params"]["contentChanges"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    assert!(changes[0].get("range").is_none());
    assert_eq!(changes[0]["text"], snapshot.text());
    assert_eq!(change["params"]["textDocument"]["version"], json!(5));
    assert_eq!(
        tracker.document(&path).unwrap().synced_version,
        buffer.version()
    );

    // Opening twice is rejected; a snapshot of another buffer is rejected.
    assert!(matches!(
        tracker
            .open_document(&client, &path, "java", &buffer.snapshot())
            .await,
        Err(LspDapError::InvalidState { .. })
    ));
    let other = Buffer::with_id(BufferId::new(10), "x");
    assert!(matches!(
        tracker
            .sync_changes(&client, &path, other.edit_log(), &other.snapshot())
            .await,
        Err(LspDapError::InvalidState { .. })
    ));
    assert!(matches!(
        tracker
            .open_document(
                &client,
                Path::new("relative.java"),
                "java",
                &other.snapshot()
            )
            .await,
        Err(LspDapError::InvalidUri { .. })
    ));
}

#[cfg(unix)]
mod process {
    use std::collections::BTreeMap;

    use kova_toolchain::JdtlsLaunchSpec;

    use super::*;

    fn spec(java_bin: &str, args: Vec<String>) -> JdtlsLaunchSpec {
        let dir = std::env::temp_dir();
        JdtlsLaunchSpec {
            java_bin: PathBuf::from(java_bin),
            jvm_and_app_args: args,
            env: BTreeMap::new(),
            working_dir: dir.clone(),
            workspace_data_dir: dir.join("data"),
            osgi_config_dir: dir.join("config"),
            initialization_options: json!({ "fromSpec": true }),
        }
    }

    fn process_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[tokio::test]
    async fn graceful_shutdown_kills_unresponsive_server_process() {
        // A fake server: answers `initialize` (id 1), logs to stderr, then hangs forever.
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        let script = format!(
            "printf 'Content-Length: {}\\r\\n\\r\\n{body}'; echo 'jdt.ls booting' >&2; exec sleep 30",
            body.len()
        );
        let mut config = ClientRuntimeConfig::new(project_root());
        config.initialize_timeout = STEP;
        let client =
            JdtlsClient::start_with_spec(spec("/bin/sh", vec!["-c".into(), script]), config)
                .await
                .unwrap();
        let pid = client.child_pid().unwrap();
        assert!(process_alive(pid));
        assert_eq!(client.state(), LspLifecycleState::IndexingProject);

        for _ in 0..100 {
            if !client.stderr_tail().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(client.stderr_tail(), vec!["jdt.ls booting".to_owned()]);

        client
            .shutdown_graceful(Duration::from_millis(300))
            .await
            .unwrap();
        assert_eq!(client.state(), LspLifecycleState::Stopped);
        assert!(!process_alive(pid), "jdt.ls process {pid} must be gone");
    }

    #[tokio::test]
    async fn spawn_failure_is_reported() {
        let result = JdtlsClient::start_with_spec(
            spec("/nonexistent/kova/java", Vec::new()),
            ClientRuntimeConfig::new(project_root()),
        )
        .await;
        assert!(matches!(result, Err(LspDapError::SpawnFailed { .. })));
    }
}
