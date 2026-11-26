//! Headless `JUnit` result tree: *Suite → Class → Method* (`KOVA-040`).
//!
//! A [`TestRun`] is fed with [`TestEvent`]s coming from BSP notifications or parsed
//! `TEST-*.xml` reports. It aggregates statuses and durations up the tree, keeps the
//! `Passed / Failed / Total` counters incrementally and exposes a flattened, filtered list of
//! [`VisibleRow`]s so the UI can virtualize thousands of tests.

use std::collections::HashMap;

use crate::junit::stacktrace_parser::{
    StackFrame, TraceLine, parse_stack_frame, parse_stacktrace, stack_frames,
};

/// Execution status of a test node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TestStatus {
    /// Started (or queued) and not finished yet.
    Running,
    /// Finished successfully.
    Passed,
    /// Assertion failure or error.
    Failed,
    /// Skipped / disabled / aborted assumption.
    Ignored,
}

impl TestStatus {
    /// Whether the status is final.
    #[must_use]
    pub const fn is_finished(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// Level of a node in the result tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// Build target / module / XML `testsuite` grouping several classes.
    Suite,
    /// Test class (`com.acme.OrderTest`).
    Class,
    /// Test method (`shouldCreateOrder`).
    Method,
}

/// Identity of a test reported by a runner.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TestId {
    /// Optional suite (module or build target) grouping the class.
    pub suite: Option<String>,
    /// Fully qualified class name; `$` separates nested classes.
    pub class_name: String,
    /// Method name (`None` for class-level events).
    pub method: Option<String>,
}

impl TestId {
    /// Identifies a test method.
    #[must_use]
    pub fn method(class_name: impl Into<String>, method: impl Into<String>) -> Self {
        Self {
            suite: None,
            class_name: class_name.into(),
            method: Some(method.into()),
        }
    }

    /// Identifies a test class.
    #[must_use]
    pub fn class(class_name: impl Into<String>) -> Self {
        Self {
            suite: None,
            class_name: class_name.into(),
            method: None,
        }
    }

    /// Places the test inside `suite`.
    #[must_use]
    pub fn in_suite(mut self, suite: impl Into<String>) -> Self {
        self.suite = Some(suite.into());
        self
    }

    /// Parses a runner target (`com.acme.OrderTest` or `com.acme.OrderTest#creates`).
    #[must_use]
    pub fn from_target(target: &str) -> Self {
        match target.split_once('#') {
            Some((class, method)) if !method.is_empty() => Self::method(class, method),
            Some((class, _)) => Self::class(class),
            None => Self::class(target),
        }
    }

    /// Runner target string (`Class` or `Class#method`).
    #[must_use]
    pub fn target(&self) -> String {
        match &self.method {
            Some(method) => format!("{}#{method}", self.class_name),
            None => self.class_name.clone(),
        }
    }
}

/// Output stream of a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// Final result of one test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestOutcome {
    /// Final status (`Running` is treated as `Ignored`).
    pub status: TestStatus,
    /// Wall-clock duration in milliseconds, when reported.
    pub duration_ms: Option<u64>,
    /// Failure / skip message.
    pub message: Option<String>,
    /// Full stack trace text of the failure.
    pub stacktrace: Option<String>,
}

impl TestOutcome {
    /// A passed outcome with a duration.
    #[must_use]
    pub const fn passed(duration_ms: u64) -> Self {
        Self {
            status: TestStatus::Passed,
            duration_ms: Some(duration_ms),
            message: None,
            stacktrace: None,
        }
    }

    /// A failed outcome with a message and stack trace.
    #[must_use]
    pub fn failed(
        duration_ms: u64,
        message: impl Into<String>,
        stacktrace: impl Into<String>,
    ) -> Self {
        Self {
            status: TestStatus::Failed,
            duration_ms: Some(duration_ms),
            message: Some(message.into()),
            stacktrace: Some(stacktrace.into()),
        }
    }

    /// An ignored outcome.
    #[must_use]
    pub const fn ignored() -> Self {
        Self {
            status: TestStatus::Ignored,
            duration_ms: None,
            message: None,
            stacktrace: None,
        }
    }
}

