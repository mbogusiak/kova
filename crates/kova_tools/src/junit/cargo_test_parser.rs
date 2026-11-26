//! Headless streaming parser for `cargo test` (`libtest`) stdout output (`KOVA-058`).

use std::collections::HashMap;

use crate::junit::model::{OutputStream, TestEvent, TestId, TestOutcome};

#[derive(Debug, Clone)]
struct PendingFailure {
    id: TestId,
    output: String,
}

/// Stateful streaming parser converting `cargo test` lines into [`TestEvent`]s.
#[derive(Debug, Default)]
pub struct CargoTestOutputParser {
    current_suite: Option<String>,
    pending_failures: HashMap<String, PendingFailure>,
    failure_order: Vec<String>,
    active_failure_section: Option<String>,
}

impl CargoTestOutputParser {
    /// Creates an empty parser.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one `stdout` line from `cargo test` and returns zero or more [`TestEvent`]s.
    pub fn parse_line(&mut self, raw_line: &str) -> Vec<TestEvent> {
        let line = raw_line.trim_end_matches(['\r', '\n']);
        let trimmed = line.trim();
        let mut events = Vec::new();

        // `---- module::test_name stdout ----`
        if let Some(name) = parse_failure_stdout_header(trimmed) {
            if self.pending_failures.contains_key(name) {
                self.active_failure_section = Some(name.to_owned());
            } else {
                self.active_failure_section = None;
            }
            return events;
        }

        // End of `failures:` / `---- ... stdout ----` sections.
        if trimmed == "failures:" || trimmed.starts_with("test result:") {
            self.active_failure_section = None;
            if trimmed.starts_with("test result:") {
                events.extend(self.flush_failures());
            }
            return events;
        }

        // Capture lines inside an active `---- <name> stdout ----` failure block.
        if let Some(active_name) = &self.active_failure_section {
            if let Some(failure) = self.pending_failures.get_mut(active_name) {
                if !failure.output.is_empty() {
                    failure.output.push('\n');
                }
                failure.output.push_str(line);
                events.push(TestEvent::Output {
                    id: Some(failure.id.clone()),
                    stream: OutputStream::Stderr,
                    text: line.to_owned(),
                });
            }
            return events;
        }

        // `Running unittests src/lib.rs (target/debug/deps/...)` or `Running tests/it.rs (...)`
        if let Some(suite) = parse_running_suite(trimmed) {
            self.current_suite = Some(suite);
            return events;
        }

        // `test foo::bar ... ok | FAILED | ignored`
        if let Some((full_name, status)) = parse_test_result_line(trimmed) {
            let id = self.build_test_id(full_name);
            events.push(TestEvent::TestStarted(id.clone()));
            match status {
                LibtestStatus::Ok => {
                    events.push(TestEvent::TestFinished {
                        id,
                        outcome: TestOutcome::passed(0),
                    });
                }
                LibtestStatus::Ignored => {
                    events.push(TestEvent::TestFinished {
                        id,
                        outcome: TestOutcome::ignored(),
                    });
                }
                LibtestStatus::Failed => {
                    if !self.pending_failures.contains_key(full_name) {
                        self.failure_order.push(full_name.to_owned());
                    }
                    self.pending_failures.insert(
                        full_name.to_owned(),
                        PendingFailure {
                            id,
                            output: String::new(),
                        },
                    );
                }
            }
        }

        events
    }

    /// Flushes any deferred failure outcomes at the end of a `cargo test` run.
    pub fn finish(&mut self) -> Vec<TestEvent> {
        self.active_failure_section = None;
        self.flush_failures()
    }

    fn flush_failures(&mut self) -> Vec<TestEvent> {
        let mut out = Vec::new();
        for name in self.failure_order.drain(..) {
            if let Some(failure) = self.pending_failures.remove(&name) {
                let message = first_panic_message(&failure.output)
                    .unwrap_or_else(|| format!("Test `{name}` failed"));
                out.push(TestEvent::TestFinished {
                    id: failure.id,
                    outcome: TestOutcome::failed(0, message, failure.output),
                });
            }
        }
        self.pending_failures.clear();
        out
    }

