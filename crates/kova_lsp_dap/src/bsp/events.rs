//! Adapter turning BSP notifications (`build/taskStart`, `build/taskProgress`,
//! `build/taskFinish`, `build/logMessage`, `build/publishDiagnostics`) into one unified stream of
//! [`BspEvent`]s (`KOVA-040`).
//!
//! Servers differ in how they nest test tasks: Gradle reports a class `test-start` whose
//! children are the method `test-start`s, sbt/Bloop report `Class#method` style display names,
//! and some servers attach `junit-style-test-case-data` to `test-finish`. The
//! [`BspEventMapper`] accepts all of these shapes.

use std::collections::HashMap;

use kova_tools::junit::{OutputStream, TestEvent, TestId, TestOutcome, TestStatus};
use serde_json::Value;

use crate::lsp::transport::JsonRpcNotification;

/// `StatusCode` of `buildTarget/compile` and `buildTarget/test`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BspStatusCode {
    /// `1` — the task succeeded.
    Ok,
    /// `2` — the task failed.
    Error,
    /// `3` — the task was cancelled.
    Cancelled,
}

impl BspStatusCode {
    /// Decodes the wire value (unknown values map to [`Self::Error`]).
    #[must_use]
    pub const fn from_code(code: i64) -> Self {
        match code {
            1 => Self::Ok,
            3 => Self::Cancelled,
            _ => Self::Error,
        }
    }

    /// Whether the task succeeded.
    #[must_use]
    pub const fn is_ok(self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// Severity of a `build/logMessage` / `build/showMessage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BspMessageLevel {
    /// `1`
    Error,
    /// `2`
    Warning,
    /// `3`
    Info,
    /// `4`
    Log,
}

impl BspMessageLevel {
    const fn from_code(code: i64) -> Self {
        match code {
            1 => Self::Error,
            2 => Self::Warning,
            3 => Self::Info,
            _ => Self::Log,
        }
    }
}

/// Unified event emitted while a BSP server builds and tests.
#[derive(Debug, Clone, PartialEq)]
pub enum BspEvent {
    /// Test tree update for the `JUnit` *Test Runner*.
    Test(TestEvent),
    /// Compile diagnostics for one document (`reset` replaces the previous ones).
    Diagnostics {
        /// Document URI.
        uri: String,
        /// Build target URI that produced them.
        build_target: Option<String>,
        /// LSP diagnostics.
        diagnostics: Vec<lsp_types::Diagnostic>,
        /// Whether to replace (`true`) or extend (`false`) the previous set for `uri`.
        reset: bool,
    },
    /// A user-visible message (`build/showMessage`, or a log not tied to a test).
    Message {
        /// Severity.
        level: BspMessageLevel,
        /// Text.
        message: String,
    },
    /// Progress of a long task (`build/taskProgress`).
    Progress {
        /// Task id.
        task: String,
        /// Optional message.
        message: Option<String>,
        /// Units done.
        progress: Option<u64>,
        /// Units total.
        total: Option<u64>,
    },
    /// A `compile-report` finished.
    CompileReport {
        /// Build target URI.
        target: Option<String>,
        /// Compile errors.
        errors: u64,
        /// Compile warnings.
        warnings: u64,
    },
}

#[derive(Debug, Clone)]
enum TaskKind {
    /// `test-task` for a build target; children inherit the suite name.
    TestTarget,
    /// `test-start` for a class or method.
    TestCase(TestId),
    /// Any other task (compile, …).
    Other,
}

#[derive(Debug, Clone)]
struct TaskInfo {
    kind: TaskKind,
    parent: Option<String>,
    suite: Option<String>,
    started_at: Option<i64>,
}

/// Stateful mapper from BSP notifications to [`BspEvent`]s.
#[derive(Debug, Default)]
pub struct BspEventMapper {
    tasks: HashMap<String, TaskInfo>,
    target_names: HashMap<String, String>,
}

impl BspEventMapper {
    /// Creates an empty mapper.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers display names of build targets, used as suite names (`uri → name`).
    pub fn set_target_names(&mut self, names: impl IntoIterator<Item = (String, String)>) {
        self.target_names.extend(names);
    }