/// Unified event stream produced by the BSP adapter and the `TEST-*.xml` fallback parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestEvent {
    /// A new run begins (resets nothing; create a fresh [`TestRun`] per run).
    RunStarted {
        /// Human-readable run label.
        label: String,
    },
    /// A test (method or class) started.
    TestStarted(TestId),
    /// A test (method or class) finished.
    TestFinished {
        /// Test identity.
        id: TestId,
        /// Result.
        outcome: TestOutcome,
    },
    /// Console output of a test, or of the whole run when `id` is `None`.
    Output {
        /// Owning test, if known.
        id: Option<TestId>,
        /// Stream.
        stream: OutputStream,
        /// Text (may contain several lines).
        text: String,
    },
    /// The run ended (`success` is the build/test task result).
    RunFinished {
        /// Whether the build reported success.
        success: bool,
        /// Optional summary or error.
        message: Option<String>,
    },
}

/// Index of a node inside a [`TestRun`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub usize);

/// One node of the result tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestNode {
    /// Tree level.
    pub kind: NodeKind,
    /// Display name (suite name, simple class name or method name).
    pub name: String,
    /// Runner target (`suite`, `com.acme.T`, `com.acme.T#m`).
    pub target: String,
    /// Parent node.
    pub parent: Option<NodeId>,
    /// Children in arrival order.
    pub children: Vec<NodeId>,
    /// Depth from the root (0 for roots).
    pub depth: usize,
    /// Aggregated status.
    pub status: TestStatus,
    /// Own duration (methods) or the sum of the children (suites / classes).
    pub duration_ms: u64,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Failure or skip message.
    pub message: Option<String>,
    /// Failure stack trace text.
    pub stacktrace: Option<String>,
    /// Frames parsed from [`Self::stacktrace`].
    pub frames: Vec<StackFrame>,
    /// Whether the children are shown.
    pub expanded: bool,
    own_status: Option<TestStatus>,
}

/// `Passed / Failed / Ignored / Running / Total` counters over test methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TestCounts {
    /// Passed methods.
    pub passed: usize,
    /// Failed methods.
    pub failed: usize,
    /// Ignored methods.
    pub ignored: usize,
    /// Methods still running.
    pub running: usize,
    /// All known methods.
    pub total: usize,
}

impl TestCounts {
    /// Finished methods.
    #[must_use]
    pub const fn finished(&self) -> usize {
        self.passed + self.failed + self.ignored
    }

    /// Progress in `0.0..=1.0` (`0.0` when nothing is known yet).
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "test counts are far below f32 precision limits"
    )]
    pub fn progress(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.finished() as f32 / self.total as f32
        }
    }

    fn add(&mut self, status: TestStatus) {
        match status {
            TestStatus::Running => self.running += 1,
            TestStatus::Passed => self.passed += 1,
            TestStatus::Failed => self.failed += 1,
            TestStatus::Ignored => self.ignored += 1,
        }
    }

    fn remove(&mut self, status: TestStatus) {
        let slot = match status {
            TestStatus::Running => &mut self.running,
            TestStatus::Passed => &mut self.passed,
            TestStatus::Failed => &mut self.failed,
            TestStatus::Ignored => &mut self.ignored,
        };
        *slot = slot.saturating_sub(1);
    }
}

/// *Show Passed* / *Show Failed* toolbar filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestFilter {
    /// Show passed and ignored tests.
    pub show_passed: bool,
    /// Show failed tests.
    pub show_failed: bool,
}

impl Default for TestFilter {
    fn default() -> Self {
        Self {
            show_passed: true,
            show_failed: true,
        }
    }
}

impl TestFilter {
    const fn accepts(self, status: TestStatus) -> bool {
        match status {
            TestStatus::Running => true,
            TestStatus::Passed | TestStatus::Ignored => self.show_passed,
            TestStatus::Failed => self.show_failed,
        }
    }
}

/// One row of the flattened, filtered tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleRow {
    /// Node shown on this row.
    pub node: NodeId,
    /// Indentation level.
    pub depth: usize,
    /// Whether the node has children (shows a disclosure triangle).
    pub has_children: bool,
    /// Whether the node is expanded.
    pub expanded: bool,
}

/// Overall state of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
    /// Tests are executing.
    Running,
    /// The run ended.
    Finished {
        /// Build / test task result.
        success: bool,
        /// Optional summary or error message.
        message: Option<String>,
    },
}

/// Kind of a line in the output pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// Failure message or exception header.
    Failure,
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// One line of the output pane, optionally linked to a source frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLine {
    /// Text to display.
    pub text: String,
    /// Styling hint.
    pub kind: OutputKind,
    /// Parsed frame when the line is an `at …` frame.
    pub frame: Option<StackFrame>,
}

