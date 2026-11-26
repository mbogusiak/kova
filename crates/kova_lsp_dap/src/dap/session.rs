//! Debug session driver (`KOVA-041`): the DAP handshake, breakpoint synchronisation,
//! execution control and lazy, paged inspection on top of a [`DapClient`].
//!
//! Handshake order (as required by `java-debug` and the DAP specification):
//!
//! 1. `initialize` → adapter capabilities,
//! 2. `launch` / `attach` is **sent but not awaited**,
//! 3. wait for the `initialized` event,
//! 4. one `setBreakpoints` per file with breakpoints (always the complete set of the file),
//! 5. `configurationDone`,
//! 6. await the `launch` / `attach` response.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::dap::client::DapClient;
use crate::dap::model::{
    BreakpointStatus, BreakpointStore, DapScope, DapThread, DapVariable, DebugEvent,
    DebugSessionState, EvaluateContext, EvaluateResult, SessionState, SourceBreakpoint,
    StackTracePage, StoppedInfo, VariablesFilter, VariablesPage,
};
use crate::dap::protocol::DapEvent;
use crate::error::LspDapError;

/// How the debuggee is started.
#[derive(Debug, Clone, PartialEq)]
pub enum DebugTarget {
    /// `launch` request with adapter-specific arguments.
    Launch(Value),
    /// `attach` request with adapter-specific arguments.
    Attach(Value),
}

impl DebugTarget {
    /// DAP command name.
    #[must_use]
    pub const fn command(&self) -> &'static str {
        match self {
            Self::Launch(_) => "launch",
            Self::Attach(_) => "attach",
        }
    }

    /// Request arguments.
    #[must_use]
    pub const fn arguments(&self) -> &Value {
        match self {
            Self::Launch(args) | Self::Attach(args) => args,
        }
    }
}

/// Stepping command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StepKind {
    /// *Step Over* (`F8`, DAP `next`).
    Over,
    /// *Step Into* (`F7`, DAP `stepIn`).
    Into,
    /// *Step Out* (`Shift+F8`, DAP `stepOut`).
    Out,
}

impl StepKind {
    /// DAP command name.
    #[must_use]
    pub const fn command(self) -> &'static str {
        match self {
            Self::Over => "next",
            Self::Into => "stepIn",
            Self::Out => "stepOut",
        }
    }
}

type VariablesKey = (i64, usize, usize, VariablesFilter);

/// A DAP debug session.
#[derive(Debug)]
pub struct DebugSession {
    client: DapClient,
    events: mpsc::UnboundedReceiver<DapEvent>,
    backlog: VecDeque<DebugEvent>,
    state: SessionState,
    capabilities: Value,
    breakpoints: BreakpointStore,
    statuses: BTreeMap<PathBuf, Vec<BreakpointStatus>>,
    variables_epoch: u64,
    variables: HashMap<VariablesKey, Vec<DapVariable>>,
}

impl DebugSession {
    /// Wraps a connected client and its event stream.
    #[must_use]
    pub fn new(client: DapClient, events: mpsc::UnboundedReceiver<DapEvent>) -> Self {
        Self {
            client,
            events,
            backlog: VecDeque::new(),
            state: SessionState::new(),
            capabilities: Value::Null,
            breakpoints: BreakpointStore::new(),
            statuses: BTreeMap::new(),
            variables_epoch: 0,
            variables: HashMap::new(),
        }
    }

    /// Seeds the breakpoints sent during the handshake.
    #[must_use]
    pub fn with_breakpoints(mut self, breakpoints: BreakpointStore) -> Self {
        self.breakpoints = breakpoints;
        self
    }

    /// Lifecycle state.
    #[must_use]
    pub const fn state(&self) -> DebugSessionState {
        self.state.state()
    }

    /// Last suspension, while stopped.
    #[must_use]
    pub const fn stopped(&self) -> Option<&StoppedInfo> {
        self.state.stopped()
    }

    /// Adapter capabilities from the `initialize` response.
    #[must_use]
    pub const fn capabilities(&self) -> &Value {
        &self.capabilities
    }

    /// Breakpoints cached per file.
    #[must_use]
    pub const fn breakpoints(&self) -> &BreakpointStore {
        &self.breakpoints
    }

    /// Adapter view of the breakpoints of `file` (after the last `setBreakpoints`).
    #[must_use]
    pub fn breakpoint_statuses(&self, file: &Path) -> &[BreakpointStatus] {
        self.statuses.get(file).map_or(&[], Vec::as_slice)
    }

    fn capability(&self, name: &str) -> Option<bool> {
        self.capabilities.get(name).and_then(Value::as_bool)
    }