    /// Maps one notification to zero or more events.
    pub fn map(&mut self, notification: &JsonRpcNotification) -> Vec<BspEvent> {
        let params = &notification.params;
        match notification.method.as_str() {
            "build/taskStart" => self.task_start(params),
            "build/taskProgress" => task_progress(params),
            "build/taskFinish" => self.task_finish(params),
            "build/logMessage" => self.log_message(params),
            "build/showMessage" => vec![BspEvent::Message {
                level: BspMessageLevel::from_code(params["type"].as_i64().unwrap_or(4)),
                message: str_field(params, "message").unwrap_or_default(),
            }],
            "build/publishDiagnostics" => publish_diagnostics(params),
            _ => Vec::new(),
        }
    }

    fn task_start(&mut self, params: &Value) -> Vec<BspEvent> {
        let Some(task_id) = task_id(params) else {
            return Vec::new();
        };
        let parent = parent_id(params);
        let inherited_suite = parent
            .as_ref()
            .and_then(|p| self.tasks.get(p))
            .and_then(|p| p.suite.clone());
        let data = &params["data"];
        let started_at = params["eventTime"].as_i64();
        match params["dataKind"].as_str() {
            Some("test-task") => {
                let uri = data["target"]["uri"].as_str().unwrap_or_default();
                let suite = self
                    .target_names
                    .get(uri)
                    .cloned()
                    .or_else(|| target_short_name(uri));
                self.tasks.insert(
                    task_id,
                    TaskInfo {
                        kind: TaskKind::TestTarget,
                        parent,
                        suite: suite.clone(),
                        started_at,
                    },
                );
                let text = format!(
                    "Testing {}",
                    suite.unwrap_or_else(|| "build target".to_owned())
                );
                vec![BspEvent::Test(TestEvent::Output {
                    id: None,
                    stream: OutputStream::Stdout,
                    text,
                })]
            }
            Some("test-start") => {
                let display = str_field(data, "displayName").unwrap_or_default();
                let parent_case = parent
                    .as_ref()
                    .and_then(|p| match &self.tasks.get(p)?.kind {
                        TaskKind::TestCase(id) => Some(id.clone()),
                        _ => None,
                    });
                let mut id = test_id_from(&display, parent_case.as_ref(), data);
                id.suite.clone_from(&inherited_suite);
                self.tasks.insert(
                    task_id,
                    TaskInfo {
                        kind: TaskKind::TestCase(id.clone()),
                        parent,
                        suite: inherited_suite,
                        started_at,
                    },
                );
                vec![BspEvent::Test(TestEvent::TestStarted(id))]
            }
            _ => {
                self.tasks.insert(
                    task_id,
                    TaskInfo {
                        kind: TaskKind::Other,
                        parent,
                        suite: inherited_suite,
                        started_at,
                    },
                );
                Vec::new()
            }
        }
    }

    fn task_finish(&mut self, params: &Value) -> Vec<BspEvent> {
        let id = task_id(params);
        let info = id.as_ref().and_then(|id| self.tasks.remove(id));
        let data = &params["data"];
        match params["dataKind"].as_str() {
            Some("test-finish") => {
                let display = str_field(data, "displayName").unwrap_or_default();
                let test_id = if let Some(TaskKind::TestCase(id)) = info.as_ref().map(|i| &i.kind) {
                    id.clone()
                } else {
                    let mut id = test_id_from(&display, None, data);
                    id.suite = info.as_ref().and_then(|i| i.suite.clone());
                    id
                };
                let status = match data["status"].as_i64() {
                    Some(1) => TestStatus::Passed,
                    Some(2) => TestStatus::Failed,
                    _ => TestStatus::Ignored,
                };
                let junit = (data["dataKind"].as_str() == Some("junit-style-test-case-data"))
                    .then(|| &data["data"]);
                let duration_ms = junit
                    .and_then(|d| d["time"].as_f64())
                    .map(seconds_to_ms)
                    .or_else(|| {
                        let start = info.as_ref()?.started_at?;
                        let end = params["eventTime"].as_i64()?;
                        u64::try_from(end - start).ok()
                    });
                let message = str_field(data, "message");
                let stacktrace = junit.and_then(|d| str_field(d, "fullError")).or_else(|| {
                    (status == TestStatus::Failed)
                        .then(|| message.clone())
                        .flatten()
                        .filter(|m| m.contains("\tat ") || m.contains("\n    at "))
                });
                let message = message.map(|m| m.lines().next().unwrap_or_default().to_owned());
                vec![BspEvent::Test(TestEvent::TestFinished {
                    id: test_id,
                    outcome: TestOutcome {
                        status,
                        duration_ms,
                        message,
                        stacktrace,
                    },
                })]
            }
            Some("test-report") => {
                let n = |key: &str| data[key].as_u64().unwrap_or(0);
                let text = format!(
                    "Tests: {} passed, {} failed, {} ignored, {} cancelled, {} skipped",
                    n("passed"),
                    n("failed"),
                    n("ignored"),
                    n("cancelled"),
                    n("skipped")
                );
                vec![BspEvent::Test(TestEvent::Output {
                    id: None,
                    stream: OutputStream::Stdout,
                    text,
                })]
            }
            Some("compile-report") => vec![BspEvent::CompileReport {
                target: data["target"]["uri"].as_str().map(str::to_owned),
                errors: data["errors"].as_u64().unwrap_or(0),
                warnings: data["warnings"].as_u64().unwrap_or(0),
            }],
            _ => Vec::new(),
        }
    }