/// A whole test run: tree, counters, filter, selection and run-level console.
#[derive(Debug, Clone)]
pub struct TestRun {
    label: String,
    targets: Vec<String>,
    state: RunState,
    nodes: Vec<TestNode>,
    roots: Vec<NodeId>,
    index: HashMap<(Option<String>, String, Option<String>), NodeId>,
    counts: TestCounts,
    filter: TestFilter,
    rows: Vec<VisibleRow>,
    rows_dirty: bool,
    selected: Option<NodeId>,
    console: String,
    console_lines: Vec<(OutputStream, String)>,
}

impl TestRun {
    /// Creates an empty run for the requested `targets` (`Class` / `Class#method`).
    #[must_use]
    pub fn new(label: impl Into<String>, targets: Vec<String>) -> Self {
        Self {
            label: label.into(),
            targets,
            state: RunState::Running,
            nodes: Vec::new(),
            roots: Vec::new(),
            index: HashMap::new(),
            counts: TestCounts::default(),
            filter: TestFilter::default(),
            rows: Vec::new(),
            rows_dirty: false,
            selected: None,
            console: String::new(),
            console_lines: Vec::new(),
        }
    }

    /// Run label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Targets requested for this run (used by *Rerun All*).
    #[must_use]
    pub fn targets(&self) -> &[String] {
        &self.targets
    }

    /// Current run state.
    #[must_use]
    pub const fn state(&self) -> &RunState {
        &self.state
    }

