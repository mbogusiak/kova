//! Host side of the JVM *Debugger* (`KOVA-041`): runs [`DebugCommand`]s from the shell on the
//! `tokio` runtime through a `kova_lsp_dap::dap::DebugSession` and reports
//! [`DebugUpdate`]s back as [`AppEvent::Debug`].
//!
//! The DAP endpoint is the `java-debug` server that `jdt.ls` opens for
//! `vscode.java.startDebugSession`. Until the IDE manages a `jdt.ls` process itself, the
//! endpoint is taken from `KOVA_DAP_ADAPTER=host:port`; without it, a start reports a clear
//! error in the *Debugger* dock instead of hanging.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use kova_lsp_dap::dap::{
    BreakpointStore, DapClient, DapScope, DapStackFrame, DapVariable, DebugEvent, DebugSession,
    EvaluateContext, JavaAttachConfig, JavaLaunchConfig, OutputCategory, SourceBreakpoint,
    StepKind, VariablesFilter, VariablesPage,
};
use kova_ui::debugger::{
    BreakpointSpec, DebugCommand, DebugCommandSink, DebugLaunchRequest, DebugStep, DebugUpdate,
    DebuggerStatus, EvaluateTarget, FrameItem, STACK_PAGE_SIZE, ThreadItem, VariableItem,
};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::AppEvent;

/// Environment variable naming a running `java-debug` DAP server (`host:port`).
pub const DAP_ADAPTER_ENV: &str = "KOVA_DAP_ADAPTER";

type Current = Arc<Mutex<Option<(u64, mpsc::UnboundedSender<DebugCommand>)>>>;

/// Creates the [`DebugCommandSink`] installed on the shell.
///
/// Starting a session ends the previous one.
#[must_use]
pub fn debug_command_sink(
    runtime: Handle,
    events: mpsc::UnboundedSender<AppEvent>,
) -> DebugCommandSink {
    let current: Current = Arc::new(Mutex::new(None));
    Arc::new(move |session_id: u64, command: DebugCommand| {
        let mut slot = current.lock().unwrap_or_else(PoisonError::into_inner);
        if let DebugCommand::Start(request) = command {
            if let Some((_, previous)) = slot.take() {
                drop(previous.send(DebugCommand::Stop));
            }
            let (tx, rx) = mpsc::unbounded_channel();
            *slot = Some((request.session_id, tx));
            let adapter = std::env::var(DAP_ADAPTER_ENV).ok();
            runtime.spawn(run_session(request, adapter, rx, events.clone()));
            return;
        }
        if let Some((id, tx)) = slot.as_ref()
            && *id == session_id
        {
            drop(tx.send(command));
        }
    })
}

struct Reporter {
    session_id: u64,
    events: mpsc::UnboundedSender<AppEvent>,
}

impl Reporter {
    fn send(&self, update: DebugUpdate) {
        drop(self.events.send(AppEvent::Debug {
            session_id: self.session_id,
            update,
        }));
    }

    fn terminated(&self, message: Option<String>) {
        self.send(DebugUpdate::Status(DebuggerStatus::Terminated { message }));
    }
}

/// Parses `host:port`.
fn parse_endpoint(value: &str) -> Option<(String, u16)> {
    let (host, port) = value.trim().rsplit_once(':')?;
    let port = port.parse().ok().filter(|p| *p != 0)?;
    let host = if host.is_empty() { "127.0.0.1" } else { host };
    Some((host.to_owned(), port))
}

async fn run_session(
    request: DebugLaunchRequest,
    adapter: Option<String>,
    mut commands: mpsc::UnboundedReceiver<DebugCommand>,
    events: mpsc::UnboundedSender<AppEvent>,
) {
    let reporter = Reporter {
        session_id: request.session_id,
        events,
    };
    let Some((host, port)) = adapter.as_deref().and_then(parse_endpoint) else {
        reporter.terminated(Some(format!(
            "java-debug adapter not reachable: start it through jdt.ls \
             (`vscode.java.startDebugSession`) and set {DAP_ADAPTER_ENV}=host:port"
        )));
        return;
    };
    let (client, adapter_events) = match DapClient::connect_tcp(&host, port).await {
        Ok(connected) => connected,
        Err(err) => {
            reporter.terminated(Some(err.to_string()));
            return;
        }
    };
    let mut session =
        DebugSession::new(client, adapter_events).with_breakpoints(store_of(&request.breakpoints));
    let target = match request.attach_port {
        Some(port) => JavaAttachConfig::localhost(port).target(),
        None => JavaLaunchConfig {
            // `Class#method` test targets launch their class.
            main_class: request
                .main_class
                .split('#')
                .next()
                .unwrap_or_default()
                .to_owned(),
            cwd: request.project_root.clone(),
            ..JavaLaunchConfig::default()
        }
        .target(),
    };
    if let Err(err) = session.start(&target).await {
        reporter.terminated(Some(err.to_string()));
        return;
    }
    let files: Vec<PathBuf> = request.breakpoints.keys().cloned().collect();
    for file in &files {
        report_verified(&session, file, &reporter);
    }
    reporter.send(DebugUpdate::Status(DebuggerStatus::Running));

    loop {
        let next = tokio::select! {
            command = commands.recv() => Step::Command(command),
            event = session.next_event() => Step::Event(event),
        };
        let keep_going = match next {
            Step::Command(None | Some(DebugCommand::Stop)) => {
                drop(session.stop().await);
                reporter.terminated(None);
                false
            }
            Step::Command(Some(command)) => {
                if let Err(message) = execute(&mut session, command, &reporter).await {
                    reporter.send(DebugUpdate::Output(format!("debugger: {message}\n")));
                }
                true
            }
            Step::Event(Some(event)) => handle_event(&mut session, event, &reporter).await,
            Step::Event(None) => {
                reporter.terminated(None);
                false
            }
        };
        if !keep_going {
            break;
        }
    }
}