    /// Runs the handshake (`initialize` → `launch`/`attach` → `initialized` →
    /// `setBreakpoints`* → `configurationDone`).
    ///
    /// # Errors
    /// [`LspDapError::InvalidState`] when called twice, and any transport, timeout or
    /// adapter failure of the handshake requests.
    pub async fn start(&mut self, target: &DebugTarget) -> Result<(), LspDapError> {
        self.expect_state(&[DebugSessionState::NotStarted], "not-started")?;
        self.state.set_state(DebugSessionState::Initializing);
        self.capabilities = self
            .client
            .request("initialize", initialize_arguments())
            .await?;
        self.state.set_state(DebugSessionState::Configuring);

        let timeout = self.client.timeout();
        let launch = self
            .client
            .start_request(target.command(), target.arguments().clone())?
            .wait(timeout);
        tokio::pin!(launch);
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let mut launched = false;
        loop {
            tokio::select! {
                // The launch response wins over a closed event stream, so an adapter that
                // rejects `launch` and disconnects reports its error, not `TransportClosed`.
                biased;
                result = &mut launch, if !launched => {
                    result?;
                    launched = true;
                }
                event = self.events.recv() => {
                    let event = event.ok_or(LspDapError::TransportClosed)?;
                    match self.state.apply(&event) {
                        Some(DebugEvent::Initialized) => break,
                        Some(other) => self.backlog.push_back(other),
                        None => {}
                    }
                }
                () = &mut deadline => {
                    return Err(LspDapError::RequestTimeout {
                        method: "initialized".to_owned(),
                        timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                    });
                }
            }
        }

        let files: Vec<PathBuf> = self.breakpoints.files().map(Path::to_path_buf).collect();
        for file in files {
            self.sync_breakpoints(&file).await?;
        }
        if self.capability("supportsConfigurationDoneRequest") != Some(false) {
            self.client.request("configurationDone", json!({})).await?;
        }
        if !launched {
            launch.await?;
        }
        if self.state.state() == DebugSessionState::Configuring {
            self.state.set_state(DebugSessionState::Running);
        }
        Ok(())
    }

    /// Next debugger event (events buffered during the handshake come first).
    ///
    /// Returns `None` once the adapter closed the connection.
    pub async fn next_event(&mut self) -> Option<DebugEvent> {
        if let Some(event) = self.backlog.pop_front() {
            return Some(event);
        }
        loop {
            let Some(raw) = self.events.recv().await else {
                if self.state.state() != DebugSessionState::Terminated {
                    self.state.set_state(DebugSessionState::Terminated);
                    return Some(DebugEvent::Terminated);
                }
                return None;
            };
            if let Some(event) = self.state.apply(&raw) {
                if let DebugEvent::Breakpoint { status, .. } = &event {
                    self.update_status(status);
                }
                return Some(event);
            }
        }
    }

    fn update_status(&mut self, status: &BreakpointStatus) {
        let Some(id) = status.id else {
            return;
        };
        for statuses in self.statuses.values_mut() {
            if let Some(slot) = statuses.iter_mut().find(|s| s.id == Some(id)) {
                *slot = status.clone();
            }
        }
    }

    // -- Breakpoints --------------------------------------------------------

    /// Toggles a line breakpoint and synchronises the file when the session is active.
    ///
    /// Returns whether a breakpoint now exists at `line`.
    ///
    /// # Errors
    /// Transport and adapter failures of `setBreakpoints`.
    pub async fn toggle_breakpoint(&mut self, file: &Path, line: u32) -> Result<bool, LspDapError> {
        let added = self.breakpoints.toggle(file, line);
        self.sync_if_active(file).await?;
        Ok(added)
    }

    /// Inserts or replaces a breakpoint (line, conditional or logpoint) and synchronises.
    ///
    /// # Errors
    /// Transport and adapter failures of `setBreakpoints`.
    pub async fn upsert_breakpoint(
        &mut self,
        file: &Path,
        breakpoint: SourceBreakpoint,
    ) -> Result<(), LspDapError> {
        self.breakpoints.upsert(file, breakpoint);
        self.sync_if_active(file).await
    }

    /// Replaces every breakpoint of `file` and synchronises.
    ///
    /// # Errors
    /// Transport and adapter failures of `setBreakpoints`.
    pub async fn replace_file_breakpoints(
        &mut self,
        file: &Path,
        breakpoints: Vec<SourceBreakpoint>,
    ) -> Result<(), LspDapError> {
        let lines: Vec<u32> = self.breakpoints.for_file(file).map(|b| b.line).collect();
        for line in lines {
            self.breakpoints.remove(file, line);
        }
        for breakpoint in breakpoints {
            self.breakpoints.upsert(file, breakpoint);
        }
        self.sync_if_active(file).await
    }

