//! KOVA-040 integration tests: a mock BSP server on `tokio::io::duplex` drives the
//! `BspClient` handshake, `workspace/buildTargets`, `buildTarget/test` with streamed test
//! notifications, diagnostics and shutdown; the events build a `kova_tools` `TestRun`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "integration tests"
)]

use std::time::{Duration, SystemTime};

use kova_lsp_dap::bsp::{BspClient, BspEvent, TestRunRequest, run_over_bsp};
use kova_lsp_dap::lsp::{path_to_lsp_uri, read_lsp_frame, write_lsp_frame};
use kova_tools::junit::{TestId, TestRun, TestStatus};
use serde_json::{Value, json};
use tokio::io::{BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::mpsc;

const STEP: Duration = Duration::from_secs(5);

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

    async fn expect(&mut self, method: &str) -> Value {
        let message = self.recv().await;
        assert_eq!(message["method"], method, "unexpected message {message}");
        message
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
}

fn pipes() -> (
    (ReadHalf<DuplexStream>, WriteHalf<DuplexStream>),
    MockServer,
) {
    let (client_end, server_end) = tokio::io::duplex(1 << 20);
    let (client_read, client_write) = tokio::io::split(client_end);
    let (server_read, server_write) = tokio::io::split(server_end);
    (
        (client_read, client_write),
        MockServer {
            reader: BufReader::new(server_read),
            writer: server_write,
        },
    )
}

async fn handshake(server: &mut MockServer, root_uri: &str) {
    let init = server.expect("build/initialize").await;
    assert_eq!(init["params"]["rootUri"], root_uri);
    assert_eq!(init["params"]["bspVersion"], "2.1.0");
    server
        .respond(
            &init["id"],
            json!({"displayName": "mock-gradle-bsp", "version": "1", "bspVersion": "2.1.0",
                   "capabilities": {"testProvider": {"languageIds": ["java"]}}}),
        )
        .await;
    server.expect("build/initialized").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_test_method_runs_over_bsp_and_streams_live_statuses() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let root_uri = path_to_lsp_uri(&root).unwrap().as_str().to_owned();
    let module_uri = format!("{root_uri}/app");
    let file = root.join("app/src/test/java/com/kova/shop/OrderServiceTest.java");
    let ((reader, writer), mut server) = pipes();

    let server_task = tokio::spawn({
        let root_uri = root_uri.clone();
        let module_uri = module_uri.clone();
        async move {
            handshake(&mut server, &root_uri).await;
            let targets = server.expect("workspace/buildTargets").await;
            server
                .respond(
                    &targets["id"],
                    json!({"targets": [
                        {"id": {"uri": "app-main"}, "displayName": "app", "baseDirectory": module_uri,
                         "tags": ["library"], "languageIds": ["java"],
                         "capabilities": {"canCompile": true, "canTest": false}},
                        {"id": {"uri": "app-test"}, "displayName": "app-test", "baseDirectory": module_uri,
                         "tags": ["test"], "languageIds": ["java"],
                         "capabilities": {"canCompile": true, "canTest": true}},
                        {"id": {"uri": "other-test"}, "displayName": "other", "baseDirectory": format!("{root_uri}/other"),
                         "tags": ["test"], "capabilities": {"canTest": true}}
                    ]}),
                )
                .await;
            let test = server.expect("buildTarget/test").await;
            let params = &test["params"];
            assert_eq!(params["targets"], json!([{"uri": "app-test"}]));
            assert_eq!(params["dataKind"], "scala-test-suites-selection");
            assert_eq!(
                params["data"]["suites"],
                json!([{"className": "com.kova.shop.OrderServiceTest",
                        "tests": ["shouldRejectNegativeAmount"]}])
            );
            let origin = params["originId"].clone();
            server
                .notify(
                    "build/publishDiagnostics",
                    json!({"textDocument": {"uri": format!("{module_uri}/src/main/java/A.java")},
                           "buildTarget": {"uri": "app-test"}, "originId": origin, "reset": true,
                           "diagnostics": [{"range": {"start": {"line": 1, "character": 0},
                                                      "end": {"line": 1, "character": 4}},
                                            "severity": 2, "message": "deprecated"}]}),
                )
                .await;
            server
                .notify(
                    "build/taskStart",
                    json!({"taskId": {"id": "t"}, "originId": origin, "dataKind": "test-task",
                           "data": {"target": {"uri": "app-test"}}}),
                )
                .await;
            server
                .notify(
                    "build/taskStart",
                    json!({"taskId": {"id": "c", "parents": ["t"]}, "dataKind": "test-start",
                           "data": {"displayName": "com.kova.shop.OrderServiceTest"}}),
                )
                .await;
            server
                .notify(
                    "build/taskStart",
                    json!({"taskId": {"id": "m", "parents": ["t", "c"]}, "eventTime": 100,
                           "dataKind": "test-start",
                           "data": {"displayName": "shouldRejectNegativeAmount()"}}),
                )
                .await;
            server
                .notify(
                    "build/taskFinish",
                    json!({"taskId": {"id": "m", "parents": ["t", "c"]}, "eventTime": 164,
                           "dataKind": "test-finish",
                           "data": {"displayName": "shouldRejectNegativeAmount()", "status": 2,
                                    "message": "java.lang.AssertionError: Expected IllegalArgumentException\n\tat com.kova.shop.OrderService.validate(OrderService.java:84)"}}),
                )
                .await;
            server
                .notify(
                    "build/taskFinish",
                    json!({"taskId": {"id": "c", "parents": ["t"]}, "dataKind": "test-finish",
                           "data": {"displayName": "com.kova.shop.OrderServiceTest", "status": 2}}),
                )
                .await;
            server
                .notify(
                    "build/taskFinish",
                    json!({"taskId": {"id": "t"}, "dataKind": "test-report",
                           "data": {"target": {"uri": "app-test"}, "passed": 0, "failed": 1}}),
                )
                .await;
            server
                .respond(&test["id"], json!({"originId": origin, "statusCode": 2}))
                .await;
            let shutdown = server.expect("build/shutdown").await;
            server.respond(&shutdown["id"], Value::Null).await;
            server.expect("build/exit").await;
        }
    });

    let client = BspClient::start_with_streams(reader, writer, &root)
        .await
        .unwrap();
    assert_eq!(client.server_name(), "mock-gradle-bsp");
    let request = TestRunRequest {
        project_root: root.clone(),
        targets: vec!["com.kova.shop.OrderServiceTest#shouldRejectNegativeAmount".to_owned()],
        file: Some(file),
        ..TestRunRequest::default()
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let success = run_over_bsp(&client, &request, &tx, SystemTime::now())
        .await
        .unwrap();
    assert!(!success);
    server_task.await.unwrap();

    let mut run = TestRun::new("OrderServiceTest", request.targets.clone());
    let mut diagnostics = 0;
    let mut live_running_seen = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            BspEvent::Test(event) => {
                run.apply_batch([event]);
                if run.counts().running == 1 {
                    live_running_seen = true;
                }
            }
            BspEvent::Diagnostics { diagnostics: d, .. } => diagnostics += d.len(),
            _ => {}
        }
    }
    assert!(
        live_running_seen,
        "the method must be shown as running before it finishes"
    );
    assert_eq!(diagnostics, 1);
    let counts = run.counts();
    assert_eq!((counts.failed, counts.total), (1, 1));
    let method = run
        .find(
            &TestId::method(
                "com.kova.shop.OrderServiceTest",
                "shouldRejectNegativeAmount",
            )
            .in_suite("app-test"),
        )
        .unwrap();
    let node = run.node(method).unwrap();
    assert_eq!(node.status, TestStatus::Failed);
    assert_eq!(node.duration_ms, 64);
    assert_eq!(node.frames[0].line, Some(84));
    assert!(run.console().contains("Testing app-test"));
    assert_eq!(
        run.failed_targets(),
        vec!["com.kova.shop.OrderServiceTest#shouldRejectNegativeAmount".to_owned()]
    );
    assert_eq!(client.diagnostics().summary().total_warnings, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn falls_back_to_xml_reports_when_server_streams_no_tests() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let root_uri = path_to_lsp_uri(&root).unwrap().as_str().to_owned();
    let ((reader, writer), mut server) = pipes();
    let reports = root.join("build/test-results/test");

    let server_task = tokio::spawn({
        let reports = reports.clone();
        async move {
            handshake(&mut server, &root_uri).await;
            let targets = server.expect("workspace/buildTargets").await;
            server
                .respond(
                    &targets["id"],
                    json!({"targets": [{"id": {"uri": "root-test"}, "tags": ["test"],
                                        "capabilities": {"canTest": true}}]}),
                )
                .await;
            let test = server.expect("buildTarget/test").await;
            assert!(test["params"].get("dataKind").is_none());
            std::fs::create_dir_all(&reports).unwrap();
            std::fs::write(
                reports.join("TEST-com.kova.A.xml"),
                r#"<testsuite name="com.kova.A"><testcase name="ok()" classname="com.kova.A" time="0.01"/></testsuite>"#,
            )
            .unwrap();
            server.respond(&test["id"], json!({"statusCode": 1})).await;
            let shutdown = server.expect("build/shutdown").await;
            server.respond(&shutdown["id"], Value::Null).await;
            server.expect("build/exit").await;
        }
    });

    let client = BspClient::start_with_streams(reader, writer, &root)
        .await
        .unwrap();
    let request = TestRunRequest {
        project_root: root.clone(),
        ..TestRunRequest::default()
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let started = SystemTime::now();
    assert!(run_over_bsp(&client, &request, &tx, started).await.unwrap());
    server_task.await.unwrap();

    let mut run = TestRun::new("all", Vec::new());
    while let Ok(event) = rx.try_recv() {
        if let BspEvent::Test(event) = event {
            run.apply_batch([event]);
        }
    }
    assert_eq!(run.counts().passed, 1);
    assert!(run.console().contains("TEST-*.xml"));
    assert!(matches!(
        run.node(run.roots()[0]).map(|n| n.status),
        Some(TestStatus::Passed)
    ));
}