enum Step {
    Command(Option<DebugCommand>),
    Event(Option<DebugEvent>),
}

async fn execute(
    session: &mut DebugSession,
    command: DebugCommand,
    reporter: &Reporter,
) -> Result<(), String> {
    let err = |e: kova_lsp_dap::LspDapError| e.to_string();
    match command {
        DebugCommand::Start(_) | DebugCommand::Stop => {}
        DebugCommand::SetBreakpoints { file, breakpoints } => {
            session
                .replace_file_breakpoints(&file, source_breakpoints(&breakpoints))
                .await
                .map_err(err)?;
            report_verified(session, &file, reporter);
        }
        DebugCommand::Resume { thread_id } => {
            session.resume(thread_id).await.map_err(err)?;
            reporter.send(DebugUpdate::Status(DebuggerStatus::Running));
        }
        DebugCommand::Step { kind, thread_id } => {
            let kind = match kind {
                DebugStep::Over => StepKind::Over,
                DebugStep::Into => StepKind::Into,
                DebugStep::Out => StepKind::Out,
            };
            session.step(kind, thread_id).await.map_err(err)?;
            reporter.send(DebugUpdate::Status(DebuggerStatus::Running));
        }
        DebugCommand::LoadStack { thread_id } => {
            let page = session
                .stack_trace(thread_id, 0, STACK_PAGE_SIZE)
                .await
                .map_err(err)?;
            reporter.send(DebugUpdate::Stack {
                thread_id,
                frames: page.frames.iter().map(frame_item).collect(),
            });
        }
        DebugCommand::LoadScopes { frame_id } => {
            let scopes = session.scopes(frame_id).await.map_err(err)?;
            reporter.send(DebugUpdate::Scopes {
                frame_id,
                scopes: scopes.iter().map(scope_item).collect(),
            });
        }
        DebugCommand::LoadVariables {
            reference,
            start,
            count,
            indexed,
        } => {
            let page = VariablesPage {
                start,
                count,
                filter: if indexed {
                    VariablesFilter::Indexed
                } else {
                    VariablesFilter::All
                },
            };
            let items = session.variables(reference, page).await.map_err(err)?;
            reporter.send(DebugUpdate::Variables {
                reference,
                start,
                items: items.iter().map(variable_item).collect(),
            });
        }
        DebugCommand::Evaluate {
            expression,
            frame_id,
            target,
        } => {
            let context = match target {
                EvaluateTarget::Dialog => EvaluateContext::Repl,
                EvaluateTarget::Watch(_) => EvaluateContext::Watch,
            };
            let result = session
                .evaluate(&expression, frame_id, context)
                .await
                .map(|r| VariableItem {
                    name: expression.clone(),
                    value: r.result,
                    type_name: r.type_name,
                    reference: r.variables_reference,
                    indexed: None,
                    evaluate_name: Some(expression),
                })
                .map_err(err);
            reporter.send(DebugUpdate::Evaluated { target, result });
        }
    }
    Ok(())
}

async fn handle_event(session: &mut DebugSession, event: DebugEvent, reporter: &Reporter) -> bool {
    match event {
        DebugEvent::Stopped(info) => {
            if let Ok(threads) = session.threads().await {
                reporter.send(DebugUpdate::Threads(
                    threads
                        .into_iter()
                        .map(|t| ThreadItem {
                            id: t.id,
                            name: t.name,
                        })
                        .collect(),
                ));
            }
            let reason = match info.description {
                Some(description) if info.reason == "exception" => {
                    format!("exception: {description}")
                }
                _ => info.reason,
            };
            reporter.send(DebugUpdate::Status(DebuggerStatus::Suspended {
                reason,
                thread_id: info.thread_id,
            }));
        }
        DebugEvent::Continued { .. } => {
            reporter.send(DebugUpdate::Status(DebuggerStatus::Running));
        }
        DebugEvent::Output { category, text } => {
            let text = match category {
                OutputCategory::Stderr => format!("[stderr] {text}"),
                OutputCategory::Stdout | OutputCategory::Console => text,
            };
            reporter.send(DebugUpdate::Output(text));
        }
        DebugEvent::Breakpoint { status, .. } => {
            let files: Vec<PathBuf> = session
                .breakpoints()
                .files()
                .map(Path::to_path_buf)
                .collect();
            for file in files {
                if status.id.is_some()
                    && session
                        .breakpoint_statuses(&file)
                        .iter()
                        .any(|s| s.id == status.id)
                {
                    report_verified(session, &file, reporter);
                }
            }
        }
        DebugEvent::Exited { exit_code } => {
            reporter.send(DebugUpdate::Output(format!(
                "Process finished with exit code {exit_code}\n"
            )));
        }
        DebugEvent::Terminated => {
            reporter.terminated(None);
            return false;
        }
        DebugEvent::Initialized | DebugEvent::Thread { .. } => {}
    }
    true
}

