//! Host side of the `JUnit` *Test Runner* (`KOVA-040`): executes [`TestRunRequest`]s from the
//! shell on the `tokio` runtime through `kova_lsp_dap::bsp::run_tests` (BSP, or Gradle / Maven
//! with the `TEST-*.xml` fallback) and reports event batches back as [`AppEvent`]s.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use kova_lsp_dap::bsp::{BspEvent, TestRunRequest as BspRunRequest, run_tests};
use kova_lsp_dap::lsp::lsp_uri_to_path;
use kova_tools::junit::{OutputStream, TestEvent};
use kova_ui::{ProblemEntry, ProblemSeverity, TestRunRequest, TestRunSink};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::AppEvent;

/// Maximum number of test events forwarded to the UI thread in one batch.
const MAX_BATCH: usize = 512;

/// Creates the [`TestRunSink`] installed on the shell.
///
/// Starting a run aborts the previous one (its build process is killed on drop).
#[must_use]
pub fn test_run_sink(runtime: Handle, events: mpsc::UnboundedSender<AppEvent>) -> TestRunSink {
    let current: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    Arc::new(move |request: TestRunRequest| {
        let events = events.clone();
        let task = runtime.spawn(execute(request, events));
        let previous = current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(task);
        if let Some(previous) = previous {
            previous.abort();
        }
    })
}

async fn execute(request: TestRunRequest, events: mpsc::UnboundedSender<AppEvent>) {
    let run_id = request.run_id;
    let Some(project_root) = request.project_root.clone() else {
        drop(events.send(AppEvent::TestEvents {
            run_id,
            events: vec![TestEvent::RunFinished {
                success: false,
                message: Some("Open a Gradle or Maven project to run tests".to_owned()),
            }],
        }));
        return;
    };
    let bsp_request = BspRunRequest {
        project_root: project_root.clone(),
        targets: request.targets,
        file: request.file,
        vm_options: request.vm_options,
        env: request.env,
        build_tool: None,
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let runner = tokio::spawn(async move {
        // Errors are reported to the run console through `RunFinished`.
        drop(run_tests(bsp_request, tx).await);
    });
    while let Some(first) = rx.recv().await {
        let mut batch = Vec::new();
        route(first, &project_root, &mut batch, &events);
        while batch.len() < MAX_BATCH {
            let Ok(next) = rx.try_recv() else {
                break;
            };
            route(next, &project_root, &mut batch, &events);
        }
        if !batch.is_empty()
            && events
                .send(AppEvent::TestEvents {
                    run_id,
                    events: batch,
                })
                .is_err()
        {
            runner.abort();
            return;
        }
    }
    drop(runner.await);
}

fn route(
    event: BspEvent,
    project_root: &Path,
    batch: &mut Vec<TestEvent>,
    events: &mpsc::UnboundedSender<AppEvent>,
) {
    match event {
        BspEvent::Test(event) => batch.push(event),
        BspEvent::Message { message, .. } => batch.push(console(message)),
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

/// Converts BSP compile diagnostics of `uri` into *Problems* entries with a project-relative
/// file name; hints and information messages are skipped.
#[must_use]
pub fn to_problems(
    uri: &str,
    diagnostics: &[lsp_types::Diagnostic],
    project_root: &Path,
) -> (String, Vec<ProblemEntry>) {
    let file = uri
        .parse::<lsp_types::Uri>()
        .ok()
        .and_then(|u| lsp_uri_to_path(&u))
        .map_or_else(
            || uri.to_owned(),
            |path| {
                path.strip_prefix(project_root)
                    .unwrap_or(&path)
                    .display()
                    .to_string()
            },
        );
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
mod tests {
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
    fn diagnostics_become_project_relative_problems() {
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
            env: std::collections::BTreeMap::new(),
        });
        let event = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), rx.recv()).await })
            .unwrap()
            .unwrap();
        let AppEvent::TestEvents { run_id, events } = event else {
            panic!("expected TestEvents");
        };
        assert_eq!(run_id, 7);
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
            env: std::collections::BTreeMap::new(),
        });
        let events = runtime.block_on(async {
            let mut all = Vec::new();
            while let Ok(Some(AppEvent::TestEvents { events, .. })) =
                tokio::time::timeout(Duration::from_secs(5), rx.recv()).await
            {
                let done = events
                    .iter()
                    .any(|e| matches!(e, TestEvent::RunFinished { .. }));
                all.extend(events);
                if done {
                    break;
                }
            }
            all
        });
        assert!(events.iter().any(|e| matches!(
            e,
            TestEvent::RunFinished { success: false, message: Some(m) } if m.contains("Gradle/Maven")
        )));
    }
}
