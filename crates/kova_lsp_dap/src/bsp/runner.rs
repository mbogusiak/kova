//! Test run orchestration (`KOVA-040`): BSP when the project has a `.bsp/*.json` connection,
//! otherwise the Gradle / Maven command line; in both cases `TEST-*.xml` reports are read when
//! the server did not stream per-test notifications.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::SystemTime;

use kova_tools::junit::{
    BuildTool, OutputStream, TestCommand, TestEvent, TestId, build_test_command, collect_reports,
    report_dirs,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::bsp::client::{BspClient, BspConnectionDetails, BspTestParams, select_test_targets};
use crate::bsp::events::BspEvent;
use crate::error::LspDapError;
use crate::lsp::sync::path_to_lsp_uri;

/// A request to run `JUnit` targets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestRunRequest {
    /// Project root.
    pub project_root: PathBuf,
    /// `Class` / `Class#method` targets (empty = every test).
    pub targets: Vec<String>,
    /// Absolute path of the file the run was started from (selects the BSP target).
    pub file: Option<PathBuf>,
    /// Extra JVM options.
    pub vm_options: Vec<String>,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
    /// Build tool forced by `kova.toml` (`None` = auto-detect).
    pub build_tool: Option<BuildTool>,
}

/// How a run is executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestBackend {
    /// Build Server Protocol.
    Bsp(BspConnectionDetails),
    /// Build tool command line + `TEST-*.xml` reports.
    Cli(TestCommand),
}

impl TestBackend {
    /// Short description for the run console.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Bsp(details) => format!("BSP server `{}`", details.name),
            Self::Cli(cmd) => format!("{}: {}", cmd.tool.label(), cmd.display()),
        }
    }
}

/// Chooses the backend for `request` (BSP connection first, then the detected build tool).
#[must_use]
pub fn plan_test_backend(request: &TestRunRequest) -> Option<TestBackend> {
    if let Some(details) = BspConnectionDetails::discover_jvm(&request.project_root) {
        return Some(TestBackend::Bsp(details));
    }
    let tool = request
        .build_tool
        .or_else(|| BuildTool::detect(&request.project_root))?;
    let mut command = build_test_command(
        tool,
        &request.project_root,
        &request.targets,
        &request.vm_options,
        &request.env,
    );
    command.report_dirs = module_report_dirs(&request.project_root);
    Some(TestBackend::Cli(command))
}

/// Report directories of the root project and its first-level modules.
#[must_use]
pub fn module_report_dirs(project_root: &Path) -> Vec<PathBuf> {
    let mut dirs = report_dirs(project_root);
    if let Ok(entries) = std::fs::read_dir(project_root) {
        let mut modules: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && ["build.gradle", "build.gradle.kts", "pom.xml"]
                        .iter()
                        .any(|f| p.join(f).is_file())
            })
            .collect();
        modules.sort();
        for module in modules {
            dirs.extend(report_dirs(&module));
        }
    }
    dirs
}

/// Runs `request`, streaming [`BspEvent`]s (test tree updates, diagnostics, messages) to
/// `sink`. Always ends with a `TestEvent::RunFinished`. Returns whether the run succeeded.
///
/// # Errors
/// [`LspDapError::InvalidArgument`] when the project has neither a BSP connection nor a
/// Gradle / Maven build, or spawn / protocol errors (a `RunFinished { success: false }` is
/// still emitted).
pub async fn run_tests(
    request: TestRunRequest,
    sink: mpsc::UnboundedSender<BspEvent>,
) -> Result<bool, LspDapError> {
    let Some(backend) = plan_test_backend(&request) else {
        let err = LspDapError::InvalidArgument {
            reason: format!(
                "no BSP connection (.bsp/*.json) or Gradle/Maven build found in {}",
                request.project_root.display()
            ),
        };
        finish(&sink, false, Some(err.to_string()));
        return Err(err);
    };
    run_with_backend(request, backend, sink).await
}

/// Runs `request` with an explicit `backend`.
///
/// # Errors
/// See [`run_tests`].
pub async fn run_with_backend(
    request: TestRunRequest,
    backend: TestBackend,
    sink: mpsc::UnboundedSender<BspEvent>,
) -> Result<bool, LspDapError> {
    let started = SystemTime::now();
    console(&sink, format!("Running tests via {}", backend.describe()));
    let result = match &backend {
        TestBackend::Bsp(details) => {
            match BspClient::connect(details, &request.project_root).await {
                Ok(client) => run_over_bsp(&client, &request, &sink, started).await,
                Err(err) => Err(err),
            }
        }
        TestBackend::Cli(command) => run_cli(command, &sink, started).await,
    };
    match result {
        Ok(success) => {
            finish(&sink, success, None);
            Ok(success)
        }
        Err(err) => {
            finish(&sink, false, Some(err.to_string()));
            Err(err)
        }
    }
}

