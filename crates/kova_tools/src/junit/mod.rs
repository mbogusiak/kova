//! `JUnit` test runner model (`KOVA-040`): the *Suite → Class → Method* result tree, the JVM
//! stack trace parser, the `TEST-*.xml` fallback parser and build-tool command lines.

pub mod launch;
pub mod model;
pub mod stacktrace_parser;
pub mod xml_report;

pub use launch::{BuildTool, TestCommand, build_test_command};
pub use model::{
    NodeId, NodeKind, OutputKind, OutputLine, OutputStream, RunState, TestCounts, TestEvent,
    TestFilter, TestId, TestNode, TestOutcome, TestRun, TestStatus, VisibleRow,
};
pub use stacktrace_parser::{
    StackFrame, TraceLine, parse_stack_frame, parse_stacktrace, stack_frames,
};
pub use xml_report::{JunitXmlError, collect_reports, parse_junit_xml, report_dirs};
