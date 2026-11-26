//! Debug Adapter Protocol client for `java-debug` / JDWP (`KOVA-041`).
//!
//! - [`protocol`]: the DAP message model (`seq` / `request_seq`, `request` / `response` /
//!   `event`) on top of the LSP `Content-Length` framing,
//! - [`client`]: [`DapClient`], request/response correlation, the event stream and answers
//!   to reverse requests,
//! - [`model`]: breakpoints cached per file ([`BreakpointStore`]), DAP payload types and the
//!   headless [`SessionState`] machine,
//! - [`session`]: [`DebugSession`], the handshake, breakpoint synchronisation, stepping and
//!   lazy, paged inspection (`threads`, `stackTrace`, `scopes`, `variables`, `evaluate`),
//! - [`java`]: `java-debug` `launch` / `attach` arguments and the `jdt.ls` commands that start
//!   the adapter.

pub mod client;
pub mod java;
pub mod model;
pub mod protocol;
pub mod session;

pub use client::{DEFAULT_DAP_REQUEST_TIMEOUT, DapClient, PendingRequest};
pub use java::{
    JavaAttachConfig, JavaLaunchConfig, RESOLVE_CLASSPATH_COMMAND, RESOLVE_MAIN_CLASS_COMMAND,
    START_DEBUG_SESSION_COMMAND, parse_classpath, parse_debug_port, resolve_classpath,
    start_java_debug_adapter,
};
pub use model::{
    BreakpointKind, BreakpointStatus, BreakpointStore, DapScope, DapSource, DapStackFrame,
    DapThread, DapVariable, DebugEvent, DebugSessionState, EvaluateContext, EvaluateResult,
    OutputCategory, SessionState, SourceBreakpoint, StackTracePage, StoppedInfo, VariablesFilter,
    VariablesPage,
};
pub use protocol::{DapEvent, DapMessage, DapRequest, DapResponse};
pub use session::{DebugSession, DebugTarget, StepKind, initialize_arguments};
