//! JVM *Debugger* bottom dock (`KOVA-041`).
//!
//! [`DebuggerModel`] is the headless state behind the dock: breakpoints per file (line,
//! conditional, logpoint), the session status, *Threads & Frames*, the lazily expanded and
//! paged *Variables* tree, *Watches*, the debuggee console and the *Evaluate Expression*
//! (`Alt+F8`) dialog ([`evaluate_modal::EvaluateModal`]).
//!
//! The DAP session itself runs in the host (`kova_app` → `kova_lsp_dap::dap`): the shell turns
//! user actions into [`DebugCommand`]s for a [`DebugCommandSink`] and applies the
//! [`DebugUpdate`]s the host sends back. Nothing here blocks the UI thread: children of
//! large arrays and collections are fetched one page ([`VARIABLES_PAGE_SIZE`]) at a time.

pub mod debugger_panel;
pub mod evaluate_modal;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use debugger_panel::{DEBUG_ROW_HEIGHT_PX, DebuggerColors, DebuggerHandler};
pub use evaluate_modal::{EvaluateModal, EvaluateState};

/// Number of indexed children fetched per *Variables* page.
pub const VARIABLES_PAGE_SIZE: usize = 100;
/// Number of stack frames fetched per thread.
pub const STACK_PAGE_SIZE: usize = 64;
/// Maximum number of console lines kept.
const MAX_CONSOLE_LINES: usize = 5_000;

/// A breakpoint as edited in the gutter / *Breakpoints* dialog.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BreakpointSpec {
    /// Boolean Java / Kotlin condition (conditional breakpoint).
    pub condition: Option<String>,
    /// Message with `{expr}` placeholders (logpoint, does not suspend).
    pub log_message: Option<String>,
    /// `Some(true)` once the JVM accepted it, `Some(false)` when rejected, `None` before a
    /// session synchronised it.
    pub verified: Option<bool>,
}

impl BreakpointSpec {
    /// Gutter icon of the breakpoint.
    #[must_use]
    pub const fn icon(&self) -> BreakpointIcon {
        match (&self.log_message, &self.condition, self.verified) {
            (_, _, Some(false)) => BreakpointIcon::Invalid,
            (Some(_), _, _) => BreakpointIcon::Logpoint,
            (None, Some(_), _) => BreakpointIcon::Conditional,
            (None, None, _) => BreakpointIcon::Line,
        }
    }
}

/// Gutter icon of a breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BreakpointIcon {
    /// Red dot.
    Line,
    /// Red dot with a `?` badge.
    Conditional,
    /// Orange diamond (no suspension).
    Logpoint,
    /// Hollow dot (rejected by the JVM).
    Invalid,
}

/// A thread as shown in *Threads & Frames*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadItem {
    /// JVM thread id.
    pub id: i64,
    /// Thread name.
    pub name: String,
}

/// A stack frame as shown in *Threads & Frames*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameItem {
    /// DAP frame id.
    pub id: i64,
    /// Method label.
    pub name: String,
    /// Absolute source path, when on disk.
    pub path: Option<PathBuf>,
    /// 1-based line.
    pub line: u32,
}

/// A variable / field / evaluation result.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VariableItem {
    /// Name.
    pub name: String,
    /// Rendered value.
    pub value: String,
    /// Java type.
    pub type_name: Option<String>,
    /// Non-zero when the value has children.
    pub reference: i64,
    /// Number of indexed children (arrays / collections), when known.
    pub indexed: Option<usize>,
    /// Expression evaluating to this value (*Add to Watches*).
    pub evaluate_name: Option<String>,
}

/// Session status shown in the debugger toolbar.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DebuggerStatus {
    /// No session.
    #[default]
    Idle,
    /// Connecting to `java-debug` / JDWP.
    Connecting,
    /// Debuggee running.
    Running,
    /// Suspended.
    Suspended {
        /// `breakpoint`, `step`, `pause`, `exception`.
        reason: String,
        /// Suspended thread.
        thread_id: Option<i64>,
    },
    /// Session ended; `message` explains failures.
    Terminated {
        /// Reason or error.
        message: Option<String>,
    },
}