    /// Whether the run is still executing.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        matches!(self.state, RunState::Running)
    }

    /// Counters over test methods.
    #[must_use]
    pub const fn counts(&self) -> TestCounts {
        self.counts
    }

    /// Current filter.
    #[must_use]
    pub const fn filter(&self) -> TestFilter {
        self.filter
    }

    /// Run-level console output (build log, messages without an owning test).
    #[must_use]
    pub fn console(&self) -> &str {
        &self.console
    }

    /// Run-level console lines preserving `Stdout`/`Stderr` streams and parsed stack frames.
    #[must_use]
    pub fn console_output_lines(&self) -> Vec<OutputLine> {
        self.console_lines
            .iter()
            .map(|(stream, text)| OutputLine {
                text: text.clone(),
                kind: match stream {
                    OutputStream::Stdout => OutputKind::Stdout,
                    OutputStream::Stderr => OutputKind::Stderr,
                },
                frame: parse_stack_frame(text),
            })
            .collect()
    }

    /// Node by id.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&TestNode> {
        self.nodes.get(id.0)
    }

    /// Root nodes.
    #[must_use]
    pub fn roots(&self) -> &[NodeId] {
        &self.roots
    }

    /// Number of nodes of every level.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Looks a test up by identity.
    #[must_use]
    pub fn find(&self, id: &TestId) -> Option<NodeId> {
        self.index
            .get(&(id.suite.clone(), id.class_name.clone(), id.method.clone()))
            .copied()
    }

    /// Applies one event. Call [`Self::refresh_rows`] (or use [`Self::apply_batch`]) before
    /// reading [`Self::rows`].
    pub fn apply(&mut self, event: TestEvent) {
        match event {
            TestEvent::RunStarted { label } => {
                self.label = label;
                self.state = RunState::Running;
            }
            TestEvent::TestStarted(id) => {
                let node = self.ensure(&id);
                self.set_own_status(node, TestStatus::Running);
            }
            TestEvent::TestFinished { id, outcome } => {
                let node = self.ensure(&id);
                self.finish(node, outcome);
            }
            TestEvent::Output { id, stream, text } => {
                if let Some(id) = id {
                    let node = self.ensure(&id);
                    let target = match stream {
                        OutputStream::Stdout => &mut self.nodes[node.0].stdout,
                        OutputStream::Stderr => &mut self.nodes[node.0].stderr,
                    };
                    push_text(target, &text);
                } else {
                    push_text(&mut self.console, &text);
                    push_console_lines(&mut self.console_lines, stream, &text);
                }
            }
            TestEvent::RunFinished { success, message } => {
                self.finish_dangling(success);
                if let Some(msg) = &message {
                    push_text(&mut self.console, msg);
                    let stream = if success {
                        OutputStream::Stdout
                    } else {
                        OutputStream::Stderr
                    };
                    push_console_lines(&mut self.console_lines, stream, msg);
                }
                self.state = RunState::Finished { success, message };
            }
        }
        self.rows_dirty = true;
    }

    /// Applies several events and refreshes the visible rows once.
    pub fn apply_batch(&mut self, events: impl IntoIterator<Item = TestEvent>) {
        for event in events {
            self.apply(event);
        }
        self.refresh_rows();
    }

    /// Changes the filter and refreshes the rows.
    pub fn set_filter(&mut self, filter: TestFilter) {
        self.filter = filter;
        self.rows_dirty = true;
        self.refresh_rows();
    }

    /// Expands or collapses `node`.
    pub fn toggle_expanded(&mut self, node: NodeId) {
        if let Some(n) = self.nodes.get_mut(node.0) {
            n.expanded = !n.expanded;
            self.rows_dirty = true;
            self.refresh_rows();
        }
    }

    /// Rebuilds the flattened rows if anything changed since the last call.
    pub fn refresh_rows(&mut self) {
        if !self.rows_dirty {
            return;
        }
        self.rows_dirty = false;
        let mut rows = Vec::with_capacity(self.rows.len().max(self.nodes.len()));
        let mut stack: Vec<NodeId> = self.roots.iter().rev().copied().collect();
        while let Some(id) = stack.pop() {
            if !self.is_visible(id) {
                continue;
            }
            let node = &self.nodes[id.0];
            rows.push(VisibleRow {
                node: id,
                depth: node.depth,
                has_children: !node.children.is_empty(),
                expanded: node.expanded,
            });
            if node.expanded {
                stack.extend(node.children.iter().rev().copied());
            }
        }
        self.rows = rows;
    }

    /// Flattened rows as of the last [`Self::refresh_rows`].
    #[must_use]
    pub fn rows(&self) -> &[VisibleRow] {
        &self.rows
    }

    /// Selects the node shown on `row`.
    pub fn select_row(&mut self, row: usize) {
        if let Some(visible) = self.rows.get(row) {
            self.selected = Some(visible.node);
        }
    }

    /// Selects `node` directly.
    pub fn select(&mut self, node: NodeId) {
        if node.0 < self.nodes.len() {
            self.selected = Some(node);
        }
    }

    /// Selected node.
    #[must_use]
    pub const fn selected(&self) -> Option<NodeId> {
        self.selected
    }

    /// Row index of the selected node, if it is visible.
    #[must_use]
    pub fn selected_row(&self) -> Option<usize> {
        let selected = self.selected?;
        self.rows.iter().position(|row| row.node == selected)
    }

    /// Moves the selection one visible row down (`true`) or up (`false`).
    pub fn move_selection(&mut self, down: bool) {
        if self.rows.is_empty() {
            return;
        }
        let next = match (self.selected_row(), down) {
            (None, _) => 0,
            (Some(row), true) => (row + 1).min(self.rows.len() - 1),
            (Some(row), false) => row.saturating_sub(1),
        };
        self.select_row(next);
    }

    /// First failed method, in tree order.
    #[must_use]
    pub fn first_failed(&self) -> Option<NodeId> {
        let mut stack: Vec<NodeId> = self.roots.iter().rev().copied().collect();
        while let Some(id) = stack.pop() {
            let node = &self.nodes[id.0];
            if node.status == TestStatus::Failed && node.children.is_empty() {
                return Some(id);
            }
            stack.extend(node.children.iter().rev().copied());
        }
        None
    }

    /// Targets of failed leaves (`Class#method`, or `Class` for class-level failures), for
    /// *Rerun Failed*.
    #[must_use]
    pub fn failed_targets(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|n| {
                n.children.is_empty() && n.kind != NodeKind::Suite && n.status == TestStatus::Failed
            })
            .map(|n| n.target.clone())
            .collect()
    }

    /// Output pane content for `node`: failure message, clickable stack trace, then
    /// `stdout` / `stderr`. A suite or class without own output shows its failed children.
    #[must_use]
    pub fn output_lines(&self, node: NodeId) -> Vec<OutputLine> {
        let Some(n) = self.node(node) else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        append_node_output(n, &mut lines);
        if lines.is_empty() && !n.children.is_empty() {
            let mut stack: Vec<NodeId> = n.children.iter().rev().copied().collect();
            while let Some(child) = stack.pop() {
                let c = &self.nodes[child.0];
                if c.status == TestStatus::Failed && c.children.is_empty() {
                    lines.push(OutputLine {
                        text: format!("✗ {}", c.target),
                        kind: OutputKind::Failure,
                        frame: None,
                    });
                    append_node_output(c, &mut lines);
                }
                stack.extend(c.children.iter().rev().copied());
            }
        }
        lines
    }

    fn ensure(&mut self, id: &TestId) -> NodeId {
        let key = (id.suite.clone(), id.class_name.clone(), id.method.clone());
        if let Some(existing) = self.index.get(&key) {
            return *existing;
        }
        let suite = id.suite.as_ref().map(|suite| {
            let key = (Some(suite.clone()), String::new(), None);
            if let Some(node) = self.index.get(&key) {
                *node
            } else {
                let node = self.push_node(NodeKind::Suite, suite.clone(), suite.clone(), None);
                self.index.insert(key, node);
                node
            }
        });
        let class_key = (id.suite.clone(), id.class_name.clone(), None);
        let class = if let Some(node) = self.index.get(&class_key) {
            *node
        } else {
            let simple = id
                .class_name
                .rsplit_once('.')
                .map_or(id.class_name.as_str(), |(_, s)| s)
                .to_owned();
            let node = self.push_node(NodeKind::Class, simple, id.class_name.clone(), suite);
            self.index.insert(class_key, node);
            node
        };
        let Some(method) = &id.method else {
            return class;
        };
        let node = self.push_node(NodeKind::Method, method.clone(), id.target(), Some(class));
        self.counts.total += 1;
        self.counts.add(TestStatus::Running);
        self.index.insert(key, node);
        node
    }

    fn push_node(
        &mut self,
        kind: NodeKind,
        name: String,
        target: String,
        parent: Option<NodeId>,
    ) -> NodeId {
        let id = NodeId(self.nodes.len());
        let depth = parent.map_or(0, |p| self.nodes[p.0].depth + 1);
        self.nodes.push(TestNode {
            kind,
            name,
            target,
            parent,
            children: Vec::new(),
            depth,
            status: TestStatus::Running,
            duration_ms: 0,
            stdout: String::new(),
            stderr: String::new(),
            message: None,
            stacktrace: None,
            frames: Vec::new(),
            expanded: kind != NodeKind::Method,
            own_status: None,
        });
        match parent {
            Some(p) => self.nodes[p.0].children.push(id),
            None => self.roots.push(id),
        }
        if let Some(p) = parent {
            self.recompute_up(p);
        }
        id
    }

    fn finish(&mut self, node: NodeId, outcome: TestOutcome) {
        let status = match outcome.status {
            TestStatus::Running => TestStatus::Ignored,
            other => other,
        };
        {
            let n = &mut self.nodes[node.0];
            if let Some(duration) = outcome.duration_ms {
                if n.kind == NodeKind::Method || n.children.is_empty() {
                    n.duration_ms = duration;
                }
            }
            if outcome.message.is_some() {
                n.message = outcome.message;
            }
            if let Some(trace) = outcome.stacktrace {
                n.frames = stack_frames(&trace);
                n.stacktrace = Some(trace);
            }
        }
        // Class-level results only override the aggregate when they carry a failure.
        let is_container = !self.nodes[node.0].children.is_empty();
        if is_container && status != TestStatus::Failed {
            self.nodes[node.0].own_status = None;
            self.recompute_up(node);
        } else {
            self.set_own_status(node, status);
        }
    }

    fn set_own_status(&mut self, node: NodeId, status: TestStatus) {
        let n = &mut self.nodes[node.0];
        n.own_status = Some(status);
        if n.kind == NodeKind::Method {
            let old = n.status;
            n.status = status;
            self.counts.remove(old);
            self.counts.add(status);
            if let Some(parent) = n.parent {
                self.recompute_up(parent);
            }
        } else {
            self.recompute_up(node);
        }
    }

    fn recompute_up(&mut self, start: NodeId) {
        let mut current = Some(start);
        while let Some(id) = current {
            let node = &self.nodes[id.0];
            let (status, duration) = if node.children.is_empty() {
                (
                    node.own_status.unwrap_or(TestStatus::Running),
                    node.duration_ms,
                )
            } else {
                let mut agg = Aggregate::default();
                let mut duration = 0_u64;
                for child in &node.children {
                    let c = &self.nodes[child.0];
                    agg.add(c.status);
                    duration = duration.saturating_add(c.duration_ms);
                }
                if let Some(own) = node.own_status {
                    agg.add(own);
                }
                (agg.status(), duration)
            };
            let node = &mut self.nodes[id.0];
            node.status = status;
            node.duration_ms = duration;
            current = node.parent;
        }
    }

    fn finish_dangling(&mut self, success: bool) {
        let dangling: Vec<NodeId> = (0..self.nodes.len())
            .map(NodeId)
            .filter(|id| {
                let n = &self.nodes[id.0];
                n.children.is_empty() && n.status == TestStatus::Running
            })
            .collect();
        for id in dangling {
            let outcome = if success {
                TestOutcome::ignored()
            } else {
                TestOutcome {
                    status: TestStatus::Failed,
                    duration_ms: None,
                    message: Some("Test did not finish (run terminated)".to_owned()),
                    stacktrace: None,
                }
            };
            self.finish(id, outcome);
        }
    }

    fn is_visible(&self, id: NodeId) -> bool {
        let node = &self.nodes[id.0];
        if node.children.is_empty() {
            return self.filter.accepts(node.status);
        }
        if node.own_status == Some(TestStatus::Failed) && self.filter.show_failed {
            return true;
        }
        node.children.iter().any(|child| self.is_visible(*child))
    }
}

