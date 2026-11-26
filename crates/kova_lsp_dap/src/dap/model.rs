//! Headless debugger data model (`KOVA-041`): breakpoints cached per source file, the DAP
//! payload types Kova reads (`threads`, `stackTrace`, `scopes`, `variables`, `evaluate`) and
//! the [`SessionState`] reducer turning adapter events into [`DebugEvent`]s.
//!
//! Nothing here performs I/O, so the session lifecycle is unit-testable without an adapter.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::dap::protocol::DapEvent;

// ---------------------------------------------------------------------------
// Breakpoints
// ---------------------------------------------------------------------------

/// Kind of a source breakpoint, derived from its options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BreakpointKind {
    /// Plain line breakpoint.
    Line,
    /// Suspends only when the Java / Kotlin `condition` evaluates to `true`.
    Conditional,
    /// Logs `log_message` without suspending (*logpoint*).
    Logpoint,
}

/// A breakpoint set by the user in a source file.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceBreakpoint {
    /// 1-based line.
    pub line: u32,
    /// Boolean Java / Kotlin expression; the adapter suspends only when it holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Hit count condition (e.g. `">= 3"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
    /// Log message with `{expression}` interpolation; turns the breakpoint into a logpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_message: Option<String>,
}

impl SourceBreakpoint {
    /// A plain line breakpoint.
    #[must_use]
    pub const fn line(line: u32) -> Self {
        Self {
            line,
            condition: None,
            hit_condition: None,
            log_message: None,
        }
    }

    /// A conditional breakpoint.
    #[must_use]
    pub fn conditional(line: u32, condition: impl Into<String>) -> Self {
        Self {
            condition: non_empty(&condition.into()),
            ..Self::line(line)
        }
    }

    /// A logpoint.
    #[must_use]
    pub fn logpoint(line: u32, message: impl Into<String>) -> Self {
        Self {
            log_message: non_empty(&message.into()),
            ..Self::line(line)
        }
    }

    /// Kind derived from the options (a log message wins over a condition).
    #[must_use]
    pub const fn kind(&self) -> BreakpointKind {
        if self.log_message.is_some() {
            BreakpointKind::Logpoint
        } else if self.condition.is_some() {
            BreakpointKind::Conditional
        } else {
            BreakpointKind::Line
        }
    }
}

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Breakpoint as acknowledged by the adapter (`setBreakpoints` response or `breakpoint` event).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BreakpointStatus {
    /// Adapter-assigned id (used by `breakpoint` events and `hitBreakpointIds`).
    #[serde(default)]
    pub id: Option<i64>,
    /// Whether the JVM accepted the breakpoint (class loaded, line has code).
    #[serde(default)]
    pub verified: bool,
    /// Actual line (the adapter may move it to the next executable line).
    #[serde(default)]
    pub line: Option<u32>,
    /// Why the breakpoint is not verified.
    #[serde(default)]
    pub message: Option<String>,
}

/// Breakpoints cached per source file.
///
/// DAP `setBreakpoints` **replaces** the whole set of a source, so the store always
/// produces the complete list of a file ([`Self::set_breakpoints_arguments`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BreakpointStore {
    files: BTreeMap<PathBuf, BTreeMap<u32, SourceBreakpoint>>,
}