impl DebuggerStatus {
    /// Whether a session is alive.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Connecting | Self::Running | Self::Suspended { .. }
        )
    }

    /// Whether stepping and evaluation are possible.
    #[must_use]
    pub const fn is_suspended(&self) -> bool {
        matches!(self, Self::Suspended { .. })
    }

    /// Toolbar label.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Idle => "Not started".to_owned(),
            Self::Connecting => "Connecting to java-debug…".to_owned(),
            Self::Running => "Running".to_owned(),
            Self::Suspended { reason, .. } => format!("Suspended ({reason})"),
            Self::Terminated { message: None } => "Terminated".to_owned(),
            Self::Terminated { message: Some(m) } => format!("Terminated: {m}"),
        }
    }
}

/// Stepping command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DebugStep {
    /// *Step Over* (`F8`).
    Over,
    /// *Step Into* (`F7`).
    Into,
    /// *Step Out* (`Shift+F8`).
    Out,
}

/// Where an expression is evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EvaluateTarget {
    /// *Evaluate Expression* dialog.
    Dialog,
    /// *Watches* row `usize`.
    Watch(usize),
}

/// How a debug session is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugLaunchRequest {
    /// Session identifier echoed by every [`DebugUpdate`].
    pub session_id: u64,
    /// Toolbar title.
    pub label: String,
    /// Fully qualified main class (or test class) to launch.
    pub main_class: String,
    /// Attach to a JVM already listening on this JDWP port instead of launching.
    pub attach_port: Option<u16>,
    /// Project root.
    pub project_root: Option<PathBuf>,
    /// Breakpoints to install before the debuggee runs.
    pub breakpoints: BTreeMap<PathBuf, BTreeMap<u32, BreakpointSpec>>,
}

/// Request to the host DAP session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebugCommand {
    /// Start a session.
    Start(DebugLaunchRequest),
    /// Replace the breakpoints of one file.
    SetBreakpoints {
        /// Absolute source path.
        file: PathBuf,
        /// Breakpoints by 1-based line.
        breakpoints: BTreeMap<u32, BreakpointSpec>,
    },
    /// *Resume Program* (`F9`).
    Resume {
        /// Thread to resume.
        thread_id: i64,
    },
    /// Step command.
    Step {
        /// Kind.
        kind: DebugStep,
        /// Thread.
        thread_id: i64,
    },
    /// Load the stack of a thread.
    LoadStack {
        /// Thread.
        thread_id: i64,
    },
    /// Load the scopes (and first variables) of a frame.
    LoadScopes {
        /// Frame.
        frame_id: i64,
    },
    /// Load one page of children of `reference`.
    LoadVariables {
        /// Container reference.
        reference: i64,
        /// First child index.
        start: usize,
        /// Page size (`0` = all named children).
        count: usize,
        /// Whether to fetch indexed children only.
        indexed: bool,
    },
    /// Evaluate an expression in the selected frame.
    Evaluate {
        /// Expression.
        expression: String,
        /// Selected frame.
        frame_id: Option<i64>,
        /// Requester.
        target: EvaluateTarget,
    },
    /// Stop the session.
    Stop,
}

/// Update reported by the host for the current session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebugUpdate {
    /// Session status changed.
    Status(DebuggerStatus),
    /// Thread list.
    Threads(Vec<ThreadItem>),
    /// Stack of a thread.
    Stack {
        /// Thread.
        thread_id: i64,
        /// Frames, innermost first.
        frames: Vec<FrameItem>,
    },
    /// Scopes of a frame (as top-level variables with references).
    Scopes {
        /// Frame.
        frame_id: i64,
        /// Scopes.
        scopes: Vec<VariableItem>,
    },
    /// One page of children.
    Variables {
        /// Container reference.
        reference: i64,
        /// Index of the first child.
        start: usize,
        /// Children.
        items: Vec<VariableItem>,
    },
    /// Evaluation result or error message.
    Evaluated {
        /// Requester.
        target: EvaluateTarget,
        /// Result.
        result: Result<VariableItem, String>,
    },
    /// Adapter verification of a breakpoint.
    BreakpointVerified {
        /// Source path.
        file: PathBuf,
        /// Line.
        line: u32,
        /// Whether the JVM accepted it.
        verified: bool,
    },
    /// Debuggee / logpoint output.
    Output(String),
}

/// Host callback executing [`DebugCommand`]s off the UI thread.
pub type DebugCommandSink = Arc<dyn Fn(u64, DebugCommand) + Send + Sync>;