/// Reports the adapter verification of every breakpoint of `file` (statuses are positional).
fn report_verified(session: &DebugSession, file: &Path, reporter: &Reporter) {
    let lines: Vec<u32> = session
        .breakpoints()
        .for_file(file)
        .map(|b| b.line)
        .collect();
    for (line, status) in lines.into_iter().zip(session.breakpoint_statuses(file)) {
        reporter.send(DebugUpdate::BreakpointVerified {
            file: file.to_path_buf(),
            line,
            verified: status.verified,
        });
    }
}

fn source_breakpoints(lines: &BTreeMap<u32, BreakpointSpec>) -> Vec<SourceBreakpoint> {
    lines
        .iter()
        .map(|(line, spec)| SourceBreakpoint {
            line: *line,
            condition: spec.condition.clone(),
            hit_condition: None,
            log_message: spec.log_message.clone(),
        })
        .collect()
}

fn store_of(breakpoints: &BTreeMap<PathBuf, BTreeMap<u32, BreakpointSpec>>) -> BreakpointStore {
    let mut store = BreakpointStore::new();
    for (file, lines) in breakpoints {
        for breakpoint in source_breakpoints(lines) {
            store.upsert(file, breakpoint);
        }
    }
    store
}

fn frame_item(frame: &DapStackFrame) -> FrameItem {
    FrameItem {
        id: frame.id,
        name: frame.name.clone(),
        path: frame.path(),
        line: frame.line,
    }
}

fn scope_item(scope: &DapScope) -> VariableItem {
    VariableItem {
        name: scope.name.clone(),
        value: String::new(),
        type_name: None,
        reference: scope.variables_reference,
        indexed: scope.indexed_variables,
        evaluate_name: None,
    }
}

fn variable_item(variable: &DapVariable) -> VariableItem {
    VariableItem {
        name: variable.name.clone(),
        value: variable.value.clone(),
        type_name: variable.type_name.clone(),
        reference: variable.variables_reference,
        indexed: variable.indexed_variables,
        evaluate_name: variable.evaluate_name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_adapter_endpoints() {
        assert_eq!(
            parse_endpoint("localhost:41234"),
            Some(("localhost".to_owned(), 41234))
        );
        assert_eq!(
            parse_endpoint(":5005"),
            Some(("127.0.0.1".to_owned(), 5005))
        );
        assert_eq!(parse_endpoint("localhost"), None);
        assert_eq!(parse_endpoint("h:0"), None);
    }

    #[test]
    fn maps_breakpoint_specs_to_source_breakpoints() {
        let mut lines = BTreeMap::new();
        lines.insert(
            12,
            BreakpointSpec {
                condition: Some("i == 3".to_owned()),
                ..BreakpointSpec::default()
            },
        );
        lines.insert(
            20,
            BreakpointSpec {
                log_message: Some("i={i}".to_owned()),
                ..BreakpointSpec::default()
            },
        );
        let file = PathBuf::from("/p/A.java");
        let mut all = BTreeMap::new();
        all.insert(file.clone(), lines);
        let store = store_of(&all);
        let args = store.set_breakpoints_arguments(&file);
        assert_eq!(args["breakpoints"][0]["condition"], "i == 3");
        assert_eq!(args["breakpoints"][1]["logMessage"], "i={i}");
    }

    #[tokio::test]
    async fn missing_adapter_reports_a_clear_error() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_commands_tx, commands_rx) = mpsc::unbounded_channel();
        let request = DebugLaunchRequest {
            session_id: 3,
            label: "App".to_owned(),
            main_class: "com.acme.App".to_owned(),
            attach_port: None,
            project_root: None,
            breakpoints: BTreeMap::new(),
        };
        run_session(request, None, commands_rx, tx).await;
        let Some(AppEvent::Debug { session_id, update }) = rx.recv().await else {
            panic!("expected a debug update");
        };
        assert_eq!(session_id, 3);
        assert!(matches!(
            update,
            DebugUpdate::Status(DebuggerStatus::Terminated { message: Some(ref m) })
                if m.contains(DAP_ADAPTER_ENV)
        ));
    }
}