impl BreakpointStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a line breakpoint, or removes the existing one at `line`.
    ///
    /// Returns `true` when a breakpoint now exists at `line`.
    pub fn toggle(&mut self, file: &Path, line: u32) -> bool {
        let lines = self.files.entry(file.to_path_buf()).or_default();
        let added = if lines.remove(&line).is_some() {
            false
        } else {
            lines.insert(line, SourceBreakpoint::line(line));
            true
        };
        if lines.is_empty() {
            self.files.remove(file);
        }
        added
    }

    /// Inserts or replaces the breakpoint at `breakpoint.line`.
    pub fn upsert(&mut self, file: &Path, breakpoint: SourceBreakpoint) {
        self.files
            .entry(file.to_path_buf())
            .or_default()
            .insert(breakpoint.line, breakpoint);
    }

    /// Removes the breakpoint at `line`; returns whether one existed.
    pub fn remove(&mut self, file: &Path, line: u32) -> bool {
        let Some(lines) = self.files.get_mut(file) else {
            return false;
        };
        let removed = lines.remove(&line).is_some();
        if lines.is_empty() {
            self.files.remove(file);
        }
        removed
    }

    /// Breakpoint at `line`, if any.
    #[must_use]
    pub fn get(&self, file: &Path, line: u32) -> Option<&SourceBreakpoint> {
        self.files.get(file)?.get(&line)
    }

    /// Breakpoints of `file`, ordered by line.
    pub fn for_file(&self, file: &Path) -> impl Iterator<Item = &SourceBreakpoint> {
        self.files.get(file).into_iter().flat_map(BTreeMap::values)
    }

    /// Files with at least one breakpoint.
    pub fn files(&self) -> impl Iterator<Item = &Path> {
        self.files.keys().map(PathBuf::as_path)
    }

    /// Total number of breakpoints.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.values().map(BTreeMap::len).sum()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// `setBreakpoints` arguments carrying the complete set of `file` (empty clears it).
    #[must_use]
    pub fn set_breakpoints_arguments(&self, file: &Path) -> Value {
        let breakpoints: Vec<&SourceBreakpoint> = self.for_file(file).collect();
        json!({
            "source": {
                "name": file.file_name().map(|n| n.to_string_lossy().into_owned()),
                "path": file.display().to_string(),
            },
            "breakpoints": breakpoints,
            "lines": breakpoints.iter().map(|bp| bp.line).collect::<Vec<_>>(),
            "sourceModified": false,
        })
    }
}

// ---------------------------------------------------------------------------
// Inspection payloads
// ---------------------------------------------------------------------------

/// A JVM thread (`threads` response).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DapThread {
    /// Thread id.
    pub id: i64,
    /// Thread name (`main`, `pool-1-thread-2`, …).
    pub name: String,
}

/// Source reference of a stack frame.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DapSource {
    /// File name (`OrderService.java`).
    #[serde(default)]
    pub name: Option<String>,
    /// Absolute path, when the source is on disk.
    #[serde(default)]
    pub path: Option<String>,
    /// Non-zero when the content must be fetched through `source` (decompiled classes).
    #[serde(default)]
    pub source_reference: Option<i64>,
}

/// One stack frame (`stackTrace` response).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DapStackFrame {
    /// Frame id (valid while the thread stays suspended).
    pub id: i64,
    /// Method label (`OrderService.create(Order)`).
    pub name: String,
    /// Source of the frame.
    #[serde(default)]
    pub source: Option<DapSource>,
    /// 1-based line.
    #[serde(default)]
    pub line: u32,
    /// 1-based column.
    #[serde(default)]
    pub column: u32,
    /// `normal`, `label` or `subtle` (library frames).
    #[serde(default)]
    pub presentation_hint: Option<String>,
}

impl DapStackFrame {
    /// Path of the frame source on disk.
    #[must_use]
    pub fn path(&self) -> Option<PathBuf> {
        self.source
            .as_ref()
            .and_then(|s| s.path.as_deref())
            .map(PathBuf::from)
    }
}

/// One page of a `stackTrace` response.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StackTracePage {
    /// Frames, innermost first.
    pub frames: Vec<DapStackFrame>,
    /// Total frames of the thread, when the adapter reports it.
    pub total_frames: Option<usize>,
}

/// A variable scope of a frame (`Locals`, `this`, `Static`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DapScope {
    /// Scope name.
    pub name: String,
    /// Reference passed to `variables`.
    pub variables_reference: i64,
    /// Number of named children, when known.
    #[serde(default)]
    pub named_variables: Option<usize>,
    /// Number of indexed children, when known.
    #[serde(default)]
    pub indexed_variables: Option<usize>,
    /// Whether fetching the scope is expensive (fetched lazily on expand).
    #[serde(default)]
    pub expensive: bool,
}

/// A variable or object field (`variables` response).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DapVariable {
    /// Name (`order`, `[3]`, `this`).
    pub name: String,
    /// Rendered value (`Order@1234`, `"PLN"`, `42`).
    pub value: String,
    /// Java type, when reported.
    #[serde(default, rename = "type")]
    pub type_name: Option<String>,
    /// Non-zero when the variable has children to fetch lazily.
    #[serde(default)]
    pub variables_reference: i64,
    /// Number of named children (fields), when known.
    #[serde(default)]
    pub named_variables: Option<usize>,
    /// Number of indexed children (array / collection elements), when known.
    #[serde(default)]
    pub indexed_variables: Option<usize>,
    /// Expression that evaluates to this variable (used for *Add to Watches*).
    #[serde(default)]
    pub evaluate_name: Option<String>,
}