/// One row of the *Variables & Watches* tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariableRow {
    /// A variable (scope, field, element).
    Value {
        /// Nesting level.
        depth: usize,
        /// Value.
        item: VariableItem,
        /// Whether its children are shown.
        expanded: bool,
    },
    /// Placeholder while a page is being fetched.
    Loading {
        /// Nesting level.
        depth: usize,
    },
    /// *Load more* row for the next page of indexed children.
    More {
        /// Nesting level.
        depth: usize,
        /// Container reference.
        reference: i64,
        /// First index of the next page.
        start: usize,
        /// Total indexed children.
        total: usize,
    },
}

impl VariableRow {
    const fn depth(&self) -> usize {
        match self {
            Self::Value { depth, .. } | Self::Loading { depth } | Self::More { depth, .. } => {
                *depth
            }
        }
    }
}

/// A *Watches* entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchItem {
    /// Watched expression.
    pub expression: String,
    /// Last result (`Err` holds the evaluation error).
    pub result: Option<Result<String, String>>,
}

/// User interaction inside the debugger dock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebuggerAction {
    /// *Resume Program*.
    Resume,
    /// Step.
    Step(DebugStep),
    /// Stop the session.
    Stop,
    /// Open *Evaluate Expression*.
    OpenEvaluate,
    /// Select a thread.
    SelectThread(usize),
    /// Select a frame (and jump to its source).
    SelectFrame(usize),
    /// Expand / collapse a variable row.
    ToggleVariable(usize),
    /// Fetch the next page of a *Load more* row.
    LoadMore(usize),
    /// Remove a watch.
    RemoveWatch(usize),
}

/// Where a frame points to in the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    /// Absolute path.
    pub path: PathBuf,
    /// 1-based line.
    pub line: u32,
}

/// Headless state of the *Debugger* dock.
#[derive(Debug, Default)]
pub struct DebuggerModel {
    breakpoints: BTreeMap<PathBuf, BTreeMap<u32, BreakpointSpec>>,
    session_id: u64,
    label: String,
    status: DebuggerStatus,
    threads: Vec<ThreadItem>,
    selected_thread: Option<i64>,
    frames: Vec<FrameItem>,
    selected_frame: usize,
    variables: Vec<VariableRow>,
    watches: Vec<WatchItem>,
    console: Vec<String>,
    evaluate: Option<EvaluateModal>,
}

impl DebuggerModel {
    // -- Breakpoints --------------------------------------------------------

    /// Toggles a line breakpoint; returns the command synchronising the file with a live
    /// session (`None` when no session is active).
    pub fn toggle_breakpoint(&mut self, file: &Path, line: u32) -> Option<DebugCommand> {
        let lines = self.breakpoints.entry(file.to_path_buf()).or_default();
        if lines.remove(&line).is_none() {
            lines.insert(line, BreakpointSpec::default());
        }
        if lines.is_empty() {
            self.breakpoints.remove(file);
        }
        self.sync_command(file)
    }

    /// Sets (or clears with `None`) the condition of a breakpoint, creating it if needed.
    pub fn set_condition(
        &mut self,
        file: &Path,
        line: u32,
        condition: Option<String>,
    ) -> Option<DebugCommand> {
        let spec = self
            .breakpoints
            .entry(file.to_path_buf())
            .or_default()
            .entry(line)
            .or_default();
        spec.condition = condition.filter(|c| !c.trim().is_empty());
        spec.verified = None;
        self.sync_command(file)
    }

    /// Turns a breakpoint into a logpoint (or back with `None`), creating it if needed.
    pub fn set_log_message(
        &mut self,
        file: &Path,
        line: u32,
        message: Option<String>,
    ) -> Option<DebugCommand> {
        let spec = self
            .breakpoints
            .entry(file.to_path_buf())
            .or_default()
            .entry(line)
            .or_default();
        spec.log_message = message.filter(|m| !m.trim().is_empty());
        spec.verified = None;
        self.sync_command(file)
    }

    fn sync_command(&self, file: &Path) -> Option<DebugCommand> {
        self.status
            .is_active()
            .then(|| DebugCommand::SetBreakpoints {
                file: file.to_path_buf(),
                breakpoints: self.breakpoints.get(file).cloned().unwrap_or_default(),
            })
    }

    /// Breakpoints of `file` by 1-based line.
    #[must_use]
    pub fn breakpoints_for(&self, file: &Path) -> Option<&BTreeMap<u32, BreakpointSpec>> {
        self.breakpoints.get(file)
    }

    /// Every breakpoint.
    #[must_use]
    pub const fn breakpoints(&self) -> &BTreeMap<PathBuf, BTreeMap<u32, BreakpointSpec>> {
        &self.breakpoints
    }

