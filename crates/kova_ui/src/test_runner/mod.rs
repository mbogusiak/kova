//! `JUnit` *Test Runner* bottom dock (`KOVA-040`).
//!
//! [`TestRunnerModel`] is the headless state behind the panel: the current
//! [`kova_tools::junit::TestRun`], the request that started it (for *Rerun All* /
//! *Rerun Failed*) and stack-frame resolution to project files. [`test_runner_panel`] renders it
//! with GPUI following `junit_debugger_dock.svg` (*Kova Islands Dark*).
//!
//! Test execution itself happens in the host (`kova_app` → `kova_lsp_dap::bsp`): the shell
//! hands a [`TestRunRequest`] to a [`TestRunSink`] and receives `TestEvent` batches back.

pub mod test_runner_panel;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kova_tools::junit::{
    OutputKind, OutputLine, StackFrame, TestEvent, TestFilter, TestRun, TestStatus,
};

pub use test_runner_panel::{TEST_ROW_HEIGHT_PX, TestRunnerColors};

/// A request to run `JUnit` targets, handed to the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestRunRequest {
    /// Identifier echoed by the host with every event batch.
    pub run_id: u64,
    /// Tool window title (`OrderServiceTest.shouldCreate`).
    pub label: String,
    /// `Class` / `Class#method` targets (empty = all tests).
    pub targets: Vec<String>,
    /// Project root, when a real project is open.
    pub project_root: Option<PathBuf>,
    /// Absolute path of the file the run started from.
    pub file: Option<PathBuf>,
    /// Extra JVM options (from a `kova.toml` run configuration).
    pub vm_options: Vec<String>,
    /// Extra environment variables (from a `kova.toml` run configuration).
    pub env: BTreeMap<String, String>,
}

/// Host callback executing a [`TestRunRequest`] off the UI thread.
pub type TestRunSink = Arc<dyn Fn(TestRunRequest) + Send + Sync>;

/// User interaction inside the Test Runner panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestRunnerAction {
    /// Rerun every target of the last run.
    RerunAll,
    /// Rerun failed tests only.
    RerunFailed,
    /// Toggle *Show Passed*.
    ToggleShowPassed,
    /// Toggle *Show Failed*.
    ToggleShowFailed,
    /// Select the tree row.
    SelectRow(usize),
    /// Expand or collapse the tree row.
    ToggleRow(usize),
    /// Jump to the source of the frame on output line `usize`.
    OpenFrame(usize),
    /// Save the current targets as a `[[run_configurations]]` entry in `kova.toml`.
    SaveConfiguration,
}

/// Where a clicked stack frame points to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameTarget {
    /// Project-relative path.
    pub path: PathBuf,
    /// 1-based line.
    pub line: usize,
}

/// Headless state of the Test Runner dock.
#[derive(Debug, Default)]
pub struct TestRunnerModel {
    run: Option<TestRun>,
    request: Option<TestRunRequest>,
    next_run_id: u64,
    output: Vec<OutputLine>,
    output_targets: Vec<Option<FrameTarget>>,
    files: Vec<PathBuf>,
}

impl TestRunnerModel {
    /// Current run, if any.
    #[must_use]
    pub const fn run(&self) -> Option<&TestRun> {
        self.run.as_ref()
    }

    /// Request of the current run.
    #[must_use]
    pub const fn request(&self) -> Option<&TestRunRequest> {
        self.request.as_ref()
    }

    /// Output pane lines of the selected node (or the run console).
    #[must_use]
    pub fn output(&self) -> &[OutputLine] {
        &self.output
    }

    /// Jump targets of the output lines (`Some` for frames inside the project).
    #[must_use]
    pub fn output_targets(&self) -> &[Option<FrameTarget>] {
        &self.output_targets
    }

    /// Sets the project-relative source files used to resolve stack frames.
    pub fn set_project_files(&mut self, files: Vec<PathBuf>) {
        self.files = files;
        self.refresh_output();
    }