    fn build_test_id(&self, full_name: &str) -> TestId {
        let (class_name, method) = match full_name.rsplit_once("::") {
            Some((module, fn_name)) if !module.is_empty() && !fn_name.is_empty() => {
                (module.to_owned(), fn_name.to_owned())
            }
            _ => {
                let suite = self
                    .current_suite
                    .clone()
                    .unwrap_or_else(|| "tests".to_owned());
                (suite, full_name.to_owned())
            }
        };
        let mut id = TestId::method(class_name, method);
        if let Some(suite) = &self.current_suite {
            id = id.in_suite(suite.clone());
        }
        id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibtestStatus {
    Ok,
    Failed,
    Ignored,
}

fn parse_running_suite(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix("Running ")?;
    let before_paren = rest.split_once(" (").map_or(rest, |(head, _)| head).trim();
    if before_paren.is_empty() {
        return None;
    }
    Some(
        before_paren
            .strip_prefix("unittests ")
            .unwrap_or(before_paren)
            .to_owned(),
    )
}

fn parse_failure_stdout_header(trimmed: &str) -> Option<&str> {
    let inner = trimmed
        .strip_prefix("---- ")?
        .strip_suffix(" stdout ----")?
        .trim();
    (!inner.is_empty()).then_some(inner)
}

fn parse_test_result_line(trimmed: &str) -> Option<(&str, LibtestStatus)> {
    let rest = trimmed.strip_prefix("test ")?;
    let (name, status_part) = rest.rsplit_once(" ... ")?;
    let name = name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let status_word = status_part.split_whitespace().next()?;
    let status = match status_word {
        "ok" => LibtestStatus::Ok,
        "FAILED" => LibtestStatus::Failed,
        "ignored" => LibtestStatus::Ignored,
        _ => return None,
    };
    Some((name, status))
}

fn first_panic_message(output: &str) -> Option<String> {
    for line in output.lines() {
        let t = line.trim();
        if !t.is_empty() {
            return Some(t.to_owned());
        }
    }
    None
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests assert cargo test parser output"
)]
mod tests {
    use super::*;
    use crate::junit::model::{TestRun, TestStatus};

    #[test]
    fn parses_cargo_test_passed_ignored_and_failed_with_panic_output() {
        let lines = [
            "running 3 tests",
            "Running unittests src/lib.rs (target/debug/deps/my_crate-1234)",
            "test orders::tests::creates_order ... ok",
            "test orders::tests::skipped_case ... ignored",
            "test orders::tests::rejects_negative ... FAILED",
            "",
            "failures:",
            "",
            "---- orders::tests::rejects_negative stdout ----",
            "thread 'orders::tests::rejects_negative' panicked at src/orders.rs:42:9:",
            "assertion failed: amount > 0",
            "",
            "failures:",
            "    orders::tests::rejects_negative",
            "",
            "test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out",
        ];

        let mut parser = CargoTestOutputParser::new();
        let mut events = Vec::new();
        for line in lines {
            events.extend(parser.parse_line(line));
        }
        events.extend(parser.finish());

        let mut run = TestRun::new("cargo test", Vec::new());
        run.apply_batch(events);

        let counts = run.counts();
        assert_eq!(counts.passed, 1);
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.ignored, 1);
        assert_eq!(counts.total, 3);

        let failed_id = run.first_failed().expect("failed test exists");
        let failed_node = run.node(failed_id).expect("failed node");
        assert_eq!(failed_node.name, "rejects_negative");
        assert_eq!(failed_node.status, TestStatus::Failed);
        assert!(
            failed_node
                .message
                .as_deref()
                .is_some_and(|m| m.contains("panicked at src/orders.rs:42:9"))
        );
        assert!(
            failed_node
                .stacktrace
                .as_deref()
                .is_some_and(|s| s.contains("assertion failed: amount > 0"))
        );
    }
}