    /// Gutter icons of `file` keyed by 0-based buffer row.
    #[must_use]
    pub fn gutter_icons(&self, file: &Path) -> BTreeMap<usize, BreakpointIcon> {
        self.breakpoints
            .get(file)
            .into_iter()
            .flatten()
            .map(|(line, spec)| ((*line as usize).saturating_sub(1), spec.icon()))
            .collect()
    }

    // -- Session ------------------------------------------------------------

    /// Starts a new session; returns the command for the host.
    pub fn start(
        &mut self,
        label: impl Into<String>,
        main_class: impl Into<String>,
        attach_port: Option<u16>,
        project_root: Option<PathBuf>,
    ) -> DebugCommand {
        self.session_id += 1;
        self.label = label.into();
        self.status = DebuggerStatus::Connecting;
        self.threads.clear();
        self.selected_thread = None;
        self.frames.clear();
        self.selected_frame = 0;
        self.variables.clear();
        self.console.clear();
        for spec in self.breakpoints.values_mut().flat_map(BTreeMap::values_mut) {
            spec.verified = None;
        }
        DebugCommand::Start(DebugLaunchRequest {
            session_id: self.session_id,
            label: self.label.clone(),
            main_class: main_class.into(),
            attach_port,
            project_root,
            breakpoints: self.breakpoints.clone(),
        })
    }

    /// Current session id.
    #[must_use]
    pub const fn session_id(&self) -> u64 {
        self.session_id
    }

    /// Session title.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Session status.
    #[must_use]
    pub const fn status(&self) -> &DebuggerStatus {
        &self.status
    }

    /// JVM threads.
    #[must_use]
    pub fn threads(&self) -> &[ThreadItem] {
        &self.threads
    }

    /// Selected thread id.
    #[must_use]
    pub const fn selected_thread(&self) -> Option<i64> {
        self.selected_thread
    }

    /// Frames of the selected thread.
    #[must_use]
    pub fn frames(&self) -> &[FrameItem] {
        &self.frames
    }

    /// Index of the selected frame.
    #[must_use]
    pub const fn selected_frame_index(&self) -> usize {
        self.selected_frame
    }

    /// Selected frame.
    #[must_use]
    pub fn selected_frame(&self) -> Option<&FrameItem> {
        self.frames.get(self.selected_frame)
    }

    /// Flattened *Variables* tree.
    #[must_use]
    pub fn variable_rows(&self) -> &[VariableRow] {
        &self.variables
    }

    /// *Watches*.
    #[must_use]
    pub fn watches(&self) -> &[WatchItem] {
        &self.watches
    }

    /// Debuggee console.
    #[must_use]
    pub fn console(&self) -> &[String] {
        &self.console
    }

    /// Location of the execution point (top frame of the suspended thread).
    #[must_use]
    pub fn execution_point(&self) -> Option<SourceLocation> {
        if !self.status.is_suspended() {
            return None;
        }
        let frame = self.selected_frame()?;
        Some(SourceLocation {
            path: frame.path.clone()?,
            line: frame.line,
        })
    }