impl DapVariable {
    /// Whether the variable can be expanded.
    #[must_use]
    pub const fn has_children(&self) -> bool {
        self.variables_reference > 0
    }
}

/// Which children of a container to fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VariablesFilter {
    /// Indexed and named children.
    All,
    /// Only array / collection elements.
    Indexed,
    /// Only fields.
    Named,
}

/// A page request for `variables` (large arrays and collections are never fetched whole).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VariablesPage {
    /// Index of the first child.
    pub start: usize,
    /// Maximum number of children (`0` = adapter default, i.e. everything).
    pub count: usize,
    /// Indexed / named filter.
    pub filter: VariablesFilter,
}

impl VariablesPage {
    /// Default page size of indexed children in the *Variables* tree.
    pub const DEFAULT_PAGE_SIZE: usize = 100;

    /// First page of [`Self::DEFAULT_PAGE_SIZE`] children.
    #[must_use]
    pub const fn first() -> Self {
        Self {
            start: 0,
            count: Self::DEFAULT_PAGE_SIZE,
            filter: VariablesFilter::All,
        }
    }

    /// Page `index` (0-based) of indexed children.
    #[must_use]
    pub const fn indexed(index: usize, size: usize) -> Self {
        Self {
            start: index * size,
            count: size,
            filter: VariablesFilter::Indexed,
        }
    }

    /// `variables` arguments for `reference`.
    #[must_use]
    pub fn arguments(&self, reference: i64) -> Value {
        let mut args = json!({ "variablesReference": reference });
        if self.count > 0 {
            args["start"] = json!(self.start);
            args["count"] = json!(self.count);
        }
        match self.filter {
            VariablesFilter::All => {}
            VariablesFilter::Indexed => args["filter"] = json!("indexed"),
            VariablesFilter::Named => args["filter"] = json!("named"),
        }
        args
    }
}

/// Where an expression is evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EvaluateContext {
    /// *Evaluate Expression* (`Alt+F8`).
    Repl,
    /// *Watches* panel.
    Watch,
    /// Editor hover / inline values.
    Hover,
}

impl EvaluateContext {
    /// DAP `context` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Repl => "repl",
            Self::Watch => "watch",
            Self::Hover => "hover",
        }
    }
}

/// Result of `evaluate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluateResult {
    /// Rendered result.
    pub result: String,
    /// Java type, when reported.
    #[serde(default, rename = "type")]
    pub type_name: Option<String>,
    /// Non-zero when the result is an object with children.
    #[serde(default)]
    pub variables_reference: i64,
}

// ---------------------------------------------------------------------------
// Session state machine
// ---------------------------------------------------------------------------

/// Lifecycle of a debug session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DebugSessionState {
    /// Nothing sent yet.
    NotStarted,
    /// `initialize` sent, waiting for the capabilities.
    Initializing,
    /// `launch` / `attach` sent, sending breakpoints until `configurationDone`.
    Configuring,
    /// Debuggee running.
    Running,
    /// At least one thread suspended.
    Stopped,
    /// Debuggee exited or the session was disconnected.
    Terminated,
}

impl DebugSessionState {
    /// Whether execution-control commands (`next`, `stepIn`, …) are allowed.
    #[must_use]
    pub const fn is_suspended(self) -> bool {
        matches!(self, Self::Stopped)
    }

    /// Whether the session is still alive.
    #[must_use]
    pub const fn is_active(self) -> bool {
        !matches!(self, Self::NotStarted | Self::Terminated)
    }
}

impl fmt::Display for DebugSessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotStarted => "not-started",
            Self::Initializing => "initializing",
            Self::Configuring => "configuring",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Terminated => "terminated",
        })
    }
}

/// Why a thread suspended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoppedInfo {
    /// `breakpoint`, `step`, `pause`, `exception`, `entry`, …
    pub reason: String,
    /// Suspended thread.
    pub thread_id: Option<i64>,
    /// Whether every thread was suspended.
    pub all_threads_stopped: bool,
    /// Extra text (exception message).
    pub description: Option<String>,
    /// Ids of the breakpoints that were hit.
    pub hit_breakpoint_ids: Vec<i64>,
}

