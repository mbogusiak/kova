//! Fallback parser for Ant/Surefire `TEST-*.xml` reports written by Gradle and Maven
//! (`KOVA-040`). Used when the BSP server does not stream per-test notifications.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::junit::model::{OutputStream, TestEvent, TestId, TestOutcome, TestStatus};

/// Errors of the `TEST-*.xml` fallback.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JunitXmlError {
    /// The document is not well-formed XML.
    #[error("malformed JUnit XML report: {0}")]
    Malformed(String),
    /// The root element is neither `<testsuite>` nor `<testsuites>`.
    #[error("unexpected root element `{0}` in JUnit XML report")]
    UnexpectedRoot(String),
    /// The report could not be read.
    #[error("cannot read JUnit XML report `{path}`: {message}")]
    Io {
        /// File path.
        path: String,
        /// OS error.
        message: String,
    },
}

/// Parses one `TEST-*.xml` document into `TestFinished` / `Output` events.
///
/// `suite` optionally groups the classes (e.g. the Gradle module or Maven artifact).
///
/// # Errors
///
/// [`JunitXmlError::Malformed`] or [`JunitXmlError::UnexpectedRoot`].
pub fn parse_junit_xml(xml: &str, suite: Option<&str>) -> Result<Vec<TestEvent>, JunitXmlError> {
    let doc =
        roxmltree::Document::parse(xml).map_err(|e| JunitXmlError::Malformed(e.to_string()))?;
    let root = doc.root_element();
    let suites: Vec<roxmltree::Node<'_, '_>> = match root.tag_name().name() {
        "testsuite" => vec![root],
        "testsuites" => root
            .children()
            .filter(|n| n.has_tag_name("testsuite"))
            .collect(),
        other => return Err(JunitXmlError::UnexpectedRoot(other.to_owned())),
    };
    let mut events = Vec::new();
    for xml_suite in suites {
        parse_suite(xml_suite, suite, &mut events);
    }
    Ok(events)
}

fn with_suite(id: TestId, suite: Option<&str>) -> TestId {
    match suite {
        Some(s) => id.in_suite(s),
        None => id,
    }
}

fn parse_suite(node: roxmltree::Node<'_, '_>, suite: Option<&str>, events: &mut Vec<TestEvent>) {
    let suite_class = node.attribute("name").unwrap_or("").to_owned();
    for case in node.children().filter(|n| n.has_tag_name("testcase")) {
        let class = case
            .attribute("classname")
            .filter(|c| !c.is_empty())
            .unwrap_or(&suite_class);
        let method = normalize_method(case.attribute("name").unwrap_or("test"));
        let id = with_suite(TestId::method(class, method), suite);
        let duration_ms = case.attribute("time").and_then(seconds_to_ms);

        let problem = case
            .children()
            .find(|n| n.has_tag_name("failure") || n.has_tag_name("error"));
        let skipped = case.children().find(|n| n.has_tag_name("skipped"));
        let outcome = if let Some(problem) = problem {
            let body = problem.text().unwrap_or("").trim().to_owned();
            let message = problem
                .attribute("message")
                .map(str::to_owned)
                .or_else(|| problem.attribute("type").map(str::to_owned));
            TestOutcome {
                status: TestStatus::Failed,
                duration_ms,
                message,
                stacktrace: (!body.is_empty()).then_some(body),
            }
        } else if let Some(skipped) = skipped {
            TestOutcome {
                status: TestStatus::Ignored,
                duration_ms,
                message: skipped.attribute("message").map(str::to_owned),
                stacktrace: None,
            }
        } else {
            TestOutcome {
                status: TestStatus::Passed,
                duration_ms,
                message: None,
                stacktrace: None,
            }
        };
        for (tag, stream) in [
            ("system-out", OutputStream::Stdout),
            ("system-err", OutputStream::Stderr),
        ] {
            if let Some(text) = child_text(case, tag) {
                events.push(TestEvent::Output {
                    id: Some(id.clone()),
                    stream,
                    text,
                });
            }
        }
        events.push(TestEvent::TestFinished { id, outcome });
    }
    // Suite-level output (Gradle puts the class's stdout here).
    if !suite_class.is_empty() {
        for (tag, stream) in [
            ("system-out", OutputStream::Stdout),
            ("system-err", OutputStream::Stderr),
        ] {
            if let Some(text) = child_text(node, tag) {
                events.push(TestEvent::Output {
                    id: Some(with_suite(TestId::class(suite_class.clone()), suite)),
                    stream,
                    text,
                });
            }
        }
    }
}

fn child_text(node: roxmltree::Node<'_, '_>, tag: &str) -> Option<String> {
    node.children()
        .find(|n| n.has_tag_name(tag))
        .and_then(|n| n.text())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

/// Gradle reports `shouldWork()`; Surefire `shouldWork`. Parameterised display names such as
/// `[1] amount=5` are kept as they are.
fn normalize_method(name: &str) -> String {
    name.strip_suffix("()").unwrap_or(name).to_owned()
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "durations are non-negative and far below u64::MAX milliseconds"
)]
fn seconds_to_ms(raw: &str) -> Option<u64> {
    let secs: f64 = raw.replace(',', "").trim().parse().ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| (secs * 1000.0).round() as u64)
}

/// Report directories used by Gradle (`build/test-results/test`) and Maven
/// (`target/surefire-reports`, `target/failsafe-reports`) under `module_root`.
#[must_use]
pub fn report_dirs(module_root: &Path) -> Vec<PathBuf> {
    vec![
        module_root.join("build").join("test-results").join("test"),
        module_root.join("target").join("surefire-reports"),
        module_root.join("target").join("failsafe-reports"),
    ]
}