    /// Applies an update of `session_id`; stale updates of older sessions are ignored.
    ///
    /// Returns follow-up commands (e.g. loading the stack after a suspension).
    pub fn apply(&mut self, session_id: u64, update: DebugUpdate) -> Vec<DebugCommand> {
        if session_id != self.session_id {
            return Vec::new();
        }
        let mut follow_up = Vec::new();
        match update {
            DebugUpdate::Status(status) => {
                if let DebuggerStatus::Suspended {
                    thread_id: Some(thread_id),
                    ..
                } = &status
                {
                    self.selected_thread = Some(*thread_id);
                    follow_up.push(DebugCommand::LoadStack {
                        thread_id: *thread_id,
                    });
                }
                if !status.is_suspended() {
                    self.frames.clear();
                    self.selected_frame = 0;
                    self.variables.clear();
                }
                if matches!(status, DebuggerStatus::Terminated { .. }) {
                    self.threads.clear();
                    self.selected_thread = None;
                }
                self.status = status;
            }
            DebugUpdate::Threads(threads) => {
                if self
                    .selected_thread
                    .is_none_or(|id| !threads.iter().any(|t| t.id == id))
                {
                    self.selected_thread = threads.first().map(|t| t.id);
                }
                self.threads = threads;
            }
            DebugUpdate::Stack { thread_id, frames } => {
                if Some(thread_id) == self.selected_thread {
                    self.frames = frames;
                    self.selected_frame = 0;
                    self.variables.clear();
                    if let Some(frame) = self.frames.first() {
                        follow_up.push(DebugCommand::LoadScopes { frame_id: frame.id });
                    }
                    follow_up.extend(self.refresh_watches());
                }
            }
            DebugUpdate::Scopes { frame_id, scopes } => {
                if self.selected_frame().map(|f| f.id) == Some(frame_id) {
                    self.variables = scopes
                        .into_iter()
                        .map(|item| VariableRow::Value {
                            depth: 0,
                            item,
                            expanded: false,
                        })
                        .collect();
                    // Expand the first scope (`Local`) like IntelliJ.
                    follow_up.extend(self.toggle_variable(0));
                }
            }
            DebugUpdate::Variables {
                reference,
                start,
                items,
            } => self.insert_children(reference, start, items),
            DebugUpdate::Evaluated { target, result } => match target {
                EvaluateTarget::Dialog => {
                    if let Some(modal) = &mut self.evaluate {
                        modal.set_result(result);
                    }
                }
                EvaluateTarget::Watch(index) => {
                    if let Some(watch) = self.watches.get_mut(index) {
                        watch.result = Some(result.map(|item| item.value));
                    }
                }
            },
            DebugUpdate::BreakpointVerified {
                file,
                line,
                verified,
            } => {
                if let Some(spec) = self
                    .breakpoints
                    .get_mut(&file)
                    .and_then(|lines| lines.get_mut(&line))
                {
                    spec.verified = Some(verified);
                }
            }
            DebugUpdate::Output(text) => {
                self.console
                    .extend(text.lines().map(str::to_owned).filter(|l| !l.is_empty()));
                if self.console.len() > MAX_CONSOLE_LINES {
                    let excess = self.console.len() - MAX_CONSOLE_LINES;
                    self.console.drain(..excess);
                }
            }
        }
        follow_up
    }

    // -- User actions -------------------------------------------------------

    /// *Resume Program*: `None` unless suspended.
    #[must_use]
    pub fn resume_command(&self) -> Option<DebugCommand> {
        self.suspended_thread()
            .map(|thread_id| DebugCommand::Resume { thread_id })
    }

    /// Step command: `None` unless suspended.
    #[must_use]
    pub fn step_command(&self, kind: DebugStep) -> Option<DebugCommand> {
        self.suspended_thread()
            .map(|thread_id| DebugCommand::Step { kind, thread_id })
    }

    fn suspended_thread(&self) -> Option<i64> {
        if self.status.is_suspended() {
            self.selected_thread
        } else {
            None
        }
    }

    /// Selects a thread; returns the command loading its stack.
    pub fn select_thread(&mut self, index: usize) -> Option<DebugCommand> {
        let thread = self.threads.get(index)?;
        self.selected_thread = Some(thread.id);
        self.frames.clear();
        self.variables.clear();
        self.status
            .is_suspended()
            .then_some(DebugCommand::LoadStack {
                thread_id: thread.id,
            })
    }

    /// Selects a frame; returns the command loading its scopes. *Evaluate Expression* and
    /// watches then run in the context of this frame.
    pub fn select_frame(&mut self, index: usize) -> Vec<DebugCommand> {
        let Some(frame) = self.frames.get(index) else {
            return Vec::new();
        };
        let frame_id = frame.id;
        self.selected_frame = index;
        self.variables.clear();
        let mut commands = vec![DebugCommand::LoadScopes { frame_id }];
        commands.extend(self.refresh_watches());
        commands
    }

    /// Expands or collapses variable row `index`; returns the fetch command for children
    /// that were never loaded.
    pub fn toggle_variable(&mut self, index: usize) -> Option<DebugCommand> {
        let Some(VariableRow::Value {
            depth,
            item,
            expanded,
        }) = self.variables.get_mut(index)
        else {
            return None;
        };
        if item.reference <= 0 {
            return None;
        }
        let depth = *depth;
        if *expanded {
            *expanded = false;
            let end = self.subtree_end(index);
            self.variables.drain(index + 1..end);
            return None;
        }
        *expanded = true;
        let reference = item.reference;
        let indexed = item.indexed.is_some_and(|n| n > 0);
        self.variables
            .insert(index + 1, VariableRow::Loading { depth: depth + 1 });
        Some(DebugCommand::LoadVariables {
            reference,
            start: 0,
            count: if indexed { VARIABLES_PAGE_SIZE } else { 0 },
            indexed,
        })
    }

