//! Host side of `Build Project` (`KOVA-058`) and the *Test Runner* (`KOVA-040`): executes
//! [`BuildRunRequest`]s and [`TestRunRequest`]s from the shell on the `tokio` runtime through
//! `kova_lsp_dap::bsp` (BSP, or Gradle / Maven / Cargo) and reports event batches back as
//! [`AppEvent`]s.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use kova_buffer::Buffer;
use kova_config::{ProjectBuildTool, ProjectConfig};
use kova_lsp_dap::bsp::{
    BspEvent, BspMessageLevel, BuildRequest as BspBuildRequest, TestRunRequest as BspRunRequest,
    run_build, run_tests,
};
use kova_lsp_dap::lsp::lsp_uri_to_path;
use kova_tools::junit::{BuildTool, CompilerOutputParser, OutputStream, TestEvent};
use kova_ui::{
    BuildEvent, BuildRunRequest, BuildRunSink, CancelRunKind, CancelRunSink, ProblemEntry,
    ProblemSeverity, TestRunRequest, TestRunSink,
};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::{AppEvent, io};

/// Maximum number of events forwarded to the UI thread in one batch.
const MAX_BATCH: usize = 512;

/// Aborts the inner runner task whenever the enclosing build/test future is aborted or dropped.
struct AbortOnDrop(Option<JoinHandle<()>>);

impl AbortOnDrop {
    fn new(handle: JoinHandle<()>) -> Self {
        Self(Some(handle))
    }

    async fn finish(mut self) {
        if let Some(handle) = self.0.take() {
            drop(handle.await);
        }
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// Creates coordinated [`TestRunSink`], [`BuildRunSink`], and [`CancelRunSink`] handles
/// backed by `runtime` and reporting into `events` (`KOVA-058`).
#[must_use]
pub fn run_sinks(
    runtime: Handle,
    events: mpsc::UnboundedSender<AppEvent>,
) -> (TestRunSink, BuildRunSink, CancelRunSink) {
    let active_test: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    let active_build: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));

    let test_sink: TestRunSink = {
        let runtime = runtime.clone();
        let events = events.clone();
        let active_test = Arc::clone(&active_test);
        Arc::new(move |request: TestRunRequest| {
            let task = runtime.spawn(execute_test(request, events.clone()));
            if let Some(previous) = active_test
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .replace(task)
            {
                previous.abort();
            }
        })
    };

    let build_sink: BuildRunSink = {
        let active_build = Arc::clone(&active_build);
        Arc::new(move |request: BuildRunRequest| {
            let task = runtime.spawn(execute_build(request, events.clone()));
            if let Some(previous) = active_build
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .replace(task)
            {
                previous.abort();
            }
        })
    };

    let cancel_sink: CancelRunSink = Arc::new(move |kind: CancelRunKind| {
        if matches!(kind, CancelRunKind::Build | CancelRunKind::All)
            && let Some(task) = active_build
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        {
            task.abort();
        }
        if matches!(kind, CancelRunKind::Test | CancelRunKind::All)
            && let Some(task) = active_test
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        {
            task.abort();
        }
    });

    (test_sink, build_sink, cancel_sink)
}