    /// Builds a request with a fresh run id.
    pub fn new_request(
        &mut self,
        label: impl Into<String>,
        targets: Vec<String>,
        project_root: Option<PathBuf>,
        file: Option<PathBuf>,
    ) -> TestRunRequest {
        self.next_run_id += 1;
        TestRunRequest {
            run_id: self.next_run_id,
            label: label.into(),
            targets,
            project_root,
            file,
            vm_options: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    /// Starts showing `request` (statuses arrive through [`Self::apply`]).
    pub fn start(&mut self, request: TestRunRequest) {
        self.next_run_id = self.next_run_id.max(request.run_id);
        let mut run = TestRun::new(request.label.clone(), request.targets.clone());
        run.apply_batch([TestEvent::RunStarted {
            label: request.label.clone(),
        }]);
        self.run = Some(run);
        self.request = Some(request);
        self.refresh_output();
    }

    /// Applies a batch of events for `run_id`; stale batches of older runs are ignored.
    ///
    /// Returns whether the model changed.
    pub fn apply(&mut self, run_id: u64, events: Vec<TestEvent>) -> bool {
        if self.request.as_ref().map(|r| r.run_id) != Some(run_id) {
            return false;
        }
        let Some(run) = self.run.as_mut() else {
            return false;
        };
        let finished = events
            .iter()
            .any(|e| matches!(e, TestEvent::RunFinished { .. }));
        run.apply_batch(events);
        // Jump to the first failure once the run ends, like IntelliJ.
        if finished
            && run
                .selected()
                .and_then(|n| run.node(n))
                .is_none_or(|n| n.status != TestStatus::Failed)
            && let Some(failed) = run.first_failed()
        {
            run.select(failed);
        }
        self.refresh_output();
        true
    }

    /// Request re-running every target of the last run.
    pub fn rerun_all_request(&mut self) -> Option<TestRunRequest> {
        let previous = self.request.clone()?;
        Some(self.follow_up(previous.label.clone(), previous.targets.clone(), &previous))
    }

    /// Request re-running only the failed tests (`None` when nothing failed).
    pub fn rerun_failed_request(&mut self) -> Option<TestRunRequest> {
        let failed = self.run.as_ref()?.failed_targets();
        if failed.is_empty() {
            return None;
        }
        let previous = self.request.clone()?;
        let label = format!("{} (failed)", previous.label.trim_end_matches(" (failed)"));
        Some(self.follow_up(label, failed, &previous))
    }

    fn follow_up(
        &mut self,
        label: String,
        targets: Vec<String>,
        previous: &TestRunRequest,
    ) -> TestRunRequest {
        let mut request = self.new_request(
            label,
            targets,
            previous.project_root.clone(),
            previous.file.clone(),
        );
        request.vm_options.clone_from(&previous.vm_options);
        request.env.clone_from(&previous.env);
        request
    }

    /// Applies a toolbar / tree action that only changes local state; returns whether it did.
    pub fn handle_local(&mut self, action: &TestRunnerAction) -> bool {
        let Some(run) = self.run.as_mut() else {
            return false;
        };
        match action {
            TestRunnerAction::ToggleShowPassed => {
                let mut filter: TestFilter = run.filter();
                filter.show_passed = !filter.show_passed;
                run.set_filter(filter);
            }
            TestRunnerAction::ToggleShowFailed => {
                let mut filter = run.filter();
                filter.show_failed = !filter.show_failed;
                run.set_filter(filter);
            }
            TestRunnerAction::SelectRow(row) => run.select_row(*row),
            TestRunnerAction::ToggleRow(row) => {
                if let Some(node) = run.rows().get(*row).map(|r| r.node) {
                    run.toggle_expanded(node);
                    run.select(node);
                }
            }
            TestRunnerAction::RerunAll
            | TestRunnerAction::RerunFailed
            | TestRunnerAction::OpenFrame(_)
            | TestRunnerAction::SaveConfiguration => return false,
        }
        self.refresh_output();
        true
    }

    /// Moves the tree selection (keyboard `↑` / `↓`).
    pub fn move_selection(&mut self, down: bool) {
        if let Some(run) = self.run.as_mut() {
            run.move_selection(down);
            self.refresh_output();
        }
    }

    /// Jump target of output line `index`, when it is a frame inside the project.
    #[must_use]
    pub fn target_at(&self, index: usize) -> Option<&FrameTarget> {
        self.output_targets.get(index)?.as_ref()
    }

    /// Index of the first output line pointing into the project (`Enter` on the tree).
    #[must_use]
    pub fn first_project_frame(&self) -> Option<usize> {
        self.output_targets.iter().position(Option::is_some)
    }

    fn refresh_output(&mut self) {
        let Some(run) = self.run.as_ref() else {
            self.output.clear();
            self.output_targets.clear();
            return;
        };
        let mut lines = run
            .selected()
            .map(|n| run.output_lines(n))
            .unwrap_or_default();
        if lines.is_empty() {
            lines = run
                .console()
                .lines()
                .map(|text| OutputLine {
                    text: text.to_owned(),
                    kind: OutputKind::Stdout,
                    frame: None,
                })
                .collect();
        }
        self.output_targets = lines
            .iter()
            .map(|line| {
                line.frame
                    .as_ref()
                    .and_then(|f| resolve_frame(f, self.files.iter().map(PathBuf::as_path)))
            })
            .collect();
        self.output = lines;
    }
}

/// Resolves `frame` against the project's files into a jump target.
#[must_use]
pub fn resolve_frame<'a>(
    frame: &StackFrame,
    files: impl IntoIterator<Item = &'a Path>,
) -> Option<FrameTarget> {
    let path = frame.resolve_in(files)?;
    Some(FrameTarget {
        path: path.to_path_buf(),
        line: usize::try_from(frame.line?).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use kova_tools::junit::{TestId, TestOutcome};

    use super::*;

    const CLASS: &str = "com.kova.shop.OrderServiceTest";

    fn failure() -> TestEvent {
        TestEvent::TestFinished {
            id: TestId::method(CLASS, "rejects"),
            outcome: TestOutcome::failed(
                64,
                "boom",
                "java.lang.AssertionError: boom\n\tat com.kova.shop.OrderService.validate(OrderService.java:84)\n\tat java.lang.Thread.run(Thread.java:1)",
            ),
        }
    }

    #[test]
    fn stale_batches_are_ignored_and_failures_selected_on_finish() {
        let mut model = TestRunnerModel::default();
        let first = model.new_request("A", vec![CLASS.to_owned()], None, None);
        model.start(first.clone());
        let second = model.new_request("B", vec![CLASS.to_owned()], None, None);
        model.start(second.clone());
        assert!(!model.apply(first.run_id, vec![failure()]));
        assert!(model.apply(
            second.run_id,
            vec![
                TestEvent::TestFinished {
                    id: TestId::method(CLASS, "ok"),
                    outcome: TestOutcome::passed(3),
                },
                failure(),
                TestEvent::RunFinished {
                    success: false,
                    message: None
                },
            ]
        ));
        let run = model.run().unwrap();
        assert_eq!(run.node(run.selected().unwrap()).unwrap().name, "rejects");
        assert!(model.output()[0].text.contains("AssertionError"));
    }

    #[test]
    fn rerun_requests_keep_context_and_select_failed() {
        let mut model = TestRunnerModel::default();
        let mut request = model.new_request(
            "OrderServiceTest",
            vec![CLASS.to_owned()],
            Some(PathBuf::from("/p")),
            Some(PathBuf::from("/p/src/test/java/T.java")),
        );
        request.vm_options = vec!["-ea".to_owned()];
        model.start(request.clone());
        assert!(model.rerun_failed_request().is_none());
        model.apply(request.run_id, vec![failure()]);

        let failed = model.rerun_failed_request().unwrap();
        assert_eq!(failed.targets, vec![format!("{CLASS}#rejects")]);
        assert_eq!(failed.vm_options, vec!["-ea".to_owned()]);
        assert_eq!(failed.label, "OrderServiceTest (failed)");
        assert!(failed.run_id > request.run_id);

        let all = model.rerun_all_request().unwrap();
        assert_eq!(all.targets, vec![CLASS.to_owned()]);
        assert_eq!(all.file, request.file);
    }

    #[test]
    fn local_actions_filter_select_and_resolve_frames() {
        let mut model = TestRunnerModel::default();
        let request = model.new_request("t", Vec::new(), None, None);
        model.start(request.clone());
        model.apply(
            request.run_id,
            vec![
                TestEvent::TestFinished {
                    id: TestId::method(CLASS, "ok"),
                    outcome: TestOutcome::passed(3),
                },
                failure(),
            ],
        );
        assert_eq!(model.run().unwrap().rows().len(), 3);
        assert!(model.handle_local(&TestRunnerAction::ToggleShowPassed));
        assert_eq!(model.run().unwrap().rows().len(), 2);
        assert!(model.handle_local(&TestRunnerAction::SelectRow(1)));
        assert!(!model.handle_local(&TestRunnerAction::RerunAll));

        let files = vec![PathBuf::from(
            "src/main/java/com/kova/shop/OrderService.java",
        )];
        model.set_project_files(files.clone());
        let index = model.first_project_frame().unwrap();
        assert_eq!(
            model.target_at(index),
            Some(&FrameTarget {
                path: files[0].clone(),
                line: 84
            })
        );
        assert!(model.output()[index].frame.is_some());
        assert_eq!(model.target_at(index + 1), None);
    }
}