    /// Replaces the *Load more* row `index` with a loading row; returns the page request.
    pub fn load_more(&mut self, index: usize) -> Option<DebugCommand> {
        let Some(&VariableRow::More {
            depth,
            reference,
            start,
            ..
        }) = self.variables.get(index)
        else {
            return None;
        };
        self.variables[index] = VariableRow::Loading { depth };
        Some(DebugCommand::LoadVariables {
            reference,
            start,
            count: VARIABLES_PAGE_SIZE,
            indexed: true,
        })
    }

    fn subtree_end(&self, index: usize) -> usize {
        let depth = self.variables[index].depth();
        self.variables[index + 1..]
            .iter()
            .position(|row| row.depth() <= depth)
            .map_or(self.variables.len(), |offset| index + 1 + offset)
    }

    fn insert_children(&mut self, reference: i64, start: usize, items: Vec<VariableItem>) {
        let Some(parent) = self.variables.iter().position(|row| {
            matches!(row, VariableRow::Value { item, expanded: true, .. } if item.reference == reference)
        }) else {
            return;
        };
        let VariableRow::Value {
            depth,
            item: parent_item,
            ..
        } = &self.variables[parent]
        else {
            return;
        };
        let depth = depth + 1;
        let total = parent_item.indexed.unwrap_or(0);
        let Some(slot) = self.variables[parent + 1..]
            .iter()
            .take_while(|row| row.depth() >= depth)
            .position(|row| matches!(row, VariableRow::Loading { depth: d } if *d == depth))
            .map(|offset| parent + 1 + offset)
        else {
            return;
        };
        let loaded = items.len();
        let mut rows: Vec<VariableRow> = items
            .into_iter()
            .map(|item| VariableRow::Value {
                depth,
                item,
                expanded: false,
            })
            .collect();
        let next = start + loaded;
        if total > 0 && loaded > 0 && next < total {
            rows.push(VariableRow::More {
                depth,
                reference,
                start: next,
                total,
            });
        }
        self.variables.splice(slot..=slot, rows);
    }

    // -- Watches & evaluation ----------------------------------------------

    /// Adds a watch; returns its evaluation command when suspended.
    pub fn add_watch(&mut self, expression: impl Into<String>) -> Option<DebugCommand> {
        let expression = expression.into().trim().to_owned();
        if expression.is_empty() {
            return None;
        }
        self.watches.push(WatchItem {
            expression: expression.clone(),
            result: None,
        });
        let index = self.watches.len() - 1;
        self.status.is_suspended().then(|| DebugCommand::Evaluate {
            expression,
            frame_id: self.selected_frame().map(|f| f.id),
            target: EvaluateTarget::Watch(index),
        })
    }

    /// Removes watch `index`.
    pub fn remove_watch(&mut self, index: usize) {
        if index < self.watches.len() {
            self.watches.remove(index);
        }
    }

    fn refresh_watches(&mut self) -> Vec<DebugCommand> {
        let frame_id = self.selected_frame().map(|f| f.id);
        self.watches
            .iter_mut()
            .enumerate()
            .map(|(index, watch)| {
                watch.result = None;
                DebugCommand::Evaluate {
                    expression: watch.expression.clone(),
                    frame_id,
                    target: EvaluateTarget::Watch(index),
                }
            })
            .collect()
    }

    /// Opens *Evaluate Expression* (`Alt+F8`) prefilled with `selection`.
    pub fn open_evaluate(&mut self, selection: Option<&str>) {
        let names = self.completion_names();
        let mut modal = self.evaluate.take().unwrap_or_default();
        modal.open(selection, names);
        self.evaluate = Some(modal);
    }

    /// Closes the dialog.
    pub fn close_evaluate(&mut self) {
        if let Some(modal) = &mut self.evaluate {
            modal.close();
        }
    }

    /// The *Evaluate Expression* dialog, while open.
    #[must_use]
    pub fn evaluate_modal(&self) -> Option<&EvaluateModal> {
        self.evaluate.as_ref().filter(|m| m.is_open())
    }

    /// Mutable dialog access for key handling.
    pub fn evaluate_modal_mut(&mut self) -> Option<&mut EvaluateModal> {
        self.evaluate.as_mut().filter(|m| m.is_open())
    }

