//! Host side of the JVM *Debugger* (`KOVA-041`): runs [`DebugCommand`]s from the shell on the
//! `tokio` runtime through a `kova_lsp_dap::dap::DebugSession` and reports
//! [`DebugUpdate`]s back as [`AppEvent::Debug`].
//!
//! The DAP endpoint is the `java-debug` server that `jdt.ls` opens for
//! `vscode.java.startDebugSession`. Until the IDE manages a `jdt.ls` process itself, the
//! endpoint is taken from `KOVA_DAP_ADAPTER=host:port`; without it, a start reports a clear
//! error in the *Debugger* dock instead of hanging.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};

use kova_config::{ProjectBuildTool, ProjectConfig};
use kova_lsp_dap::bsp::{apply_submodule_scope, resolve_submodule_for_file};
use kova_lsp_dap::dap::{
    BreakpointStore, DapClient, DapScope, DapStackFrame, DapVariable, DebugEvent, DebugSession,
    EvaluateContext, JavaAttachConfig, JavaLaunchConfig, OutputCategory, SourceBreakpoint,
    StepKind, VariablesFilter, VariablesPage,
};
use kova_toolchain::{ToolchainEnvOptions, resolve_executable, resolve_toolchain_env};
use kova_tools::junit::{BuildTool, TestCommand, build_test_command};
use kova_ui::debugger::{
    BreakpointSpec, DebugCommand, DebugCommandSink, DebugLaunchRequest, DebugStep, DebugTargetKind,
    DebugUpdate, DebuggerStatus, EvaluateTarget, FrameItem, STACK_PAGE_SIZE, ThreadItem,
    VariableItem,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

use crate::AppEvent;

/// Environment variable naming a running `java-debug` DAP server (`host:port`).
pub const DAP_ADAPTER_ENV: &str = "KOVA_DAP_ADAPTER";
/// Default JDWP port opened by Gradle `--debug-jvm` and Maven `-Dmaven.surefire.debug`.
const DEFAULT_JDWP_PORT: u16 = 5005;

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

fn map_project_build_tool(project_root: &Path) -> Option<BuildTool> {
    let cfg = ProjectConfig::load_optional(project_root).ok().flatten()?;
    match cfg.build_tool? {
        ProjectBuildTool::Auto => None,
        ProjectBuildTool::Gradle => Some(BuildTool::Gradle),
        ProjectBuildTool::Maven => Some(BuildTool::Maven),
        ProjectBuildTool::Cargo => Some(BuildTool::Cargo),
    }
}

/// Plans the Gradle `--debug-jvm` or Maven `-Dmaven.surefire.debug` test command for debugging
/// a `JUnit` or `Spock` target.
fn plan_jvm_test_debug_command(
    project_root: &Path,
    target: &str,
    target_file: Option<&Path>,
) -> Result<TestCommand, String> {
    let tool = map_project_build_tool(project_root)
        .or_else(|| BuildTool::detect(project_root))
        .ok_or_else(|| "Debugging JVM tests requires a Gradle or Maven project".to_owned())?;
    if !matches!(tool, BuildTool::Gradle | BuildTool::Maven) {
        return Err("Debugging JVM tests requires a Gradle or Maven project".to_owned());
    }
    let targets = [target.to_owned()];
    let mut cmd = build_test_command(tool, project_root, &targets, &[], &BTreeMap::new());
    if let Some(submodule) = resolve_submodule_for_file(project_root, target_file) {
        apply_submodule_scope(&mut cmd, project_root, &submodule);
    }
    match tool {
        BuildTool::Gradle => {
            if let Some(pos) = cmd.args.iter().position(|a| a == "--console=plain") {
                cmd.args.insert(pos, "--debug-jvm".to_owned());
            } else {
                cmd.args.push("--debug-jvm".to_owned());
            }
        }
        BuildTool::Maven => {
            if let Some(pos) = cmd.args.iter().position(|a| a == "-B") {
                cmd.args.insert(pos, "-Dmaven.surefire.debug".to_owned());
            } else {
                cmd.args.push("-Dmaven.surefire.debug".to_owned());
            }
        }
        BuildTool::Cargo => {}
    }
    Ok(cmd)
}

fn spawn_test_for_jdwp(
    command: &TestCommand,
    reporter: &Reporter,
) -> Result<(Child, oneshot::Receiver<()>), String> {
    let toolchain = resolve_toolchain_env(&ToolchainEnvOptions {
        project_root: Some(command.working_dir.clone()),
        ..ToolchainEnvOptions::default()
    });
    let mut env_vars = toolchain.vars;
    for (k, v) in &command.env {
        env_vars.insert(k.clone(), v.clone());
    }
    let effective_path = env_vars
        .get("PATH")
        .map_or_else(OsString::new, OsString::from);
    let mut resolved_program = resolve_executable(&command.program, &effective_path);
    let mut resolved_args = command.args.clone();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&resolved_program)
            && meta.is_file()
            && meta.permissions().mode() & 0o111 == 0
        {
            let script = resolved_program.to_string_lossy().into_owned();
            resolved_program = resolve_executable(Path::new("sh"), &effective_path);
            resolved_args.insert(0, script);
        }
    }

    let mut child = Command::new(&resolved_program)
        .args(&resolved_args)
        .envs(&env_vars)
        .current_dir(&command.working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| format!("Failed to start `{}`: {err}", resolved_program.display()))?;

    let (ready_tx, ready_rx) = oneshot::channel();
    let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));

    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(pump_test_debug_stream(
            stdout,
            false,
            reporter.session_id,
            reporter.events.clone(),
            Arc::clone(&ready_tx),
        ));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(pump_test_debug_stream(
            stderr,
            true,
            reporter.session_id,
            reporter.events.clone(),
            ready_tx,
        ));
    }

    Ok((child, ready_rx))
}