    async fn sync_if_active(&mut self, file: &Path) -> Result<(), LspDapError> {
        if matches!(
            self.state.state(),
            DebugSessionState::Running | DebugSessionState::Stopped
        ) {
            self.sync_breakpoints(file).await?;
        }
        Ok(())
    }

    /// Sends the complete breakpoint set of `file` and stores the adapter's statuses.
    ///
    /// # Errors
    /// Transport and adapter failures of `setBreakpoints`.
    pub async fn sync_breakpoints(
        &mut self,
        file: &Path,
    ) -> Result<Vec<BreakpointStatus>, LspDapError> {
        let body = self
            .client
            .request(
                "setBreakpoints",
                self.breakpoints.set_breakpoints_arguments(file),
            )
            .await?;
        let statuses: Vec<BreakpointStatus> = body
            .get("breakpoints")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        if statuses.is_empty() {
            self.statuses.remove(file);
        } else {
            self.statuses.insert(file.to_path_buf(), statuses.clone());
        }
        Ok(statuses)
    }

    // -- Execution control --------------------------------------------------

    /// *Resume Program* (`F9`).
    ///
    /// # Errors
    /// [`LspDapError::InvalidState`] unless stopped, and adapter failures.
    pub async fn resume(&mut self, thread_id: i64) -> Result<(), LspDapError> {
        self.expect_state(&[DebugSessionState::Stopped], "stopped")?;
        self.client
            .request("continue", json!({ "threadId": thread_id }))
            .await?;
        self.resumed();
        Ok(())
    }

    /// *Step Over* / *Step Into* / *Step Out*.
    ///
    /// # Errors
    /// [`LspDapError::InvalidState`] unless stopped, and adapter failures.
    pub async fn step(&mut self, kind: StepKind, thread_id: i64) -> Result<(), LspDapError> {
        self.expect_state(&[DebugSessionState::Stopped], "stopped")?;
        self.client
            .request(kind.command(), json!({ "threadId": thread_id }))
            .await?;
        self.resumed();
        Ok(())
    }

    /// Suspends `thread_id` (the `stopped` event follows).
    ///
    /// # Errors
    /// [`LspDapError::InvalidState`] unless running, and adapter failures.
    pub async fn pause(&mut self, thread_id: i64) -> Result<(), LspDapError> {
        self.expect_state(&[DebugSessionState::Running], "running")?;
        self.client
            .request("pause", json!({ "threadId": thread_id }))
            .await
            .map(drop)
    }

    fn resumed(&mut self) {
        self.state.resume();
        self.variables.clear();
    }

    /// Ends the session: `terminate` when supported (graceful JVM exit), otherwise
    /// `disconnect` with `terminateDebuggee`.
    ///
    /// # Errors
    /// Transport and adapter failures.
    pub async fn stop(&mut self) -> Result<(), LspDapError> {
        if !self.state.state().is_active() {
            return Ok(());
        }
        let result = if self.capability("supportsTerminateRequest") == Some(true) {
            self.client.request("terminate", json!({})).await
        } else {
            self.client
                .request("disconnect", json!({ "terminateDebuggee": true }))
                .await
        };
        self.state.set_state(DebugSessionState::Terminated);
        result.map(drop)
    }

    /// Detaches from the debuggee, optionally terminating it.
    ///
    /// # Errors
    /// Transport and adapter failures.
    pub async fn disconnect(&mut self, terminate_debuggee: bool) -> Result<(), LspDapError> {
        let result = self
            .client
            .request(
                "disconnect",
                json!({ "terminateDebuggee": terminate_debuggee }),
            )
            .await;
        self.state.set_state(DebugSessionState::Terminated);
        result.map(drop)
    }

    // -- Inspection ---------------------------------------------------------

    /// JVM threads.
    ///
    /// # Errors
    /// Transport and adapter failures.
    pub async fn threads(&self) -> Result<Vec<DapThread>, LspDapError> {
        let body = self.client.request("threads", Value::Null).await?;
        list_field(&body, "threads")
    }