/// Category of debuggee output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputCategory {
    /// Program `System.out`.
    Stdout,
    /// Program `System.err`.
    Stderr,
    /// Adapter / logpoint console messages.
    Console,
}

/// A debugger event after the session state was updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebugEvent {
    /// The adapter is ready for breakpoints (`initialized`).
    Initialized,
    /// A thread suspended.
    Stopped(StoppedInfo),
    /// Execution resumed.
    Continued {
        /// Resumed thread.
        thread_id: Option<i64>,
        /// Whether every thread resumed.
        all_threads_continued: bool,
    },
    /// A thread started or exited.
    Thread {
        /// `started` or `exited`.
        reason: String,
        /// Thread id.
        thread_id: i64,
    },
    /// Program or logpoint output.
    Output {
        /// Stream.
        category: OutputCategory,
        /// Text (usually newline-terminated).
        text: String,
    },
    /// The adapter changed a breakpoint (verified after class loading, moved, removed).
    Breakpoint {
        /// `changed`, `new` or `removed`.
        reason: String,
        /// New status.
        status: BreakpointStatus,
    },
    /// The debuggee exited.
    Exited {
        /// Process exit code.
        exit_code: i64,
    },
    /// The session ended.
    Terminated,
}

/// Synchronous session state updated from adapter events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    state: DebugSessionState,
    stopped: Option<StoppedInfo>,
    /// Incremented on every resume / step: cached frames and variable references of an older
    /// epoch are invalid.
    epoch: u64,
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionState {
    /// A fresh, not-started session.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: DebugSessionState::NotStarted,
            stopped: None,
            epoch: 0,
        }
    }

    /// Current lifecycle state.
    #[must_use]
    pub const fn state(&self) -> DebugSessionState {
        self.state
    }

    /// Last suspension, while stopped.
    #[must_use]
    pub const fn stopped(&self) -> Option<&StoppedInfo> {
        self.stopped.as_ref()
    }

    /// Suspension epoch (see [`Self::resume`]).
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Moves to `state` (used by the session for request-driven transitions).
    pub fn set_state(&mut self, state: DebugSessionState) {
        if self.state != DebugSessionState::Terminated {
            self.state = state;
        }
    }

    /// Marks the debuggee as running and invalidates every cached frame / variable.
    pub fn resume(&mut self) {
        self.epoch += 1;
        self.stopped = None;
        self.set_state(DebugSessionState::Running);
    }

    /// Applies one adapter event; returns the typed event, or `None` for events Kova ignores.
    pub fn apply(&mut self, event: &DapEvent) -> Option<DebugEvent> {
        let body = &event.body;
        match event.event.as_str() {
            "initialized" => Some(DebugEvent::Initialized),
            "stopped" => {
                let info = StoppedInfo {
                    reason: str_field(body, "reason").unwrap_or_else(|| "pause".to_owned()),
                    thread_id: body.get("threadId").and_then(Value::as_i64),
                    all_threads_stopped: body
                        .get("allThreadsStopped")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    description: str_field(body, "description").or_else(|| str_field(body, "text")),
                    hit_breakpoint_ids: body
                        .get("hitBreakpointIds")
                        .and_then(Value::as_array)
                        .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
                        .unwrap_or_default(),
                };
                self.epoch += 1;
                self.stopped = Some(info.clone());
                self.set_state(DebugSessionState::Stopped);
                Some(DebugEvent::Stopped(info))
            }
            "continued" => {
                self.resume();
                Some(DebugEvent::Continued {
                    thread_id: body.get("threadId").and_then(Value::as_i64),
                    all_threads_continued: body
                        .get("allThreadsContinued")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                })
            }
            "thread" => Some(DebugEvent::Thread {
                reason: str_field(body, "reason").unwrap_or_default(),
                thread_id: body.get("threadId").and_then(Value::as_i64)?,
            }),
            "output" => {
                let category = match body.get("category").and_then(Value::as_str) {
                    Some("stdout") => OutputCategory::Stdout,
                    Some("stderr") => OutputCategory::Stderr,
                    Some("telemetry") => return None,
                    _ => OutputCategory::Console,
                };
                Some(DebugEvent::Output {
                    category,
                    text: str_field(body, "output").unwrap_or_default(),
                })
            }
            "breakpoint" => Some(DebugEvent::Breakpoint {
                reason: str_field(body, "reason").unwrap_or_default(),
                status: serde_json::from_value(body.get("breakpoint")?.clone()).ok()?,
            }),
            "exited" => Some(DebugEvent::Exited {
                exit_code: body.get("exitCode").and_then(Value::as_i64).unwrap_or(0),
            }),
            "terminated" => {
                self.stopped = None;
                self.state = DebugSessionState::Terminated;
                Some(DebugEvent::Terminated)
            }
            _ => None,
        }
    }
}

