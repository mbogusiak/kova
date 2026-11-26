//! KOVA-041 headless tests: a mock `java-debug` adapter on `tokio::io::duplex` drives the
//! `DebugSession` lifecycle (handshake order, breakpoints, conditional breakpoints,
//! logpoints, stepping, paged variables, *Evaluate Expression* in the selected frame,
//! reverse requests and termination).
//!
//! The mock evaluates breakpoint conditions of the form `<name> == <int>` itself, like the
//! JVM does through JDWP, so the tests check that a conditional breakpoint stops the thread
//! only when its condition holds.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "integration tests"
)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use kova_lsp_dap::LspDapError;
use kova_lsp_dap::dap::{
    BreakpointStore, DapClient, DebugEvent, DebugSession, DebugSessionState, EvaluateContext,
    JavaLaunchConfig, OutputCategory, SourceBreakpoint, StepKind, VariablesPage,
};
use kova_lsp_dap::lsp::{read_lsp_frame, write_lsp_frame};
use serde_json::{Value, json};
use tokio::io::{BufReader, DuplexStream, ReadHalf, WriteHalf};

const STEP: Duration = Duration::from_secs(5);
const MAIN_THREAD: i64 = 1;

struct MockAdapter {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    seq: i64,
}

impl MockAdapter {
    async fn recv(&mut self) -> Value {
        let frame = tokio::time::timeout(STEP, read_lsp_frame(&mut self.reader))
            .await
            .expect("mock adapter timed out waiting for a frame")
            .expect("valid frame")
            .expect("stream open");
        serde_json::from_slice(&frame).expect("valid JSON")
    }

    async fn expect(&mut self, command: &str) -> Value {
        let message = self.recv().await;
        assert_eq!(message["type"], "request", "unexpected message {message}");
        assert_eq!(message["command"], command, "unexpected request {message}");
        message
    }

    async fn send(&mut self, mut message: Value) {
        self.seq += 1;
        message["seq"] = json!(self.seq);
        let bytes = serde_json::to_vec(&message).unwrap();
        write_lsp_frame(&mut self.writer, &bytes).await.unwrap();
    }

    async fn respond(&mut self, request: &Value, body: Value) {
        self.send(json!({
            "type": "response", "request_seq": request["seq"], "success": true,
            "command": request["command"], "body": body,
        }))
        .await;
    }

    async fn fail(&mut self, request: &Value, message: &str) {
        self.send(json!({
            "type": "response", "request_seq": request["seq"], "success": false,
            "command": request["command"], "message": message,
        }))
        .await;
    }

    async fn event(&mut self, event: &str, body: Value) {
        self.send(json!({ "type": "event", "event": event, "body": body }))
            .await;
    }

    /// `initialize` → `launch` (held) → `initialized` → `setBreakpoints`* →
    /// `configurationDone` → `launch` response. Returns the `setBreakpoints` arguments.
    async fn handshake(&mut self, breakpoint_files: usize) -> Vec<Value> {
        let init = self.expect("initialize").await;
        assert_eq!(init["arguments"]["adapterID"], "java");
        assert_eq!(init["arguments"]["linesStartAt1"], true);
        assert_eq!(init["arguments"]["supportsVariablePaging"], true);
        self.respond(
            &init,
            json!({"supportsConfigurationDoneRequest": true, "supportsConditionalBreakpoints": true,
                   "supportsLogPoints": true, "supportsTerminateRequest": false,
                   "supportsCompletionsRequest": true}),
        )
        .await;

        // java-debug answers `launch` only after `configurationDone`.
        let launch = self.expect("launch").await;
        assert_eq!(launch["arguments"]["mainClass"], "com.acme.Orders");
        self.event("initialized", Value::Null).await;

        let mut sets = Vec::new();
        for _ in 0..breakpoint_files {
            let set = self.expect("setBreakpoints").await;
            let statuses: Vec<Value> = set["arguments"]["breakpoints"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(i, bp)| json!({"id": i + 1, "verified": true, "line": bp["line"]}))
                .collect();
            self.respond(&set, json!({ "breakpoints": statuses })).await;
            sets.push(set["arguments"].clone());
        }
        let done = self.expect("configurationDone").await;
        self.respond(&done, Value::Null).await;
        self.respond(&launch, Value::Null).await;
        sets
    }