/// Creates a standalone [`TestRunSink`] installed on the shell.
///
/// Starting a run aborts the previous one (its build process is killed on drop).
#[must_use]
pub fn test_run_sink(runtime: Handle, events: mpsc::UnboundedSender<AppEvent>) -> TestRunSink {
    run_sinks(runtime, events).0
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

async fn flush_pending_saves(pending_saves: Vec<(PathBuf, String)>) {
    for (path, text) in pending_saves {
        let snapshot = Buffer::from_text(&text).snapshot();
        drop(io::atomic_write_snapshot(&path, snapshot).await);
    }
}

async fn execute_build(request: BuildRunRequest, events: mpsc::UnboundedSender<AppEvent>) {
    let run_id = request.run_id;
    drop(events.send(AppEvent::ClearBuildProblems));
    flush_pending_saves(request.pending_saves).await;

    let Some(project_root) = request.project_root.clone() else {
        drop(events.send(AppEvent::BuildEvents {
            run_id,
            events: vec![BuildEvent::Finished {
                success: false,
                duration_ms: 0,
                message: Some("Open a Gradle, Maven, or Cargo project to build".to_owned()),
            }],
        }));
        return;
    };

    let started = Instant::now();
    let build_tool = map_project_build_tool(&project_root);
    let bsp_request = BspBuildRequest {
        project_root: project_root.clone(),
        env: request.env,
        build_tool,
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let runner = AbortOnDrop::new(tokio::spawn(async move {
        drop(run_build(bsp_request, tx).await);
    }));
    let mut parser = CompilerOutputParser::new();

    while let Some(first) = rx.recv().await {
        let mut batch = Vec::new();
        route_build(
            first,
            &project_root,
            started,
            &mut parser,
            &mut batch,
            &events,
        );
        while batch.len() < MAX_BATCH {
            let Ok(next) = rx.try_recv() else {
                break;
            };
            route_build(
                next,
                &project_root,
                started,
                &mut parser,
                &mut batch,
                &events,
            );
        }
        if !batch.is_empty()
            && events
                .send(AppEvent::BuildEvents {
                    run_id,
                    events: batch,
                })
                .is_err()
        {
            return;
        }
    }
    runner.finish().await;
}

async fn execute_test(request: TestRunRequest, events: mpsc::UnboundedSender<AppEvent>) {
    let run_id = request.run_id;
    drop(events.send(AppEvent::ClearBuildProblems));
    flush_pending_saves(request.pending_saves).await;

    let Some(project_root) = request.project_root.clone() else {
        drop(events.send(AppEvent::TestEvents {
            run_id,
            events: vec![TestEvent::RunFinished {
                success: false,
                message: Some("Open a Gradle, Maven, or Cargo project to run tests".to_owned()),
            }],
        }));
        return;
    };
    let build_tool = map_project_build_tool(&project_root);
    let bsp_request = BspRunRequest {
        project_root: project_root.clone(),
        targets: request.targets,
        file: request.file,
        vm_options: request.vm_options,
        env: request.env,
        build_tool,
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let runner = AbortOnDrop::new(tokio::spawn(async move {
        // Errors are reported to the run console through `RunFinished`.
        drop(run_tests(bsp_request, tx).await);
    }));
    while let Some(first) = rx.recv().await {
        let mut batch = Vec::new();
        route_test(first, &project_root, &mut batch, &events);
        while batch.len() < MAX_BATCH {
            let Ok(next) = rx.try_recv() else {
                break;
            };
            route_test(next, &project_root, &mut batch, &events);
        }
        if !batch.is_empty()
            && events
                .send(AppEvent::TestEvents {
                    run_id,
                    events: batch,
                })
                .is_err()
        {
            return;
        }
    }
    runner.finish().await;
}

fn route_build(
    event: BspEvent,
    project_root: &Path,
    started: Instant,
    parser: &mut CompilerOutputParser,
    batch: &mut Vec<BuildEvent>,
    events: &mpsc::UnboundedSender<AppEvent>,
) {
    match event {
        BspEvent::Test(TestEvent::Output { stream, text, .. }) => {
            let target = parser.parse_line(&text).into_iter().next().map(|diag| {
                let rel = diag
                    .file
                    .strip_prefix(project_root)
                    .unwrap_or(&diag.file)
                    .display()
                    .to_string();
                (rel, diag.line, diag.col)
            });
            batch.push(BuildEvent::Output {
                stream,
                text,
                target,
            });
        }
        BspEvent::Test(TestEvent::RunFinished { success, message }) => {
            let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            batch.push(BuildEvent::Finished {
                success,
                duration_ms,
                message,
            });
        }
        BspEvent::Test(TestEvent::RunStarted { label }) => {
            batch.push(BuildEvent::Started { label });
        }
        BspEvent::Test(TestEvent::TestStarted(_) | TestEvent::TestFinished { .. })
        | BspEvent::Progress { .. } => {}
        BspEvent::Message { level, message } => {
            let stream = if level == BspMessageLevel::Error {
                OutputStream::Stderr
            } else {
                OutputStream::Stdout
            };
            batch.push(BuildEvent::Output {
                stream,
                text: message,
                target: None,
            });
        }
        BspEvent::CompileReport {
            errors, warnings, ..
        } => {
            let stream = if errors > 0 {
                OutputStream::Stderr
            } else {
                OutputStream::Stdout
            };
            batch.push(BuildEvent::Output {
                stream,
                text: format!("Compilation finished: {errors} error(s), {warnings} warning(s)"),
                target: None,
            });
        }
        BspEvent::Diagnostics {
            uri, diagnostics, ..
        } => {
            let (file, problems) = to_problems(&uri, &diagnostics, project_root);
            drop(events.send(AppEvent::BuildProblems { file, problems }));
        }
    }
}

fn route_test(
    event: BspEvent,
    project_root: &Path,
    batch: &mut Vec<TestEvent>,
    events: &mpsc::UnboundedSender<AppEvent>,
) {
    match event {
        BspEvent::Test(event) => batch.push(event),
        BspEvent::Message { level, message } => batch.push(TestEvent::Output {
            id: None,
            stream: if level == BspMessageLevel::Error {
                OutputStream::Stderr
            } else {
                OutputStream::Stdout
            },
            text: message,
        }),
        BspEvent::CompileReport {
            errors, warnings, ..
        } => batch.push(console(format!(
            "Compilation finished: {errors} error(s), {warnings} warning(s)"
        ))),
        BspEvent::Diagnostics {
            uri, diagnostics, ..
        } => {
            let (file, problems) = to_problems(&uri, &diagnostics, project_root);
            drop(events.send(AppEvent::BuildProblems { file, problems }));
        }
        BspEvent::Progress { .. } => {}
    }
}

fn console(text: String) -> TestEvent {
    TestEvent::Output {
        id: None,
        stream: OutputStream::Stdout,
        text,
    }
}

/// Converts BSP or compiler diagnostics of `uri` (either `file://` URI or raw path) into
/// *Problems* entries with a project-relative file name; hints and information messages are skipped.
#[must_use]
pub fn to_problems(
    uri: &str,
    diagnostics: &[lsp_types::Diagnostic],
    project_root: &Path,
) -> (String, Vec<ProblemEntry>) {
    let raw_path = uri
        .parse::<lsp_types::Uri>()
        .ok()
        .and_then(|u| lsp_uri_to_path(&u))
        .unwrap_or_else(|| PathBuf::from(uri));
    let file = raw_path
        .strip_prefix(project_root)
        .unwrap_or(&raw_path)
        .display()
        .to_string();
    let problems = diagnostics
        .iter()
        .filter_map(|d| {
            let severity = match d.severity {
                Some(lsp_types::DiagnosticSeverity::ERROR) | None => ProblemSeverity::Error,
                Some(lsp_types::DiagnosticSeverity::WARNING) => ProblemSeverity::Warning,
                Some(_) => return None,
            };
            Some(ProblemEntry {
                file: file.clone(),
                line: usize::try_from(d.range.start.line).unwrap_or(0) + 1,
                col: usize::try_from(d.range.start.character).unwrap_or(0) + 1,
                severity,
                message: d.message.clone(),
            })
        })
        .collect();
    (file, problems)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests assert host build/test routing and cancellation"
)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use super::*;

    fn diagnostic(
        severity: Option<lsp_types::DiagnosticSeverity>,
        message: &str,
    ) -> lsp_types::Diagnostic {
        lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 4,
                    character: 2,
                },
                end: lsp_types::Position {
                    line: 4,
                    character: 6,
                },
            },
            severity,
            message: message.to_owned(),
            ..lsp_types::Diagnostic::default()
        }
    }

    #[test]
    fn diagnostics_become_project_relative_problems_for_uris_and_raw_paths() {
        let root = std::env::temp_dir().join("kova-proj");
        let uri = kova_lsp_dap::lsp::path_to_lsp_uri(&root.join("src/main/java/A.java")).unwrap();
        let (file, problems) = to_problems(
            uri.as_str(),
            &[
                diagnostic(
                    Some(lsp_types::DiagnosticSeverity::ERROR),
                    "cannot find symbol",
                ),
                diagnostic(Some(lsp_types::DiagnosticSeverity::WARNING), "deprecated"),
                diagnostic(Some(lsp_types::DiagnosticSeverity::HINT), "hint"),
            ],
            &root,
        );
        assert_eq!(Path::new(&file), Path::new("src/main/java/A.java"));
        assert_eq!(problems.len(), 2);
        assert_eq!((problems[0].line, problems[0].col), (5, 3));
        assert_eq!(problems[1].severity, ProblemSeverity::Warning);

        // Raw absolute path (e.g. non-URI fallback) is also normalized relative to project_root.
        let (raw_file, raw_problems) = to_problems(
            &root.join("src/main.rs").display().to_string(),
            &[diagnostic(
                Some(lsp_types::DiagnosticSeverity::ERROR),
                "mismatched types",
            )],
            &root,
        );
        assert_eq!(Path::new(&raw_file), Path::new("src/main.rs"));
        assert_eq!(raw_problems.len(), 1);
    }

    #[test]
    fn requests_without_a_project_finish_immediately() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = test_run_sink(runtime.handle().clone(), tx);
        sink(TestRunRequest {
            run_id: 7,
            label: "T".to_owned(),
            targets: vec!["a.T#m".to_owned()],
            project_root: None,
            file: None,
            vm_options: Vec::new(),
            env: BTreeMap::new(),
            pending_saves: Vec::new(),
        });
        let events = runtime.block_on(async {
            let mut collected = Vec::new();
            while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                if let AppEvent::TestEvents { run_id, events } = ev {
                    assert_eq!(run_id, 7);
                    collected.extend(events);
                    break;
                }
            }
            collected
        });
        assert!(matches!(
            events[0],
            TestEvent::RunFinished { success: false, .. }
        ));
    }

    #[test]
    fn projects_without_build_report_a_failed_run() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = test_run_sink(runtime.handle().clone(), tx);
        sink(TestRunRequest {
            run_id: 1,
            label: "T".to_owned(),
            targets: Vec::new(),
            project_root: Some(tmp.path().to_path_buf()),
            file: None,
            vm_options: Vec::new(),
            env: BTreeMap::new(),
            pending_saves: Vec::new(),
        });
        let events = runtime.block_on(async {
            let mut all = Vec::new();
            while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                if let AppEvent::TestEvents { events, .. } = ev {
                    let done = events
                        .iter()
                        .any(|e| matches!(e, TestEvent::RunFinished { .. }));
                    all.extend(events);
                    if done {
                        break;
                    }
                }
            }
            all
        });
        assert!(events.iter().any(|e| matches!(
            e,
            TestEvent::RunFinished { success: false, message: Some(m) } if m.contains("Gradle/Maven/Cargo")
        )));
    }

    #[test]
    fn build_sink_clears_problems_flushes_pending_saves_and_finishes() {
        let tmp = tempfile::tempdir().unwrap();
        let target_file = tmp.path().join("src/lib.rs");
        std::fs::create_dir_all(target_file.parent().unwrap()).unwrap();
        std::fs::write(&target_file, "old content").unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_, build_sink, _) = run_sinks(runtime.handle().clone(), tx);
        build_sink(BuildRunRequest {
            run_id: 9,
            label: "Build".to_owned(),
            project_root: Some(tmp.path().to_path_buf()),
            env: BTreeMap::new(),
            pending_saves: vec![(target_file.clone(), "new content".to_owned())],
        });

        let mut saw_clear = false;
        let mut saw_finished = false;
        runtime.block_on(async {
            while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                match ev {
                    AppEvent::ClearBuildProblems => saw_clear = true,
                    AppEvent::BuildEvents { run_id, events } => {
                        assert_eq!(run_id, 9);
                        if events
                            .iter()
                            .any(|e| matches!(e, BuildEvent::Finished { success: false, .. }))
                        {
                            saw_finished = true;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });
        assert!(saw_clear);
        assert!(saw_finished);
        assert_eq!(
            std::fs::read_to_string(&target_file).unwrap(),
            "new content"
        );
    }

    #[test]
    fn build_sink_without_project_finishes_with_message() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_, build_sink, _) = run_sinks(runtime.handle().clone(), tx);
        build_sink(BuildRunRequest {
            run_id: 3,
            label: "Build".to_owned(),
            project_root: None,
            env: BTreeMap::new(),
            pending_saves: Vec::new(),
        });

        let mut saw_clear = false;
        let mut saw_finished = false;
        runtime.block_on(async {
            while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                match ev {
                    AppEvent::ClearBuildProblems => saw_clear = true,
                    AppEvent::BuildEvents { run_id, events } => {
                        assert_eq!(run_id, 3);
                        if events.iter().any(|e| {
                            matches!(
                                e,
                                BuildEvent::Finished {
                                    success: false,
                                    message: Some(msg),
                                    ..
                                } if msg.contains("Gradle, Maven, or Cargo")
                            )
                        }) {
                            saw_finished = true;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });
        assert!(saw_clear);
        assert!(saw_finished);
    }

    #[test]
    fn route_build_and_route_test_map_bsp_messages_and_compile_reports() {
        let root = std::env::temp_dir().join("kova-route-test");
        let started = Instant::now();
        let mut parser = CompilerOutputParser::new();
        let (tx, mut rx) = mpsc::unbounded_channel();

        let mut build_batch = Vec::new();
        route_build(
            BspEvent::Test(TestEvent::RunStarted {
                label: "Build".to_owned(),
            }),
            &root,
            started,
            &mut parser,
            &mut build_batch,
            &tx,
        );
        route_build(
            BspEvent::Message {
                level: BspMessageLevel::Error,
                message: "fatal compiler error".to_owned(),
            },
            &root,
            started,
            &mut parser,
            &mut build_batch,
            &tx,
        );
        route_build(
            BspEvent::CompileReport {
                target: None,
                errors: 1,
                warnings: 2,
            },
            &root,
            started,
            &mut parser,
            &mut build_batch,
            &tx,
        );
        assert!(matches!(
            &build_batch[0],
            BuildEvent::Started { label } if label == "Build"
        ));
        assert!(matches!(
            &build_batch[1],
            BuildEvent::Output {
                stream: OutputStream::Stderr,
                text,
                target: None,
            } if text == "fatal compiler error"
        ));
        assert!(matches!(
            &build_batch[2],
            BuildEvent::Output {
                stream: OutputStream::Stderr,
                text,
                target: None,
            } if text.contains("1 error(s), 2 warning(s)")
        ));

        let mut test_batch = Vec::new();
        route_test(
            BspEvent::Message {
                level: BspMessageLevel::Info,
                message: "starting suite".to_owned(),
            },
            &root,
            &mut test_batch,
            &tx,
        );
        route_test(
            BspEvent::CompileReport {
                target: None,
                errors: 0,
                warnings: 1,
            },
            &root,
            &mut test_batch,
            &tx,
        );
        assert!(matches!(
            &test_batch[0],
            TestEvent::Output {
                stream: OutputStream::Stdout,
                text,
                ..
            } if text == "starting suite"
        ));
        assert!(matches!(
            &test_batch[1],
            TestEvent::Output {
                stream: OutputStream::Stdout,
                text,
                ..
            } if text.contains("0 error(s), 1 warning(s)")
        ));
        assert!(rx.try_recv().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn build_sink_streams_compiler_targets_and_build_problems() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let src_file = tmp.path().join("src/main/java/App.java");
        std::fs::create_dir_all(src_file.parent().unwrap()).unwrap();
        std::fs::write(&src_file, "class App {}\n").unwrap();
        std::fs::write(tmp.path().join("build.gradle"), "").unwrap();

        let gradlew = tmp.path().join("gradlew");
        let script = format!(
            "#!/bin/sh\necho \"Compiling\"\necho \"{}:10:5: error: ';' expected\" >&2\nexit 1\n",
            src_file.display()
        );
        std::fs::write(&gradlew, script).unwrap();
        std::fs::set_permissions(&gradlew, std::fs::Permissions::from_mode(0o755)).unwrap();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_, build_sink, _) = run_sinks(runtime.handle().clone(), tx);
        build_sink(BuildRunRequest {
            run_id: 42,
            label: "Build Gradle".to_owned(),
            project_root: Some(tmp.path().to_path_buf()),
            env: BTreeMap::new(),
            pending_saves: Vec::new(),
        });

        let mut saw_target = false;
        let mut saw_problem = false;
        let mut saw_finished = false;
        runtime.block_on(async {
            while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                match ev {
                    AppEvent::BuildProblems { file, problems } => {
                        if Path::new(&file) == Path::new("src/main/java/App.java")
                            && problems.iter().any(|p| {
                                p.line == 10
                                    && p.col == 5
                                    && p.severity == ProblemSeverity::Error
                                    && p.message == "';' expected"
                            })
                        {
                            saw_problem = true;
                        }
                    }
                    AppEvent::BuildEvents { run_id, events } => {
                        assert_eq!(run_id, 42);
                        for event in events {
                            match event {
                                BuildEvent::Output {
                                    target: Some((rel, line, col)),
                                    ..
                                } if Path::new(&rel) == Path::new("src/main/java/App.java")
                                    && line == 10
                                    && col == 5 =>
                                {
                                    saw_target = true;
                                }
                                BuildEvent::Finished { success: false, .. } => {
                                    saw_finished = true;
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                if saw_finished && saw_target && saw_problem {
                    break;
                }
            }
        });
        assert!(
            saw_target,
            "expected clickable target in BuildEvent::Output"
        );
        assert!(saw_problem, "expected BuildProblems entry from compiler");
        assert!(saw_finished, "expected BuildEvent::Finished");
    }

    #[cfg(unix)]
    #[test]
    fn test_sink_streams_cargo_test_events_and_flushes_pending_saves() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let src_file = tmp.path().join("src/lib.rs");
        std::fs::create_dir_all(src_file.parent().unwrap()).unwrap();
        std::fs::write(&src_file, "stale").unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .unwrap();

        let bin_dir = tmp.path().join("fake_bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let fake_cargo = bin_dir.join("cargo");
        let script = r#"#!/bin/sh
printf '%s\n' "Running unittests src/lib.rs (target/debug/deps/demo-1)"
printf '%s\n' "test api::tests::passes ... ok"
printf '%s\n' "test api::tests::fails ... FAILED"
printf '%s\n' "---- api::tests::fails stdout ----"
printf '%s\n' "thread 'api::tests::fails' panicked at src/lib.rs:4:5:"
printf '%s\n' "boom"
printf '%s\n' "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out"
exit 1
"#;
        std::fs::write(&fake_cargo, script).unwrap();
        std::fs::set_permissions(&fake_cargo, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut env = BTreeMap::new();
        env.insert("PATH".to_owned(), bin_dir.display().to_string());

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (test_sink, _, _) = run_sinks(runtime.handle().clone(), tx);
        test_sink(TestRunRequest {
            run_id: 11,
            label: "Cargo Tests".to_owned(),
            targets: Vec::new(),
            project_root: Some(tmp.path().to_path_buf()),
            file: None,
            vm_options: Vec::new(),
            env,
            pending_saves: vec![(src_file.clone(), "saved_before_test".to_owned())],
        });

        let mut saw_clear = false;
        let mut saw_passed = false;
        let mut saw_failed = false;
        let mut saw_finished = false;
        runtime.block_on(async {
            while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                match ev {
                    AppEvent::ClearBuildProblems => saw_clear = true,
                    AppEvent::TestEvents { run_id, events } => {
                        assert_eq!(run_id, 11);
                        for event in events {
                            match event {
                                TestEvent::TestFinished { id, outcome }
                                    if id.method.as_deref() == Some("passes")
                                        && outcome.status
                                            == kova_tools::junit::TestStatus::Passed =>
                                {
                                    saw_passed = true;
                                }
                                TestEvent::TestFinished { id, outcome }
                                    if id.method.as_deref() == Some("fails")
                                        && outcome.status
                                            == kova_tools::junit::TestStatus::Failed =>
                                {
                                    saw_failed = true;
                                }
                                TestEvent::RunFinished { success: false, .. } => {
                                    saw_finished = true;
                                }
                                _ => {}
                            }
                        }
                        if saw_finished {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });
        assert!(saw_clear);
        assert!(saw_passed);
        assert!(saw_failed);
        assert!(saw_finished);
        assert_eq!(
            std::fs::read_to_string(&src_file).unwrap(),
            "saved_before_test"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancel_sink_aborts_active_build_and_test_subprocesses() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("build.gradle"), "").unwrap();
        let marker = tmp.path().join("started.marker");
        let gradlew = tmp.path().join("gradlew");
        let script = format!(
            "#!/bin/sh\necho started > \"{}\"\necho \"running\"\nsleep 30\n",
            marker.display()
        );
        std::fs::write(&gradlew, script).unwrap();
        std::fs::set_permissions(&gradlew, std::fs::Permissions::from_mode(0o755)).unwrap();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (test_sink, build_sink, cancel_sink) = run_sinks(runtime.handle().clone(), tx);

        build_sink(BuildRunRequest {
            run_id: 1,
            label: "Build".to_owned(),
            project_root: Some(tmp.path().to_path_buf()),
            env: BTreeMap::new(),
            pending_saves: Vec::new(),
        });

        runtime.block_on(async {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !marker.exists() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(marker.exists(), "build subprocess never started");
            cancel_sink(CancelRunKind::Build);
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        let mut build_finished = false;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::BuildEvents { events, .. } = ev
                && events
                    .iter()
                    .any(|e| matches!(e, BuildEvent::Finished { .. }))
            {
                build_finished = true;
            }
        }
        assert!(
            !build_finished,
            "cancelled build must abort before emitting BuildEvent::Finished"
        );

        std::fs::remove_file(&marker).unwrap();
        test_sink(TestRunRequest {
            run_id: 2,
            label: "Tests".to_owned(),
            targets: Vec::new(),
            project_root: Some(tmp.path().to_path_buf()),
            file: None,
            vm_options: Vec::new(),
            env: BTreeMap::new(),
            pending_saves: Vec::new(),
        });

        runtime.block_on(async {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !marker.exists() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(marker.exists(), "test subprocess never started");
            cancel_sink(CancelRunKind::All);
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        let mut test_finished = false;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::TestEvents { events, .. } = ev
                && events
                    .iter()
                    .any(|e| matches!(e, TestEvent::RunFinished { .. }))
            {
                test_finished = true;
            }
        }
        assert!(
            !test_finished,
            "cancelled test run must abort before emitting TestEvent::RunFinished"
        );
    }
}