async fn pump_test_debug_stream<R: AsyncRead + Unpin>(
    reader: R,
    is_stderr: bool,
    session_id: u64,
    events: mpsc::UnboundedSender<AppEvent>,
    ready_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.contains("Listening for transport dt_socket")
            && let Some(tx) = ready_tx
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        {
            tx.send(()).ok();
        }
        let formatted = if is_stderr {
            format!("[stderr] {line}\n")
        } else {
            format!("{line}\n")
        };
        drop(events.send(AppEvent::Debug {
            session_id,
            update: DebugUpdate::Output(formatted),
        }));
    }
}

/// Launches the Gradle/Maven test process with JDWP suspended and waits until it listens on
/// [`DEFAULT_JDWP_PORT`]. `Err(None)` means the user stopped the session before it was ready.
async fn start_jvm_test_debuggee(
    request: &DebugLaunchRequest,
    reporter: &Reporter,
    commands: &mut mpsc::UnboundedReceiver<DebugCommand>,
) -> Result<Child, Option<String>> {
    let project_root = request
        .project_root
        .as_deref()
        .ok_or_else(|| Some("Open a Gradle or Maven project to debug tests".to_owned()))?;
    let cmd = plan_jvm_test_debug_command(
        project_root,
        &request.main_class,
        request.target_file.as_deref(),
    )
    .map_err(Some)?;
    reporter.send(DebugUpdate::Output(format!(
        "Starting test debugger: {}\n",
        cmd.display()
    )));
    let (mut child, mut ready_rx) = spawn_test_for_jdwp(&cmd, reporter).map_err(Some)?;
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(120));
    tokio::pin!(timeout);
    let outcome = loop {
        tokio::select! {
            res = &mut ready_rx => break if res.is_ok() {
                Ok(())
            } else {
                Err(Some(format!(
                    "Test process closed output before JDWP became ready on port {DEFAULT_JDWP_PORT}"
                )))
            },
            status = child.wait() => break Err(Some(match status {
                Ok(exit) => format!(
                    "Test process exited ({exit}) before JDWP became ready on port {DEFAULT_JDWP_PORT}"
                ),
                Err(err) => format!("Test process failed before JDWP became ready: {err}"),
            })),
            cmd = commands.recv() => {
                if matches!(cmd, None | Some(DebugCommand::Stop)) {
                    break Err(None);
                }
            }
            () = &mut timeout => break Err(Some(format!(
                "Timed out waiting for JDWP listener on port {DEFAULT_JDWP_PORT}"
            ))),
        }
    };
    match outcome {
        Ok(()) => Ok(child),
        Err(message) => {
            drop(child.kill().await);
            Err(message)
        }
    }
}