    /// Evaluates the dialog expression in the **selected** frame.
    pub fn evaluate_command(&mut self) -> Option<DebugCommand> {
        let frame_id = self.selected_frame().map(|f| f.id);
        let suspended = self.status.is_suspended();
        let modal = self.evaluate.as_mut().filter(|m| m.is_open())?;
        let expression = modal.submit()?;
        if !suspended {
            modal.set_result(Err(
                "The program is not suspended: evaluation needs a selected stack frame".to_owned(),
            ));
            return None;
        }
        Some(DebugCommand::Evaluate {
            expression,
            frame_id,
            target: EvaluateTarget::Dialog,
        })
    }

    /// Names in scope offered by the dialog's completion popup.
    fn completion_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .variables
            .iter()
            .filter_map(|row| match row {
                VariableRow::Value { depth: 1, item, .. } => Some(item.name.clone()),
                _ => None,
            })
            .chain(self.watches.iter().map(|w| w.expression.clone()))
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::too_many_lines,
        reason = "test assertions and scenario tests"
    )]

    use super::*;

    fn var(name: &str, reference: i64, indexed: Option<usize>) -> VariableItem {
        VariableItem {
            name: name.to_owned(),
            value: format!("{name}-value"),
            reference,
            indexed,
            ..VariableItem::default()
        }
    }

    fn suspended(model: &mut DebuggerModel) -> Vec<DebugCommand> {
        let id = model.session_id();
        model.apply(
            id,
            DebugUpdate::Threads(vec![ThreadItem {
                id: 1,
                name: "main".to_owned(),
            }]),
        );
        model.apply(
            id,
            DebugUpdate::Status(DebuggerStatus::Suspended {
                reason: "breakpoint".to_owned(),
                thread_id: Some(1),
            }),
        )
    }

    fn frames() -> Vec<FrameItem> {
        vec![
            FrameItem {
                id: 10,
                name: "Orders.total".to_owned(),
                path: Some(PathBuf::from("/p/Orders.java")),
                line: 12,
            },
            FrameItem {
                id: 11,
                name: "Orders.main".to_owned(),
                path: Some(PathBuf::from("/p/Orders.java")),
                line: 30,
            },
        ]
    }

    #[test]
    fn breakpoints_toggle_and_sync_only_while_a_session_is_active() {
        let mut model = DebuggerModel::default();
        let file = Path::new("/p/Orders.java");
        assert_eq!(model.toggle_breakpoint(file, 12), None);
        assert_eq!(
            model.set_condition(file, 12, Some("i == 3".to_owned())),
            None
        );
        assert_eq!(
            model.set_log_message(file, 20, Some("i={i}".to_owned())),
            None
        );
        let icons = model.gutter_icons(file);
        assert_eq!(icons.get(&11), Some(&BreakpointIcon::Conditional));
        assert_eq!(icons.get(&19), Some(&BreakpointIcon::Logpoint));

        let DebugCommand::Start(request) = model.start("Orders", "com.acme.Orders", None, None)
        else {
            panic!("start");
        };
        assert_eq!(request.breakpoints[file].len(), 2);
        let Some(DebugCommand::SetBreakpoints { breakpoints, .. }) =
            model.toggle_breakpoint(file, 20)
        else {
            panic!("sync while active");
        };
        assert_eq!(breakpoints.keys().copied().collect::<Vec<_>>(), [12]);
        let id = model.session_id();
        model.apply(
            id,
            DebugUpdate::BreakpointVerified {
                file: file.to_path_buf(),
                line: 12,
                verified: false,
            },
        );
        assert_eq!(
            model.gutter_icons(file).get(&11),
            Some(&BreakpointIcon::Invalid)
        );
        model.toggle_breakpoint(file, 12);
        assert!(model.breakpoints_for(file).is_none());
    }

    #[test]
    fn suspension_loads_stack_scopes_and_pages_large_arrays() {
        let mut model = DebuggerModel::default();
        model.start("Orders", "com.acme.Orders", None, None);
        assert!(model.resume_command().is_none());
        let id = model.session_id();
        assert_eq!(
            suspended(&mut model),
            [DebugCommand::LoadStack { thread_id: 1 }]
        );
        assert_eq!(
            model.step_command(DebugStep::Over),
            Some(DebugCommand::Step {
                kind: DebugStep::Over,
                thread_id: 1
            })
        );
        let follow = model.apply(
            id,
            DebugUpdate::Stack {
                thread_id: 1,
                frames: frames(),
            },
        );
        assert_eq!(follow, [DebugCommand::LoadScopes { frame_id: 10 }]);
        assert_eq!(
            model.execution_point(),
            Some(SourceLocation {
                path: PathBuf::from("/p/Orders.java"),
                line: 12
            })
        );
        let follow = model.apply(
            id,
            DebugUpdate::Scopes {
                frame_id: 10,
                scopes: vec![var("Local", 5, None)],
            },
        );
        assert_eq!(
            follow,
            [DebugCommand::LoadVariables {
                reference: 5,
                start: 0,
                count: 0,
                indexed: false
            }]
        );
        model.apply(
            id,
            DebugUpdate::Variables {
                reference: 5,
                start: 0,
                items: vec![var("orders", 9, Some(250)), var("total", 0, None)],
            },
        );
        assert_eq!(model.variable_rows().len(), 3);

        // Large array: first page of 100 plus a *Load more* row.
        assert_eq!(
            model.toggle_variable(1),
            Some(DebugCommand::LoadVariables {
                reference: 9,
                start: 0,
                count: VARIABLES_PAGE_SIZE,
                indexed: true
            })
        );
        let page: Vec<VariableItem> = (0..100).map(|i| var(&format!("[{i}]"), 0, None)).collect();
        model.apply(
            id,
            DebugUpdate::Variables {
                reference: 9,
                start: 0,
                items: page,
            },
        );
        let more = model.variable_rows().len() - 2;
        assert_eq!(
            model.variable_rows()[more],
            VariableRow::More {
                depth: 2,
                reference: 9,
                start: 100,
                total: 250
            }
        );
        assert_eq!(
            model.load_more(more),
            Some(DebugCommand::LoadVariables {
                reference: 9,
                start: 100,
                count: VARIABLES_PAGE_SIZE,
                indexed: true
            })
        );
        // Collapsing removes the whole subtree.
        model.toggle_variable(1);
        assert_eq!(model.variable_rows().len(), 3);

        // Resuming clears frames and variables.
        model.apply(id, DebugUpdate::Status(DebuggerStatus::Running));
        assert_eq!(model.frames().len(), 0);
        assert!(model.execution_point().is_none());
        // Updates of an older session are ignored.
        assert_eq!(
            model
                .apply(id - 1, DebugUpdate::Output("x".to_owned()))
                .len(),
            0
        );
        assert_eq!(model.console().len(), 0);
    }

    #[test]
    fn evaluate_and_watches_use_the_selected_frame() {
        let mut model = DebuggerModel::default();
        model.start("Orders", "com.acme.Orders", None, None);
        let id = model.session_id();
        model.add_watch("total");
        suspended(&mut model);
        let follow = model.apply(
            id,
            DebugUpdate::Stack {
                thread_id: 1,
                frames: frames(),
            },
        );
        assert!(follow.contains(&DebugCommand::Evaluate {
            expression: "total".to_owned(),
            frame_id: Some(10),
            target: EvaluateTarget::Watch(0)
        }));
        let commands = model.select_frame(1);
        assert_eq!(commands[0], DebugCommand::LoadScopes { frame_id: 11 });
        assert!(commands.contains(&DebugCommand::Evaluate {
            expression: "total".to_owned(),
            frame_id: Some(11),
            target: EvaluateTarget::Watch(0)
        }));

        model.open_evaluate(Some("total * 2"));
        assert_eq!(
            model.evaluate_command(),
            Some(DebugCommand::Evaluate {
                expression: "total * 2".to_owned(),
                frame_id: Some(11),
                target: EvaluateTarget::Dialog
            })
        );
        model.apply(
            id,
            DebugUpdate::Evaluated {
                target: EvaluateTarget::Dialog,
                result: Ok(var("total * 2", 0, None)),
            },
        );
        assert!(matches!(
            model.evaluate_modal().unwrap().state(),
            EvaluateState::Result(_)
        ));
        model.apply(
            id,
            DebugUpdate::Evaluated {
                target: EvaluateTarget::Watch(0),
                result: Err("not found".to_owned()),
            },
        );
        assert_eq!(model.watches()[0].result, Some(Err("not found".to_owned())));
        model.close_evaluate();
        assert!(model.evaluate_modal().is_none());

        // Not suspended: the dialog explains instead of sending a request.
        model.apply(id, DebugUpdate::Status(DebuggerStatus::Running));
        model.open_evaluate(Some("x"));
        assert_eq!(model.evaluate_command(), None);
        assert!(matches!(
            model.evaluate_modal().unwrap().state(),
            EvaluateState::Error(_)
        ));
    }
}
