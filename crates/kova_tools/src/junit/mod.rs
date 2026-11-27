//! `JUnit` test runner model (`KOVA-040`): the *Suite → Class → Method* result tree, the JVM
//! stack trace parser, the `TEST-*.xml` fallback parser and build-tool command lines.

pub mod cargo_test_parser;
pub mod compiler_parser;
pub mod launch;
pub mod model;
pub mod stacktrace_parser;
pub mod xml_report;

pub use cargo_test_parser::CargoTestOutputParser;
pub use compiler_parser::{
    CargoStdoutLine, CompilerOutputParser, DiagnosticLevel, ParsedCompilerDiagnostic,
    parse_cargo_json_line,
};
pub use launch::{
    BuildCommand, BuildTool, TestCommand, build_compile_command, build_test_command,
    format_gradle_test_filter, format_maven_test_filter,
};
pub use model::{
    NodeId, NodeKind, OutputKind, OutputLine, OutputStream, RunState, TestCounts, TestEvent,
    TestFilter, TestId, TestNode, TestOutcome, TestRun, TestStatus, VisibleRow,
};
pub use stacktrace_parser::{
    StackFrame, TraceLine, parse_stack_frame, parse_stacktrace, stack_frames,
};
pub use xml_report::{JunitXmlError, collect_reports, parse_junit_xml, report_dirs};