/// Runs the tests of `request` on an already connected `client` (without `RunFinished`).
///
/// # Errors
/// Protocol errors of `workspace/buildTargets` or `buildTarget/test`.
pub async fn run_over_bsp(
    client: &BspClient,
    request: &TestRunRequest,
    sink: &mpsc::UnboundedSender<BspEvent>,
    started: SystemTime,
) -> Result<bool, LspDapError> {
    let mut events = client.take_events();
    let targets = client.build_targets().await?;
    let file_uri = match &request.file {
        Some(file) => Some(path_to_lsp_uri(file)?.as_str().to_owned()),
        None => None,
    };
    let selected = select_test_targets(&targets, file_uri.as_deref());
    if selected.is_empty() {
        return Err(LspDapError::InvalidArgument {
            reason: format!(
                "BSP server `{}` exposes no testable build target",
                client.server_name()
            ),
        });
    }
    let params = BspTestParams {
        targets: selected,
        selection: request
            .targets
            .iter()
            .map(|t| TestId::from_target(t))
            .collect(),
        jvm_options: request.vm_options.clone(),
        env: request.env.clone(),
        working_dir: Some(request.project_root.clone()),
    };
    let mut saw_tests = false;
    let forward = |event: BspEvent, saw_tests: &mut bool| {
        if matches!(
            event,
            BspEvent::Test(TestEvent::TestStarted(_) | TestEvent::TestFinished { .. })
        ) {
            *saw_tests = true;
        }
        drop(sink.send(event));
    };
    let test = client.test(&params);
    tokio::pin!(test);
    let status = loop {
        let Some(rx) = events.as_mut() else {
            break test.await?;
        };
        tokio::select! {
            result = &mut test => break result?,
            event = rx.recv() => match event {
                Some(event) => forward(event, &mut saw_tests),
                None => events = None,
            },
        }
    };
    // Notifications delivered before the response are already queued.
    if let Some(rx) = events.as_mut() {
        while let Ok(event) = rx.try_recv() {
            forward(event, &mut saw_tests);
        }
    }
    if !saw_tests {
        console(
            sink,
            "BSP server sent no per-test notifications; reading TEST-*.xml reports".to_owned(),
        );
        emit_reports(&module_report_dirs(&request.project_root), sink, started);
    }
    drop(client.shutdown().await);
    Ok(status.is_ok())
}

async fn run_cli(
    command: &TestCommand,
    sink: &mpsc::UnboundedSender<BspEvent>,
    started: SystemTime,
) -> Result<bool, LspDapError> {
    let program = command.program.display().to_string();
    let mut child = Command::new(&command.program)
        .args(&command.args)
        .envs(&command.env)
        .current_dir(&command.working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| LspDapError::SpawnFailed {
            program: program.clone(),
            reason: err.to_string(),
        })?;
    let stdout = child
        .stdout
        .take()
        .map(|s| tokio::spawn(pump_lines(s, OutputStream::Stdout, sink.clone())));
    let stderr = child
        .stderr
        .take()
        .map(|s| tokio::spawn(pump_lines(s, OutputStream::Stderr, sink.clone())));
    let status = child
        .wait()
        .await
        .map_err(|err| LspDapError::io(program, &err))?;
    for pump in [stdout, stderr].into_iter().flatten() {
        drop(pump.await);
    }
    emit_reports(&command.report_dirs, sink, started);
    Ok(status.success())
}

async fn pump_lines<R: AsyncRead + Unpin>(
    reader: R,
    stream: OutputStream,
    sink: mpsc::UnboundedSender<BspEvent>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        drop(sink.send(BspEvent::Test(TestEvent::Output {
            id: None,
            stream,
            text: line,
        })));
    }
}

fn emit_reports(dirs: &[PathBuf], sink: &mpsc::UnboundedSender<BspEvent>, since: SystemTime) {
    // File systems with coarse timestamps may round the report mtime down.
    let since = since
        .checked_sub(std::time::Duration::from_secs(2))
        .unwrap_or(since);
    let (events, errors) = collect_reports(dirs, Some(since));
    for event in events {
        drop(sink.send(BspEvent::Test(event)));
    }
    for error in errors {
        console(sink, error.to_string());
    }
}

fn console(sink: &mpsc::UnboundedSender<BspEvent>, text: String) {
    drop(sink.send(BspEvent::Test(TestEvent::Output {
        id: None,
        stream: OutputStream::Stdout,
        text,
    })));
}

fn finish(sink: &mpsc::UnboundedSender<BspEvent>, success: bool, message: Option<String>) {
    drop(sink.send(BspEvent::Test(TestEvent::RunFinished { success, message })));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_bsp_before_cli_and_none_without_build() {
        let tmp = tempfile::tempdir().unwrap();
        let mut request = TestRunRequest {
            project_root: tmp.path().to_path_buf(),
            targets: vec!["a.T#m".to_owned()],
            ..TestRunRequest::default()
        };
        assert_eq!(plan_test_backend(&request), None);

        std::fs::write(tmp.path().join("pom.xml"), "<project/>").unwrap();
        std::fs::create_dir_all(tmp.path().join("core")).unwrap();
        std::fs::write(tmp.path().join("core/pom.xml"), "<project/>").unwrap();
        let Some(TestBackend::Cli(cmd)) = plan_test_backend(&request) else {
            panic!("expected Maven CLI backend");
        };
        assert_eq!(cmd.tool, BuildTool::Maven);
        assert!(cmd.args.contains(&"-Dtest=a.T#m".to_owned()));
        assert!(
            cmd.report_dirs
                .contains(&tmp.path().join("core/target/surefire-reports"))
        );

        request.build_tool = Some(BuildTool::Gradle);
        let Some(TestBackend::Cli(cmd)) = plan_test_backend(&request) else {
            panic!("expected Gradle CLI backend");
        };
        assert_eq!(cmd.tool, BuildTool::Gradle);

        std::fs::create_dir_all(tmp.path().join(".bsp")).unwrap();
        std::fs::write(
            tmp.path().join(".bsp/gradle.json"),
            r#"{"name":"gradle-bsp","argv":["gradle-bsp"],"languages":["java"]}"#,
        )
        .unwrap();
        assert!(matches!(
            plan_test_backend(&request),
            Some(TestBackend::Bsp(_))
        ));
    }
}