    /// Runs `for (int i = 0; i < 5; i++) total += i;` with the breakpoints of `args`
    /// on line 12; returns the `i` values at which the thread was suspended.
    async fn run_loop(&mut self, args: &Value) -> Option<i64> {
        let breakpoints = args["breakpoints"].as_array().unwrap().clone();
        for i in 0..5 {
            for (index, bp) in breakpoints.iter().enumerate() {
                if bp["line"] != 12 {
                    continue;
                }
                if let Some(message) = bp.get("logMessage").and_then(Value::as_str) {
                    let text = message.replace("{i}", &i.to_string());
                    self.event(
                        "output",
                        json!({"category": "console", "output": format!("{text}\n")}),
                    )
                    .await;
                    continue;
                }
                if condition_holds(bp.get("condition").and_then(Value::as_str), i) {
                    self.event(
                        "stopped",
                        json!({"reason": "breakpoint", "threadId": MAIN_THREAD,
                               "allThreadsStopped": false, "hitBreakpointIds": [index + 1]}),
                    )
                    .await;
                    return Some(i);
                }
            }
        }
        None
    }
}

fn condition_holds(condition: Option<&str>, i: i64) -> bool {
    let Some(condition) = condition else {
        return true;
    };
    let (name, value) = condition.split_once("==").expect("mock supports `x == n`");
    assert_eq!(name.trim(), "i");
    value.trim().parse::<i64>().unwrap() == i
}

fn connect() -> (DebugSession, MockAdapter) {
    let (client_end, adapter_end) = tokio::io::duplex(1 << 20);
    let (client_read, client_write) = tokio::io::split(client_end);
    let (adapter_read, adapter_write) = tokio::io::split(adapter_end);
    let (client, events) = DapClient::spawn(client_read, client_write);
    (
        DebugSession::new(client.with_timeout(STEP), events),
        MockAdapter {
            reader: BufReader::new(adapter_read),
            writer: adapter_write,
            seq: 0,
        },
    )
}

fn source() -> PathBuf {
    PathBuf::from("/work/orders/src/main/java/com/acme/Orders.java")
}

fn launch() -> JavaLaunchConfig {
    JavaLaunchConfig {
        main_class: "com.acme.Orders".to_owned(),
        class_paths: vec!["/work/orders/build/classes/java/main".to_owned()],
        ..JavaLaunchConfig::default()
    }
}