    fn log_message(&self, params: &Value) -> Vec<BspEvent> {
        let level = BspMessageLevel::from_code(params["type"].as_i64().unwrap_or(4));
        let message = str_field(params, "message").unwrap_or_default();
        let owner = params["task"]["id"]
            .as_str()
            .and_then(|id| self.test_owner(id));
        let stream = if level == BspMessageLevel::Error {
            OutputStream::Stderr
        } else {
            OutputStream::Stdout
        };
        vec![BspEvent::Test(TestEvent::Output {
            id: owner,
            stream,
            text: message,
        })]
    }

    /// Nearest enclosing test case of task `id`.
    fn test_owner(&self, id: &str) -> Option<TestId> {
        let mut current = id.to_owned();
        for _ in 0..64 {
            let info = self.tasks.get(&current)?;
            if let TaskKind::TestCase(test) = &info.kind {
                return Some(test.clone());
            }
            current.clone_from(info.parent.as_ref()?);
        }
        None
    }
}

fn task_progress(params: &Value) -> Vec<BspEvent> {
    let Some(task) = task_id(params) else {
        return Vec::new();
    };
    vec![BspEvent::Progress {
        task,
        message: str_field(params, "message"),
        progress: params["progress"].as_u64(),
        total: params["total"].as_u64(),
    }]
}

fn publish_diagnostics(params: &Value) -> Vec<BspEvent> {
    let Some(uri) = params["textDocument"]["uri"].as_str() else {
        return Vec::new();
    };
    let diagnostics = params["diagnostics"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|d| serde_json::from_value(d.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    vec![BspEvent::Diagnostics {
        uri: uri.to_owned(),
        build_target: params["buildTarget"]["uri"].as_str().map(str::to_owned),
        diagnostics,
        reset: params["reset"].as_bool().unwrap_or(true),
    }]
}

fn task_id(params: &Value) -> Option<String> {
    params["taskId"]["id"].as_str().map(str::to_owned)
}

fn parent_id(params: &Value) -> Option<String> {
    params["taskId"]["parents"]
        .as_array()
        .and_then(|p| p.last())
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value[key].as_str().map(str::to_owned)
}

fn target_short_name(uri: &str) -> Option<String> {
    let trimmed = uri.trim_end_matches('/');
    let tail = trimmed.rsplit(['/', '?', '=']).next()?;
    (!tail.is_empty()).then(|| tail.to_owned())
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "test durations are non-negative and far below u64::MAX ms"
)]
fn seconds_to_ms(secs: f64) -> u64 {
    if secs.is_finite() && secs > 0.0 {
        (secs * 1000.0).round() as u64
    } else {
        0
    }
}

/// Derives a [`TestId`] from a BSP test display name.
///
/// Accepted shapes: `Class#method`, `method(Class)` (`JUnit` 4 description), a method whose
/// parent task is its class, `junit-style-test-case-data` with `className`, and a bare fully
/// qualified class name.
fn test_id_from(display: &str, parent: Option<&TestId>, data: &Value) -> TestId {
    let display = display.trim();
    let junit_class = (data["dataKind"].as_str() == Some("junit-style-test-case-data"))
        .then(|| str_field(&data["data"], "className"))
        .flatten();
    if let Some(parent) = parent
        && parent.method.is_none()
    {
        if let Some((class, method)) = display.split_once('#')
            && class == parent.class_name
        {
            return TestId::method(class, strip_parens(method));
        }
        if let Some(open) = display.find('(')
            && display.ends_with(')')
        {
            let inner = &display[open + 1..display.len() - 1];
            if inner == parent.class_name {
                return TestId::method(inner, &display[..open]);
            }
        }
        return TestId::method(parent.class_name.clone(), strip_parens(display));
    }
    if let Some((class, method)) = display.split_once('#')
        && !class.is_empty()
        && !class.contains(' ')
    {
        return TestId::method(class, strip_parens(method));
    }
    if let Some(open) = display.find('(')
        && display.ends_with(')')
    {
        let inner = &display[open + 1..display.len() - 1];
        if inner.contains('.') && !inner.contains(' ') {
            return TestId::method(inner, &display[..open]);
        }
    }
    if let Some(class) = junit_class {
        return TestId::method(class, strip_parens(display));
    }
    TestId::class(display)
}

fn strip_parens(name: &str) -> String {
    name.strip_suffix("()").unwrap_or(name).to_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn n(method: &str, params: Value) -> JsonRpcNotification {
        JsonRpcNotification::new(method, params)
    }

    #[test]
    fn gradle_style_nested_class_and_method_tasks() {
        let mut mapper = BspEventMapper::new();
        mapper.set_target_names([(
            "file:///p/app?sourceset=test".to_owned(),
            "app-test".to_owned(),
        )]);
        let ev = mapper.map(&n(
            "build/taskStart",
            json!({"taskId": {"id": "t"}, "dataKind": "test-task",
                   "data": {"target": {"uri": "file:///p/app?sourceset=test"}}}),
        ));
        assert!(
            matches!(&ev[0], BspEvent::Test(TestEvent::Output { text, .. }) if text == "Testing app-test")
        );
        let ev = mapper.map(&n(
            "build/taskStart",
            json!({"taskId": {"id": "c", "parents": ["t"]}, "dataKind": "test-start",
                   "data": {"displayName": "com.kova.OrderTest"}}),
        ));
        assert_eq!(
            ev,
            vec![BspEvent::Test(TestEvent::TestStarted(
                TestId::class("com.kova.OrderTest").in_suite("app-test")
            ))]
        );
        mapper.map(&n(
            "build/taskStart",
            json!({"taskId": {"id": "m", "parents": ["t", "c"]}, "eventTime": 1000,
                   "dataKind": "test-start", "data": {"displayName": "creates()"}}),
        ));
        let log = mapper.map(&n(
            "build/logMessage",
            json!({"type": 4, "task": {"id": "m"}, "message": "hello"}),
        ));
        let method = TestId::method("com.kova.OrderTest", "creates").in_suite("app-test");
        assert_eq!(
            log,
            vec![BspEvent::Test(TestEvent::Output {
                id: Some(method.clone()),
                stream: OutputStream::Stdout,
                text: "hello".to_owned()
            })]
        );
        let ev = mapper.map(&n(
            "build/taskFinish",
            json!({"taskId": {"id": "m", "parents": ["t", "c"]}, "eventTime": 1042,
                   "dataKind": "test-finish",
                   "data": {"displayName": "creates()", "status": 2,
                            "message": "java.lang.AssertionError: nope\n\tat com.kova.OrderTest.creates(OrderTest.java:12)"}}),
        ));
        let BspEvent::Test(TestEvent::TestFinished { id, outcome }) = &ev[0] else {
            panic!("expected TestFinished, got {ev:?}");
        };
        assert_eq!(id, &method);
        assert_eq!(outcome.status, TestStatus::Failed);
        assert_eq!(outcome.duration_ms, Some(42));
        assert_eq!(
            outcome.message.as_deref(),
            Some("java.lang.AssertionError: nope")
        );
        assert!(
            outcome
                .stacktrace
                .as_deref()
                .unwrap()
                .contains("OrderTest.java:12")
        );
    }

    #[test]
    fn hash_and_junit4_display_names_and_junit_data() {
        let none = Value::Null;
        assert_eq!(
            test_id_from("a.B#m()", None, &none),
            TestId::method("a.B", "m")
        );
        assert_eq!(
            test_id_from("m(a.B)", None, &none),
            TestId::method("a.B", "m")
        );
        let data = json!({"dataKind": "junit-style-test-case-data",
                          "data": {"className": "a.C", "time": 0.5, "fullError": "boom"}});
        assert_eq!(test_id_from("m", None, &data), TestId::method("a.C", "m"));
        assert_eq!(test_id_from("a.D", None, &none), TestId::class("a.D"));

        let mut mapper = BspEventMapper::new();
        let mut finish = data;
        finish["displayName"] = json!("m");
        finish["status"] = json!(1);
        let ev = mapper.map(&n(
            "build/taskFinish",
            json!({"taskId": {"id": "x"}, "dataKind": "test-finish", "data": finish}),
        ));
        let BspEvent::Test(TestEvent::TestFinished { outcome, .. }) = &ev[0] else {
            panic!("expected TestFinished");
        };
        assert_eq!(outcome.status, TestStatus::Passed);
        assert_eq!(outcome.duration_ms, Some(500));
    }

    #[test]
    fn diagnostics_progress_and_reports() {
        let mut mapper = BspEventMapper::new();
        let ev = mapper.map(&n(
            "build/publishDiagnostics",
            json!({"textDocument": {"uri": "file:///p/A.java"}, "buildTarget": {"uri": "t"},
                   "reset": false,
                   "diagnostics": [{"range": {"start": {"line": 3, "character": 1},
                                              "end": {"line": 3, "character": 5}},
                                    "severity": 1, "message": "cannot find symbol"}]}),
        ));
        let BspEvent::Diagnostics {
            uri,
            diagnostics,
            reset,
            ..
        } = &ev[0]
        else {
            panic!("expected diagnostics");
        };
        assert_eq!(uri, "file:///p/A.java");
        assert!(!reset);
        assert_eq!(diagnostics[0].message, "cannot find symbol");

        let ev = mapper.map(&n(
            "build/taskProgress",
            json!({"taskId": {"id": "c"}, "message": "Compiling", "progress": 2, "total": 5}),
        ));
        assert!(matches!(&ev[0], BspEvent::Progress { total: Some(5), .. }));

        let ev = mapper.map(&n(
            "build/taskFinish",
            json!({"taskId": {"id": "c"}, "dataKind": "compile-report",
                   "data": {"target": {"uri": "t"}, "errors": 2, "warnings": 1}}),
        ));
        assert_eq!(
            ev,
            vec![BspEvent::CompileReport {
                target: Some("t".to_owned()),
                errors: 2,
                warnings: 1
            }]
        );
        assert_eq!(mapper.map(&n("build/unknown", Value::Null)), Vec::new());
    }

    #[test]
    fn status_codes() {
        assert!(BspStatusCode::from_code(1).is_ok());
        assert_eq!(BspStatusCode::from_code(3), BspStatusCode::Cancelled);
        assert_eq!(BspStatusCode::from_code(9), BspStatusCode::Error);
    }

    #[test]
    fn spock_feature_and_unroll_bsp_events_preserve_full_method_name() {
        let mut mapper = BspEventMapper::new();
        mapper.map(&n(
            "build/taskStart",
            json!({"taskId": {"id": "c"}, "dataKind": "test-start",
                   "data": {"displayName": "com.kova.spock.OrderSpec"}}),
        ));
        let start = mapper.map(&n(
            "build/taskStart",
            json!({"taskId": {"id": "m", "parents": ["c"]}, "eventTime": 1000,
                   "dataKind": "test-start",
                   "data": {"displayName": "should place order [item: book, #0]"}}),
        ));
        assert_eq!(
            start,
            vec![BspEvent::Test(TestEvent::TestStarted(TestId::method(
                "com.kova.spock.OrderSpec",
                "should place order [item: book, #0]"
            )))]
        );
        let finish = mapper.map(&n(
            "build/taskFinish",
            json!({"taskId": {"id": "m", "parents": ["c"]}, "eventTime": 1025,
                   "dataKind": "test-finish",
                   "data": {"displayName": "should place order [item: book, #0]", "status": 1}}),
        ));
        assert!(matches!(
            &finish[0],
            BspEvent::Test(TestEvent::TestFinished { id, outcome })
                if id.class_name == "com.kova.spock.OrderSpec"
                    && id.method.as_deref() == Some("should place order [item: book, #0]")
                    && outcome.status == TestStatus::Passed
        ));
    }
}