/// Kills the Gradle/Maven test process backing a JVM test debug session, if any.
async fn kill_test_child(child: &mut Option<Child>) {
    if let Some(mut child) = child.take() {
        drop(child.kill().await);
    }
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

    let mut test_child: Option<Child> = None;
    let attach_port = if let Some(port) = request.attach_port {
        Some(port)
    } else if request.target_kind == DebugTargetKind::JvmTest {
        match start_jvm_test_debuggee(&request, &reporter, &mut commands).await {
            Ok(child) => test_child = Some(child),
            Err(message) => {
                reporter.terminated(message);
                return;
            }
        }
        Some(DEFAULT_JDWP_PORT)
    } else {
        None
    };

    let (client, adapter_events) = match DapClient::connect_tcp(&host, port).await {
        Ok(connected) => connected,
        Err(err) => {
            kill_test_child(&mut test_child).await;
            reporter.terminated(Some(err.to_string()));
            return;
        }
    };
    let mut session =
        DebugSession::new(client, adapter_events).with_breakpoints(store_of(&request.breakpoints));
    let target = match attach_port {
        Some(port) => JavaAttachConfig::localhost(port).target(),
        None => JavaLaunchConfig {
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
        kill_test_child(&mut test_child).await;
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
            kill_test_child(&mut test_child).await;
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
            target_kind: DebugTargetKind::MainClass,
            target_file: None,
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

        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_commands_tx, commands_rx) = mpsc::unbounded_channel();
        let test_request = DebugLaunchRequest {
            session_id: 4,
            label: "OrderSpec.creates".to_owned(),
            main_class: "com.acme.OrderSpec#creates".to_owned(),
            target_kind: DebugTargetKind::JvmTest,
            target_file: None,
            attach_port: None,
            project_root: None,
            breakpoints: BTreeMap::new(),
        };
        run_session(test_request, None, commands_rx, tx).await;
        let Some(AppEvent::Debug { session_id, update }) = rx.recv().await else {
            panic!("expected a debug update");
        };
        assert_eq!(session_id, 4);
        assert!(matches!(
            update,
            DebugUpdate::Status(DebuggerStatus::Terminated { message: Some(ref m) })
                if m.contains(DAP_ADAPTER_ENV)
        ));
    }

    #[test]
    fn plans_jvm_test_debug_commands_for_gradle_maven_and_submodules() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(plan_jvm_test_debug_command(tmp.path(), "a.B#c", None).is_err());

        std::fs::write(tmp.path().join("settings.gradle.kts"), "").unwrap();
        let sub = tmp.path().join("api");
        let spec = sub.join("src/test/groovy/com/kova/OrderSpec.groovy");
        std::fs::create_dir_all(spec.parent().unwrap()).unwrap();
        std::fs::write(sub.join("build.gradle.kts"), "").unwrap();
        std::fs::write(&spec, "").unwrap();

        let gradle = plan_jvm_test_debug_command(
            tmp.path(),
            "com.kova.OrderSpec#should cost $10 (#1)",
            Some(&spec),
        )
        .unwrap();
        assert_eq!(
            gradle.args,
            vec![
                ":api:test",
                "--tests",
                "com.kova.OrderSpec.should cost $10 (#1)",
                "--continue",
                "--debug-jvm",
                "--console=plain",
            ]
        );

        std::fs::write(tmp.path().join("pom.xml"), "<project/>").unwrap();
        std::fs::write(sub.join("pom.xml"), "<project/>").unwrap();
        std::fs::write(tmp.path().join("kova.toml"), "build_tool = \"maven\"\n").unwrap();
        let maven =
            plan_jvm_test_debug_command(tmp.path(), "com.kova.OrderSpec#should work", Some(&spec))
                .unwrap();
        assert_eq!(
            maven.args,
            vec![
                "test",
                "-pl",
                "api",
                "-Dtest=com.kova.OrderSpec#should work",
                "-Dsurefire.failIfNoSpecifiedTests=false",
                "-Dmaven.surefire.debug",
                "-B",
            ]
        );
    }
}