    /// One page of the call stack of `thread_id`.
    ///
    /// # Errors
    /// [`LspDapError::InvalidState`] unless stopped, and adapter failures.
    pub async fn stack_trace(
        &self,
        thread_id: i64,
        start_frame: usize,
        levels: usize,
    ) -> Result<StackTracePage, LspDapError> {
        self.expect_state(&[DebugSessionState::Stopped], "stopped")?;
        let body = self
            .client
            .request(
                "stackTrace",
                json!({ "threadId": thread_id, "startFrame": start_frame, "levels": levels }),
            )
            .await?;
        Ok(StackTracePage {
            frames: list_field(&body, "stackFrames")?,
            total_frames: body
                .get("totalFrames")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok()),
        })
    }

    /// Scopes of `frame_id` (`Local`, `this`, …).
    ///
    /// # Errors
    /// [`LspDapError::InvalidState`] unless stopped, and adapter failures.
    pub async fn scopes(&self, frame_id: i64) -> Result<Vec<DapScope>, LspDapError> {
        self.expect_state(&[DebugSessionState::Stopped], "stopped")?;
        let body = self
            .client
            .request("scopes", json!({ "frameId": frame_id }))
            .await?;
        list_field(&body, "scopes")
    }

    /// Children of a scope / object / array, one page at a time.
    ///
    /// Pages are cached until the next resume or step: variable references are only valid
    /// while the thread stays suspended.
    ///
    /// # Errors
    /// [`LspDapError::InvalidState`] unless stopped, and adapter failures.
    pub async fn variables(
        &mut self,
        reference: i64,
        page: VariablesPage,
    ) -> Result<Vec<DapVariable>, LspDapError> {
        self.expect_state(&[DebugSessionState::Stopped], "stopped")?;
        if self.variables_epoch != self.state.epoch() {
            self.variables.clear();
            self.variables_epoch = self.state.epoch();
        }
        let key = (reference, page.start, page.count, page.filter);
        if let Some(cached) = self.variables.get(&key) {
            return Ok(cached.clone());
        }
        let body = self
            .client
            .request("variables", page.arguments(reference))
            .await?;
        let variables: Vec<DapVariable> = list_field(&body, "variables")?;
        self.variables.insert(key, variables.clone());
        Ok(variables)
    }

    /// Evaluates `expression` in the context of `frame_id` (*Evaluate Expression*, watches).
    ///
    /// # Errors
    /// [`LspDapError::DapRequestFailed`] with the compiler / runtime message when the
    /// expression cannot be evaluated, and transport failures.
    pub async fn evaluate(
        &self,
        expression: &str,
        frame_id: Option<i64>,
        context: EvaluateContext,
    ) -> Result<EvaluateResult, LspDapError> {
        let mut args = json!({ "expression": expression, "context": context.as_str() });
        if let Some(frame_id) = frame_id {
            args["frameId"] = json!(frame_id);
        }
        let body = self.client.request("evaluate", args).await?;
        Ok(serde_json::from_value(body)?)
    }

    /// Completion candidates for the *Evaluate Expression* field, when the adapter
    /// supports `completions`.
    ///
    /// # Errors
    /// Transport and adapter failures.
    pub async fn completions(
        &self,
        text: &str,
        column: usize,
        frame_id: Option<i64>,
    ) -> Result<Vec<String>, LspDapError> {
        if self.capability("supportsCompletionsRequest") != Some(true) {
            return Ok(Vec::new());
        }
        let mut args = json!({ "text": text, "column": column });
        if let Some(frame_id) = frame_id {
            args["frameId"] = json!(frame_id);
        }
        let body = self.client.request("completions", args).await?;
        Ok(body
            .get("targets")
            .and_then(Value::as_array)
            .map(|targets| {
                targets
                    .iter()
                    .filter_map(|t| t.get("label").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default())
    }

    fn expect_state(
        &self,
        allowed: &[DebugSessionState],
        expected: &str,
    ) -> Result<(), LspDapError> {
        let current = self.state.state();
        if allowed.contains(&current) {
            Ok(())
        } else {
            Err(LspDapError::InvalidState {
                current: current.to_string(),
                expected: expected.to_owned(),
            })
        }
    }
}

/// `initialize` arguments sent by Kova.
#[must_use]
pub fn initialize_arguments() -> Value {
    json!({
        "clientID": "kova",
        "clientName": "Kova",
        "adapterID": "java",
        "pathFormat": "path",
        "linesStartAt1": true,
        "columnsStartAt1": true,
        "locale": "en",
        "supportsVariableType": true,
        "supportsVariablePaging": true,
        "supportsRunInTerminalRequest": false,
        "supportsProgressReporting": false,
    })
}

fn list_field<T: serde::de::DeserializeOwned>(
    body: &Value,
    key: &str,
) -> Result<Vec<T>, LspDapError> {
    Ok(body
        .get(key)
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default())
}