fn str_field(body: &Value, key: &str) -> Option<String> {
    body.get(key).and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "test assertions")]

    use super::*;

    #[test]
    fn toggle_adds_and_removes_and_drops_empty_files() {
        let mut store = BreakpointStore::new();
        let file = Path::new("/p/src/Order.java");
        assert!(store.toggle(file, 10));
        assert!(store.toggle(file, 4));
        assert_eq!(
            store.for_file(file).map(|b| b.line).collect::<Vec<_>>(),
            [4, 10]
        );
        assert!(!store.toggle(file, 10));
        assert!(!store.toggle(file, 4));
        assert!(store.is_empty());
        assert_eq!(store.files().count(), 0);
    }

    #[test]
    fn set_breakpoints_arguments_carry_the_full_set_with_condition_and_log_message() {
        let mut store = BreakpointStore::new();
        let file = Path::new("/p/src/Order.java");
        store.upsert(file, SourceBreakpoint::conditional(12, " i == 3 "));
        store.upsert(file, SourceBreakpoint::logpoint(20, "total={total}"));
        store.upsert(file, SourceBreakpoint::line(5));
        let args = store.set_breakpoints_arguments(file);
        assert_eq!(args["source"]["path"], "/p/src/Order.java");
        assert_eq!(args["source"]["name"], "Order.java");
        assert_eq!(args["lines"], json!([5, 12, 20]));
        assert_eq!(args["breakpoints"][0], json!({"line": 5}));
        assert_eq!(
            args["breakpoints"][1],
            json!({"line": 12, "condition": "i == 3"})
        );
        assert_eq!(
            args["breakpoints"][2],
            json!({"line": 20, "logMessage": "total={total}"})
        );
        assert_eq!(
            store.get(file, 12).map(SourceBreakpoint::kind),
            Some(BreakpointKind::Conditional)
        );
        assert_eq!(
            store.get(file, 20).map(SourceBreakpoint::kind),
            Some(BreakpointKind::Logpoint)
        );
        assert_eq!(
            SourceBreakpoint::conditional(1, "  ").kind(),
            BreakpointKind::Line
        );
    }

    #[test]
    fn variables_page_arguments() {
        assert_eq!(
            VariablesPage::first().arguments(7),
            json!({"variablesReference": 7, "start": 0, "count": 100})
        );
        assert_eq!(
            VariablesPage::indexed(2, 50).arguments(7),
            json!({"variablesReference": 7, "start": 100, "count": 50, "filter": "indexed"})
        );
    }

    #[test]
    fn state_machine_follows_stopped_continued_terminated() {
        let mut state = SessionState::new();
        state.set_state(DebugSessionState::Running);
        let epoch = state.epoch();
        let stopped = state
            .apply(&DapEvent::new(
                1,
                "stopped",
                json!({"reason": "breakpoint", "threadId": 1, "hitBreakpointIds": [3]}),
            ))
            .unwrap();
        assert!(matches!(stopped, DebugEvent::Stopped(ref s) if s.hit_breakpoint_ids == [3]));
        assert_eq!(state.state(), DebugSessionState::Stopped);
        assert!(state.epoch() > epoch);
        assert_eq!(state.stopped().and_then(|s| s.thread_id), Some(1));

        state.apply(&DapEvent::new(2, "continued", json!({"threadId": 1})));
        assert_eq!(state.state(), DebugSessionState::Running);
        assert!(state.stopped().is_none());

        assert_eq!(
            state.apply(&DapEvent::new(
                3,
                "output",
                json!({"category": "telemetry"})
            )),
            None
        );
        assert_eq!(
            state.apply(&DapEvent::new(4, "terminated", Value::Null)),
            Some(DebugEvent::Terminated)
        );
        state.set_state(DebugSessionState::Running);
        assert_eq!(state.state(), DebugSessionState::Terminated);
        assert!(!state.state().is_active());
    }
}