#[derive(Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent flags folded while scanning children"
)]
struct Aggregate {
    running: bool,
    failed: bool,
    passed: bool,
    any: bool,
}

impl Aggregate {
    fn add(&mut self, status: TestStatus) {
        self.any = true;
        match status {
            TestStatus::Running => self.running = true,
            TestStatus::Failed => self.failed = true,
            TestStatus::Passed => self.passed = true,
            TestStatus::Ignored => {}
        }
    }

    fn status(&self) -> TestStatus {
        if self.running || !self.any {
            TestStatus::Running
        } else if self.failed {
            TestStatus::Failed
        } else if self.passed {
            TestStatus::Passed
        } else {
            TestStatus::Ignored
        }
    }
}

fn push_text(target: &mut String, text: &str) {
    if !target.is_empty() && !target.ends_with('\n') {
        target.push('\n');
    }
    target.push_str(text);
}

fn push_console_lines(target: &mut Vec<(OutputStream, String)>, stream: OutputStream, text: &str) {
    if text.is_empty() {
        return;
    }
    for line in text.split('\n') {
        target.push((stream, line.trim_end_matches('\r').to_owned()));
    }
}

fn append_node_output(node: &TestNode, lines: &mut Vec<OutputLine>) {
    let mut trace_has_message = false;
    if let Some(trace) = &node.stacktrace {
        trace_has_message = node
            .message
            .as_deref()
            .is_some_and(|msg| trace.lines().next().is_some_and(|l| l.contains(msg)));
    }
    if let Some(message) = &node.message
        && !trace_has_message
    {
        for line in message.lines() {
            lines.push(OutputLine {
                text: line.to_owned(),
                kind: OutputKind::Failure,
                frame: None,
            });
        }
    }
    if let Some(trace) = &node.stacktrace {
        for line in parse_stacktrace(trace) {
            lines.push(match line {
                TraceLine::Frame(frame) => OutputLine {
                    text: format!("  {}", frame.raw),
                    kind: OutputKind::Failure,
                    frame: Some(frame),
                },
                TraceLine::CausedBy(text) => OutputLine {
                    text: format!("Caused by: {text}"),
                    kind: OutputKind::Failure,
                    frame: None,
                },
                TraceLine::Elided(count) => OutputLine {
                    text: format!("  ... {count} more"),
                    kind: OutputKind::Failure,
                    frame: None,
                },
                TraceLine::Text(text) => OutputLine {
                    text,
                    kind: OutputKind::Failure,
                    frame: None,
                },
            });
        }
    }
    for (stream, kind) in [
        (&node.stdout, OutputKind::Stdout),
        (&node.stderr, OutputKind::Stderr),
    ] {
        for line in parse_stacktrace(stream) {
            lines.push(match line {
                TraceLine::Frame(frame) => OutputLine {
                    text: format!("  {}", frame.raw),
                    kind,
                    frame: Some(frame),
                },
                TraceLine::CausedBy(text) => OutputLine {
                    text: format!("Caused by: {text}"),
                    kind,
                    frame: None,
                },
                TraceLine::Elided(count) => OutputLine {
                    text: format!("  ... {count} more"),
                    kind,
                    frame: None,
                },
                TraceLine::Text(text) => OutputLine {
                    text,
                    kind,
                    frame: None,
                },
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLASS: &str = "com.kova.shop.OrderServiceTest";

    fn started(method: &str) -> TestEvent {
        TestEvent::TestStarted(TestId::method(CLASS, method))
    }

    fn passed(method: &str, ms: u64) -> TestEvent {
        TestEvent::TestFinished {
            id: TestId::method(CLASS, method),
            outcome: TestOutcome::passed(ms),
        }
    }

    fn failed(method: &str) -> TestEvent {
        TestEvent::TestFinished {
            id: TestId::method(CLASS, method),
            outcome: TestOutcome::failed(
                64,
                "Expected IllegalArgumentException",
                "java.lang.AssertionError: Expected IllegalArgumentException\n\
                 \tat com.kova.shop.OrderService.validate(OrderService.java:84)\n\
                 \tat com.kova.shop.OrderServiceTest.shouldRejectNegativeAmount(OrderServiceTest.java:52)\n",
            ),
        }
    }

    #[test]
    fn statuses_update_live_and_aggregate_to_the_class() {
        let mut run = TestRun::new("OrderServiceTest", vec![CLASS.to_owned()]);
        run.apply_batch([
            started("shouldCreateOrder"),
            started("shouldRejectNegativeAmount"),
        ]);
        let class = run.find(&TestId::class(CLASS)).unwrap();
        assert_eq!(run.node(class).unwrap().status, TestStatus::Running);
        assert_eq!(run.counts().running, 2);
        assert_eq!(run.rows().len(), 3);

        run.apply_batch([passed("shouldCreateOrder", 38)]);
        assert_eq!(run.node(class).unwrap().status, TestStatus::Running);
        assert_eq!(run.counts().passed, 1);

        run.apply_batch([failed("shouldRejectNegativeAmount")]);
        let class_node = run.node(class).unwrap();
        assert_eq!(class_node.status, TestStatus::Failed);
        assert_eq!(class_node.duration_ms, 102);
        let counts = run.counts();
        assert_eq!((counts.passed, counts.failed, counts.total), (1, 1, 2));
        assert!((counts.progress() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn suite_class_method_hierarchy() {
        let mut run = TestRun::new("all", Vec::new());
        run.apply_batch([
            TestEvent::TestStarted(TestId::method(CLASS, "a").in_suite(":app:test")),
            TestEvent::TestStarted(TestId::method("com.kova.X", "b").in_suite(":app:test")),
        ]);
        assert_eq!(run.roots().len(), 1);
        let suite = run.node(run.roots()[0]).unwrap();
        assert_eq!(suite.kind, NodeKind::Suite);
        assert_eq!(suite.children.len(), 2);
        let depths: Vec<usize> = run.rows().iter().map(|r| r.depth).collect();
        assert_eq!(depths, vec![0, 1, 2, 1, 2]);
        assert_eq!(
            run.node(run.rows()[1].node).unwrap().name,
            "OrderServiceTest"
        );
    }

    #[test]
    fn filter_hides_passed_and_keeps_failed_parents() {
        let mut run = TestRun::new("t", Vec::new());
        run.apply_batch([passed("a", 1), failed("b"), passed("c", 1)]);
        assert_eq!(run.rows().len(), 4);
        run.set_filter(TestFilter {
            show_passed: false,
            show_failed: true,
        });
        let names: Vec<&str> = run
            .rows()
            .iter()
            .map(|r| run.node(r.node).unwrap().name.as_str())
            .collect();
        assert_eq!(names, vec!["OrderServiceTest", "b"]);
        run.set_filter(TestFilter {
            show_passed: true,
            show_failed: false,
        });
        assert_eq!(run.rows().len(), 3);
    }

    #[test]
    fn collapse_hides_children() {
        let mut run = TestRun::new("t", Vec::new());
        run.apply_batch([passed("a", 1), passed("b", 1)]);
        let class = run.roots()[0];
        run.toggle_expanded(class);
        assert_eq!(run.rows().len(), 1);
        assert!(!run.rows()[0].expanded);
    }

    #[test]
    fn failed_targets_and_output_with_frames() {
        let mut run = TestRun::new("t", Vec::new());
        run.apply_batch([passed("a", 1), failed("shouldRejectNegativeAmount")]);
        assert_eq!(
            run.failed_targets(),
            vec![format!("{CLASS}#shouldRejectNegativeAmount")]
        );
        let node = run.first_failed().unwrap();
        let lines = run.output_lines(node);
        assert!(lines[0].text.starts_with("java.lang.AssertionError"));
        let frames: Vec<_> = lines.iter().filter_map(|l| l.frame.as_ref()).collect();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].line, Some(84));

        // Class without own output aggregates its failed children.
        let class_lines = run.output_lines(run.roots()[0]);
        assert!(class_lines[0].text.contains("shouldRejectNegativeAmount"));
    }

    #[test]
    fn output_and_console_are_captured() {
        let mut run = TestRun::new("t", Vec::new());
        run.apply_batch([
            TestEvent::Output {
                id: Some(TestId::method(CLASS, "a")),
                stream: OutputStream::Stdout,
                text: "hello".to_owned(),
            },
            TestEvent::Output {
                id: None,
                stream: OutputStream::Stdout,
                text: "> Task :test".to_owned(),
            },
        ]);
        let node = run.find(&TestId::method(CLASS, "a")).unwrap();
        assert_eq!(run.node(node).unwrap().stdout, "hello");
        assert_eq!(run.console(), "> Task :test");
    }

    #[test]
    fn console_output_lines_preserve_stderr_stream() {
        let mut run = TestRun::new("t", Vec::new());
        run.apply_batch([
            TestEvent::Output {
                id: None,
                stream: OutputStream::Stdout,
                text: "> Task :compileJava".to_owned(),
            },
            TestEvent::Output {
                id: None,
                stream: OutputStream::Stderr,
                text: "error: cannot find symbol".to_owned(),
            },
        ]);
        let lines = run.console_output_lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].kind, OutputKind::Stdout);
        assert_eq!(lines[1].kind, OutputKind::Stderr);
        assert_eq!(lines[1].text, "error: cannot find symbol");
    }

    #[test]
    fn run_finished_closes_dangling_tests() {
        let mut run = TestRun::new("t", Vec::new());
        run.apply_batch([
            started("a"),
            TestEvent::RunFinished {
                success: false,
                message: Some("Build failed".to_owned()),
            },
        ]);
        assert!(!run.is_running());
        assert_eq!(run.counts().failed, 1);
        assert_eq!(run.counts().running, 0);
    }

    #[test]
    fn class_level_failure_without_methods_is_a_failed_leaf() {
        let mut run = TestRun::new("t", Vec::new());
        run.apply_batch([TestEvent::TestFinished {
            id: TestId::class(CLASS),
            outcome: TestOutcome::failed(0, "init failed", "java.lang.ExceptionInInitializerError"),
        }]);
        assert_eq!(run.failed_targets(), vec![CLASS.to_owned()]);
        assert_eq!(run.node(run.roots()[0]).unwrap().status, TestStatus::Failed);
    }

    #[test]
    fn selection_moves_over_visible_rows() {
        let mut run = TestRun::new("t", Vec::new());
        run.apply_batch([passed("a", 1), passed("b", 1)]);
        run.move_selection(true);
        assert_eq!(run.selected_row(), Some(0));
        run.move_selection(true);
        run.move_selection(true);
        run.move_selection(true);
        assert_eq!(run.selected_row(), Some(2));
        run.move_selection(false);
        assert_eq!(run.selected_row(), Some(1));
    }

    #[test]
    fn target_parsing_roundtrips() {
        assert_eq!(TestId::from_target("a.B#m").target(), "a.B#m");
        assert_eq!(TestId::from_target("a.B").method, None);
        assert_eq!(TestId::from_target("a.B#").method, None);
    }

    #[test]
    fn thousands_of_tests_flatten_quickly() {
        let mut run = TestRun::new("big", Vec::new());
        let events: Vec<TestEvent> = (0..5_000)
            .map(|i| TestEvent::TestFinished {
                id: TestId::method(format!("com.kova.C{}", i / 50), format!("t{i}")),
                outcome: TestOutcome::passed(1),
            })
            .collect();
        run.apply_batch(events);
        assert_eq!(run.counts().total, 5_000);
        assert_eq!(run.rows().len(), 5_100);
    }
}