async fn next(session: &mut DebugSession) -> DebugEvent {
    tokio::time::timeout(STEP, session.next_event())
        .await
        .expect("debug event in time")
        .expect("event stream open")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_with_conditional_breakpoint_stepping_variables_and_evaluate() {
    let (session, mut adapter) = connect();
    let file = source();
    let mut store = BreakpointStore::new();
    store.upsert(&file, SourceBreakpoint::conditional(12, "i == 3"));
    let mut session = session.with_breakpoints(store);
    assert_eq!(session.state(), DebugSessionState::NotStarted);

    let file_for_adapter = file.clone();
    let adapter_task = tokio::spawn(async move {
        let sets = adapter.handshake(1).await;
        assert_eq!(
            sets[0]["source"]["path"],
            file_for_adapter.display().to_string()
        );
        assert_eq!(
            sets[0]["breakpoints"],
            json!([{"line": 12, "condition": "i == 3"}])
        );
        // The conditional breakpoint suspends only at i == 3, not at i == 0..2.
        let stopped_at = adapter.run_loop(&sets[0]).await;
        assert_eq!(stopped_at, Some(3));

        let threads = adapter.expect("threads").await;
        adapter
            .respond(
                &threads,
                json!({"threads": [{"id": MAIN_THREAD, "name": "main"}, {"id": 7, "name": "pool-1"}]}),
            )
            .await;

        let trace = adapter.expect("stackTrace").await;
        assert_eq!(trace["arguments"]["threadId"], MAIN_THREAD);
        assert_eq!(trace["arguments"]["levels"], 20);
        adapter
            .respond(
                &trace,
                json!({"totalFrames": 2, "stackFrames": [
                    {"id": 100, "name": "Orders.total(int)", "line": 12, "column": 1,
                     "source": {"name": "Orders.java", "path": "/work/orders/src/main/java/com/acme/Orders.java"}},
                    {"id": 101, "name": "Orders.main(String[])", "line": 30, "column": 1,
                     "source": {"name": "Orders.java", "path": "/work/orders/src/main/java/com/acme/Orders.java"}}
                ]}),
            )
            .await;

        let scopes = adapter.expect("scopes").await;
        assert_eq!(scopes["arguments"]["frameId"], 101);
        adapter
            .respond(
                &scopes,
                json!({"scopes": [{"name": "Local", "variablesReference": 5, "expensive": false}]}),
            )
            .await;

        let vars = adapter.expect("variables").await;
        assert_eq!(
            vars["arguments"],
            json!({"variablesReference": 5, "start": 0, "count": 100})
        );
        adapter
            .respond(
                &vars,
                json!({"variables": [
                    {"name": "orders", "value": "int[1000]", "type": "int[]",
                     "variablesReference": 9, "indexedVariables": 1000},
                    {"name": "total", "value": "3", "type": "int", "variablesReference": 0}
                ]}),
            )
            .await;

        // Paged array: only the second page of 100 elements is fetched.
        let page = adapter.expect("variables").await;
        assert_eq!(
            page["arguments"],
            json!({"variablesReference": 9, "start": 100, "count": 100, "filter": "indexed"})
        );
        let items: Vec<Value> = (100..200)
            .map(|i| json!({"name": format!("[{i}]"), "value": i.to_string(), "variablesReference": 0}))
            .collect();
        adapter.respond(&page, json!({ "variables": items })).await;

        // Evaluate Expression in the context of the *selected* frame (101), not the top one.
        let eval = adapter.expect("evaluate").await;
        assert_eq!(eval["arguments"]["frameId"], 101);
        assert_eq!(eval["arguments"]["context"], "repl");
        assert_eq!(eval["arguments"]["expression"], "total * 2");
        adapter
            .respond(
                &eval,
                json!({"result": "6", "type": "int", "variablesReference": 0}),
            )
            .await;

        let bad = adapter.expect("evaluate").await;
        adapter
            .fail(&bad, "Cannot find local variable 'nope'")
            .await;

        let completions = adapter.expect("completions").await;
        adapter
            .respond(
                &completions,
                json!({"targets": [{"label": "total"}, {"label": "toString()"}]}),
            )
            .await;

        let step = adapter.expect("next").await;
        assert_eq!(step["arguments"]["threadId"], MAIN_THREAD);
        adapter.respond(&step, Value::Null).await;
        adapter
            .event(
                "stopped",
                json!({"reason": "step", "threadId": MAIN_THREAD}),
            )
            .await;

        let into = adapter.expect("stepIn").await;
        adapter.respond(&into, Value::Null).await;
        adapter
            .event(
                "stopped",
                json!({"reason": "step", "threadId": MAIN_THREAD}),
            )
            .await;
        let out = adapter.expect("stepOut").await;
        adapter.respond(&out, Value::Null).await;
        adapter
            .event(
                "stopped",
                json!({"reason": "step", "threadId": MAIN_THREAD}),
            )
            .await;

        let resume = adapter.expect("continue").await;
        adapter
            .respond(&resume, json!({"allThreadsContinued": true}))
            .await;
        adapter
            .event(
                "output",
                json!({"category": "stdout", "output": "total=10\n"}),
            )
            .await;
        adapter.event("exited", json!({"exitCode": 0})).await;
        adapter.event("terminated", Value::Null).await;

        let disconnect = adapter.expect("disconnect").await;
        assert_eq!(disconnect["arguments"]["terminateDebuggee"], false);
        adapter.respond(&disconnect, Value::Null).await;
    });

    session.start(&launch().target()).await.unwrap();
    assert_eq!(session.state(), DebugSessionState::Running);
    assert!(session.breakpoint_statuses(&file)[0].verified);

    let DebugEvent::Stopped(stopped) = next(&mut session).await else {
        panic!("expected stopped");
    };
    assert_eq!(stopped.reason, "breakpoint");
    assert_eq!(stopped.thread_id, Some(MAIN_THREAD));
    assert_eq!(session.state(), DebugSessionState::Stopped);

    let threads = session.threads().await.unwrap();
    assert_eq!(threads.len(), 2);
    let trace = session.stack_trace(MAIN_THREAD, 0, 20).await.unwrap();
    assert_eq!(trace.total_frames, Some(2));
    assert_eq!(trace.frames[0].path(), Some(file.clone()));
    let selected = &trace.frames[1];

    let scopes = session.scopes(selected.id).await.unwrap();
    let locals = session
        .variables(scopes[0].variables_reference, VariablesPage::first())
        .await
        .unwrap();
    assert_eq!(locals[0].indexed_variables, Some(1000));
    assert!(locals[0].has_children());
    // Served from the cache: no second `variables` request reaches the adapter.
    let again = session
        .variables(scopes[0].variables_reference, VariablesPage::first())
        .await
        .unwrap();
    assert_eq!(again, locals);
    let page = session
        .variables(
            locals[0].variables_reference,
            VariablesPage::indexed(1, 100),
        )
        .await
        .unwrap();
    assert_eq!(page.len(), 100);
    assert_eq!(page[0].name, "[100]");

    let result = session
        .evaluate("total * 2", Some(selected.id), EvaluateContext::Repl)
        .await
        .unwrap();
    assert_eq!(result.result, "6");
    assert_eq!(result.type_name.as_deref(), Some("int"));
    let err = session
        .evaluate("nope", Some(selected.id), EvaluateContext::Repl)
        .await
        .unwrap_err();
    assert!(
        matches!(err, LspDapError::DapRequestFailed { ref message, .. } if message.contains("nope")),
        "{err:?}"
    );
    let completions = session
        .completions("to", 3, Some(selected.id))
        .await
        .unwrap();
    assert_eq!(completions, ["total", "toString()"]);

    for kind in [StepKind::Over, StepKind::Into, StepKind::Out] {
        session.step(kind, MAIN_THREAD).await.unwrap();
        assert_eq!(session.state(), DebugSessionState::Running);
        // Stepping while running is rejected.
        assert!(matches!(
            session.step(StepKind::Over, MAIN_THREAD).await,
            Err(LspDapError::InvalidState { .. })
        ));
        let DebugEvent::Stopped(step) = next(&mut session).await else {
            panic!("expected step stop");
        };
        assert_eq!(step.reason, "step");
    }

    session.resume(MAIN_THREAD).await.unwrap();
    assert_eq!(
        next(&mut session).await,
        DebugEvent::Output {
            category: OutputCategory::Stdout,
            text: "total=10\n".to_owned()
        }
    );
    assert_eq!(
        next(&mut session).await,
        DebugEvent::Exited { exit_code: 0 }
    );
    assert_eq!(next(&mut session).await, DebugEvent::Terminated);
    assert_eq!(session.state(), DebugSessionState::Terminated);
    session.disconnect(false).await.unwrap();
    adapter_task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unmet_condition_never_suspends_and_logpoints_only_log() {
    let (session, mut adapter) = connect();
    let file = source();
    let mut store = BreakpointStore::new();
    store.upsert(&file, SourceBreakpoint::conditional(12, "i == 42"));
    let mut session = session.with_breakpoints(store);

    let adapter_task = tokio::spawn(async move {
        let sets = adapter.handshake(1).await;
        assert_eq!(adapter.run_loop(&sets[0]).await, None);

        // The user turns the breakpoint into a logpoint while running: the full set is resent.
        let set = adapter.expect("setBreakpoints").await;
        assert_eq!(
            set["arguments"]["breakpoints"],
            json!([{"line": 12, "logMessage": "i={i}"}, {"line": 20}])
        );
        adapter
            .respond(
                &set,
                json!({"breakpoints": [{"id": 5, "verified": true, "line": 12},
                                       {"id": 6, "verified": false, "line": 20, "message": "no code"}]}),
            )
            .await;
        assert_eq!(adapter.run_loop(&set["arguments"]).await, None);
        adapter
            .event(
                "breakpoint",
                json!({"reason": "changed", "breakpoint": {"id": 6, "verified": true, "line": 21}}),
            )
            .await;
        // Stop: without `supportsTerminateRequest` the session disconnects and kills the JVM.
        let disconnect = adapter.expect("disconnect").await;
        assert_eq!(disconnect["arguments"]["terminateDebuggee"], true);
        adapter.respond(&disconnect, Value::Null).await;
        adapter.event("terminated", Value::Null).await;
    });

    session.start(&launch().target()).await.unwrap();
    let mut breakpoints = vec![
        SourceBreakpoint::logpoint(12, "i={i}"),
        SourceBreakpoint::line(20),
    ];
    breakpoints.reverse();
    session
        .replace_file_breakpoints(&file, breakpoints)
        .await
        .unwrap();
    assert!(!session.breakpoint_statuses(&file)[1].verified);

    let mut logged = Vec::new();
    loop {
        match next(&mut session).await {
            DebugEvent::Output {
                category: OutputCategory::Console,
                text,
            } => logged.push(text),
            DebugEvent::Breakpoint { status, .. } => {
                assert_eq!(status.line, Some(21));
                break;
            }
            other => panic!("the thread must never suspend: {other:?}"),
        }
    }
    assert_eq!(logged, ["i=0\n", "i=1\n", "i=2\n", "i=3\n", "i=4\n"]);
    assert!(session.breakpoint_statuses(&file)[1].verified);
    session.stop().await.unwrap();
    assert_eq!(session.state(), DebugSessionState::Terminated);
    assert_eq!(next(&mut session).await, DebugEvent::Terminated);
    // Stopping twice is a no-op.
    session.stop().await.unwrap();
    adapter_task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reverse_requests_are_rejected_and_closed_transport_terminates() {
    let (mut session, mut adapter) = connect();
    let adapter_task = tokio::spawn(async move {
        adapter.handshake(0).await;
        adapter
            .send(json!({"type": "request", "command": "runInTerminal",
                         "arguments": {"kind": "integrated", "args": ["java"]}}))
            .await;
        let answer = adapter.recv().await;
        assert_eq!(answer["type"], "response");
        assert_eq!(answer["command"], "runInTerminal");
        assert_eq!(answer["success"], false);
        // Closing the connection ends the session.
        drop(adapter);
    });
    session.start(&launch().target()).await.unwrap();
    adapter_task.await.unwrap();
    assert_eq!(next(&mut session).await, DebugEvent::Terminated);
    assert!(session.next_event().await.is_none());
    assert!(matches!(
        session.threads().await,
        Err(LspDapError::TransportClosed)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_launch_and_toggle_before_start() {
    let (mut session, mut adapter) = connect();
    let file = Path::new("/work/A.java");
    // Not active yet: toggling only updates the cache.
    assert!(session.toggle_breakpoint(file, 3).await.unwrap());
    assert_eq!(session.breakpoints().len(), 1);

    let adapter_task = tokio::spawn(async move {
        let init = adapter.expect("initialize").await;
        adapter.respond(&init, json!({})).await;
        let launch = adapter.expect("launch").await;
        adapter
            .fail(&launch, "Main class com.acme.Orders not found")
            .await;
    });
    let err = session.start(&launch().target()).await.unwrap_err();
    assert!(
        matches!(err, LspDapError::DapRequestFailed { ref command, .. } if command == "launch"),
        "{err:?}"
    );
    assert!(matches!(
        session.start(&launch().target()).await,
        Err(LspDapError::InvalidState { .. })
    ));
    adapter_task.await.unwrap();
}