/// Reads every `TEST-*.xml` in `dirs` modified at or after `since` (when given) and returns the
/// combined events, in file-name order.
///
/// Missing directories are skipped; malformed reports are reported as errors alongside the
/// events of the valid ones.
#[must_use]
pub fn collect_reports(
    dirs: &[PathBuf],
    since: Option<SystemTime>,
) -> (Vec<TestEvent>, Vec<JunitXmlError>) {
    let mut files = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_report = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("TEST-")
                    && Path::new(n)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("xml"))
            });
            if !is_report {
                continue;
            }
            let fresh = since.is_none_or(|since| {
                entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|modified| modified >= since)
            });
            if fresh {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut events = Vec::new();
    let mut errors = Vec::new();
    for path in files {
        match std::fs::read_to_string(&path) {
            Ok(xml) => match parse_junit_xml(&xml, None) {
                Ok(mut parsed) => events.append(&mut parsed),
                Err(err) => errors.push(err),
            },
            Err(err) => errors.push(JunitXmlError::Io {
                path: path.display().to_string(),
                message: err.to_string(),
            }),
        }
    }
    (events, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::junit::model::TestRun;

    const GRADLE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="com.kova.shop.OrderServiceTest" tests="3" skipped="1" failures="1" errors="0" time="0.142">
  <properties/>
  <testcase name="shouldCreateOrder()" classname="com.kova.shop.OrderServiceTest" time="0.038"/>
  <testcase name="shouldRejectNegativeAmount()" classname="com.kova.shop.OrderServiceTest" time="0.064">
    <failure message="java.lang.AssertionError: Expected IllegalArgumentException" type="java.lang.AssertionError">java.lang.AssertionError: Expected IllegalArgumentException
	at com.kova.shop.OrderService.validate(OrderService.java:84)
	at com.kova.shop.OrderServiceTest.shouldRejectNegativeAmount(OrderServiceTest.java:52)
</failure>
  </testcase>
  <testcase name="shouldEmitOrderEvent()" classname="com.kova.shop.OrderServiceTest" time="0.0">
    <skipped/>
  </testcase>
  <system-out><![CDATA[order created
]]></system-out>
  <system-err><![CDATA[]]></system-err>
</testsuite>
"#;

    const SUREFIRE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="com.kova.A" tests="1">
    <testcase name="works" classname="com.kova.A" time="1,234.5">
      <system-out>hi</system-out>
    </testcase>
  </testsuite>
  <testsuite name="com.kova.B" tests="1">
    <testcase name="boom" classname="com.kova.B" time="0.01">
      <error type="java.lang.NullPointerException">java.lang.NullPointerException
	at com.kova.B.boom(B.java:7)</error>
    </testcase>
  </testsuite>
</testsuites>
"#;

    #[test]
    fn gradle_report_builds_a_tree() {
        let events = parse_junit_xml(GRADLE_XML, None).unwrap();
        let mut run = TestRun::new("OrderServiceTest", Vec::new());
        run.apply_batch(events);
        let counts = run.counts();
        assert_eq!((counts.passed, counts.failed, counts.ignored), (1, 1, 1));
        let failed = run.first_failed().unwrap();
        let node = run.node(failed).unwrap();
        assert_eq!(node.name, "shouldRejectNegativeAmount");
        assert_eq!(node.duration_ms, 64);
        assert_eq!(node.frames.len(), 2);
        let class = run.node(run.roots()[0]).unwrap();
        assert_eq!(class.stdout, "order created");
        assert_eq!(class.duration_ms, 102);
    }

    #[test]
    fn surefire_testsuites_and_errors() {
        let events = parse_junit_xml(SUREFIRE_XML, Some("shop")).unwrap();
        let mut run = TestRun::new("all", Vec::new());
        run.apply_batch(events);
        assert_eq!(run.counts().total, 2);
        assert_eq!(run.failed_targets(), vec!["com.kova.B#boom".to_owned()]);
        let a = run
            .find(&TestId::method("com.kova.A", "works").in_suite("shop"))
            .unwrap();
        assert_eq!(run.node(a).unwrap().duration_ms, 1_234_500);
        assert_eq!(run.node(a).unwrap().stdout, "hi");
        let b = run.first_failed().unwrap();
        assert_eq!(
            run.node(b).unwrap().message.as_deref(),
            Some("java.lang.NullPointerException")
        );
    }

    #[test]
    fn rejects_bad_documents() {
        assert!(matches!(
            parse_junit_xml("<nope/>", None),
            Err(JunitXmlError::UnexpectedRoot(_))
        ));
        assert!(matches!(
            parse_junit_xml("<testsuite", None),
            Err(JunitXmlError::Malformed(_))
        ));
    }

    #[test]
    fn collects_reports_from_gradle_and_maven_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = report_dirs(tmp.path());
        std::fs::create_dir_all(&dirs[0]).unwrap();
        std::fs::create_dir_all(&dirs[1]).unwrap();
        std::fs::write(
            dirs[0].join("TEST-com.kova.shop.OrderServiceTest.xml"),
            GRADLE_XML,
        )
        .unwrap();
        std::fs::write(dirs[1].join("TEST-all.xml"), SUREFIRE_XML).unwrap();
        std::fs::write(dirs[1].join("TEST-broken.xml"), "<testsuite").unwrap();
        std::fs::write(dirs[1].join("summary.txt"), "ignored").unwrap();

        let (events, errors) = collect_reports(&dirs, None);
        let mut run = TestRun::new("all", Vec::new());
        run.apply_batch(events);
        assert_eq!(run.counts().total, 5);
        assert_eq!(errors.len(), 1);

        let future = SystemTime::now() + std::time::Duration::from_secs(3600);
        let (events, _) = collect_reports(&dirs, Some(future));
        assert_eq!(events.len(), 0);
    }
}
