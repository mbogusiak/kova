//! [`JdtlsClient`]: lazy `jdt.ls` process start, `initialize` handshake, readiness tracking
//! (`language/status`) and graceful shutdown.

use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use kova_config::EffectiveConfig;
use kova_toolchain::JdtlsLaunchSpec;
use lsp_types::{InitializeResult, ServerCapabilities};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::error::LspDapError;
use crate::lsp::session::{
    DefaultServerRequestHandler, LspSession, NotificationHook, ServerRequestHandler,
    SessionOptions, token_to_string,
};
use crate::lsp::sync::path_to_lsp_uri;
use crate::lsp::transport::JsonRpcNotification;

/// Default timeout for the `initialize` request (`jdt.ls` may import a large project first).
pub const DEFAULT_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(120);
/// Default timeout for ordinary requests.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Default number of `stderr` lines kept for diagnostics.
pub const DEFAULT_STDERR_RING_CAPACITY: usize = 200;
/// Default capacity of the [`LspEvent`] broadcast channel.
pub const DEFAULT_EVENT_CAPACITY: usize = 1024;

/// Lifecycle of a language-server session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspLifecycleState {
    /// No process / session.
    Stopped,
    /// Process (or streams) being set up.
    Starting,
    /// `initialize` sent, waiting for the response.
    Initializing,
    /// `initialized` sent; the server is importing / indexing the project.
    /// Documents can already be opened.
    IndexingProject,
    /// `language/status` reported `ServiceReady` / `Started`.
    Ready,
    /// `shutdown` / `exit` in progress.
    ShuttingDown,
    /// Process stopped to free RAM; can be restarted (KOVA-038).
    Hibernated,
    /// Unrecoverable failure.
    Failed(String),
}

impl fmt::Display for LspLifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stopped => f.write_str("Stopped"),
            Self::Starting => f.write_str("Starting"),
            Self::Initializing => f.write_str("Initializing"),
            Self::IndexingProject => f.write_str("IndexingProject"),
            Self::Ready => f.write_str("Ready"),
            Self::ShuttingDown => f.write_str("ShuttingDown"),
            Self::Hibernated => f.write_str("Hibernated"),
            Self::Failed(reason) => write!(f, "Failed({reason})"),
        }
    }
}

/// Events published to `kova_core` / `kova_ui`.
#[derive(Debug, Clone, PartialEq)]
pub enum LspEvent {
    /// The lifecycle state changed.
    StateChanged {
        /// State before the transition.
        previous: LspLifecycleState,
        /// State after the transition.
        current: LspLifecycleState,
    },
    /// `jdt.ls` `language/status` notification.
    LanguageStatus {
        /// Status type (`Starting`, `Message`, `ServiceReady`, `Started`, `Error`, ...).
        status_type: String,
        /// Human readable message.
        message: String,
    },
    /// `$/progress` work-done progress notification.
    WorkDoneProgress {
        /// Progress token.
        token: String,
        /// Title (only on `begin`).
        title: Option<String>,
        /// Optional message.
        message: Option<String>,
        /// Optional percentage `0..=100`.
        percentage: Option<u32>,
        /// `true` on `end`.
        finished: bool,
    },
    /// `textDocument/publishDiagnostics` (parsed in detail in KOVA-037).
    DiagnosticsReceived {
        /// Document URI.
        uri: String,
        /// Document version the diagnostics refer to.
        version: Option<i32>,
        /// Raw `diagnostics` array.
        diagnostics_json: Value,
    },
    /// A line written by the server on `stderr`.
    ServerStderrLine(String),
}

/// Runtime parameters of a [`JdtlsClient`].
#[derive(Debug, Clone, PartialEq)]
pub struct ClientRuntimeConfig {
    /// Project root (sent as `rootUri` and the single workspace folder).
    pub project_root: PathBuf,
    /// `initializationOptions`; when `null`, [`JdtlsClient::start_with_spec`] uses the
    /// options from the launch spec.
    pub initialization_options: Value,
    /// Settings object answering `workspace/configuration` (`{"java": ..., "kova": ...}`).
    pub workspace_settings: Value,
    /// Timeout of the `initialize` request.
    pub initialize_timeout: Duration,
    /// Default timeout for [`JdtlsClient::request`].
    pub request_timeout: Duration,
    /// Number of `stderr` lines retained.
    pub stderr_ring_capacity: usize,
    /// Capacity of the [`LspEvent`] broadcast channel.
    pub event_capacity: usize,
}

impl ClientRuntimeConfig {
    /// Creates a configuration with defaults and empty settings.
    #[must_use]
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
            initialization_options: Value::Null,
            workspace_settings: json!({}),
            initialize_timeout: DEFAULT_INITIALIZE_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            stderr_ring_capacity: DEFAULT_STDERR_RING_CAPACITY,
            event_capacity: DEFAULT_EVENT_CAPACITY,
        }
    }

    /// Builds the configuration from the effective Kova settings and the `jdt.ls`
    /// `initializationOptions` (whose `settings` object seeds `workspace/configuration`;
    /// Kova's own settings are exposed under the `kova` section).
    #[must_use]
    pub fn from_effective_config(
        project_root: impl Into<PathBuf>,
        effective: &EffectiveConfig,
        initialization_options: Value,
    ) -> Self {
        let mut settings = settings_from_initialization_options(&initialization_options);
        if let Ok(kova) = serde_json::to_value(&effective.settings) {
            settings.insert("kova".to_owned(), kova);
        }
        Self {
            initialization_options,
            workspace_settings: Value::Object(settings),
            ..Self::new(project_root)
        }
    }
}

/// Extracts the `settings` object of `jdt.ls` `initializationOptions` (empty when absent).
fn settings_from_initialization_options(
    initialization_options: &Value,
) -> serde_json::Map<String, Value> {
    match initialization_options.get("settings") {
        Some(Value::Object(map)) => map.clone(),
        _ => serde_json::Map::new(),
    }
}

/// Fills `config` from the launch spec where the caller left it empty: `initializationOptions`
/// and the `workspace/configuration` settings derived from them.
fn seed_from_spec(config: &mut ClientRuntimeConfig, spec_options: &Value) {
    if config.initialization_options.is_null() {
        config.initialization_options = spec_options.clone();
    }
    let settings_empty = match &config.workspace_settings {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        _ => false,
    };
    if settings_empty {
        config.workspace_settings = Value::Object(settings_from_initialization_options(
            &config.initialization_options,
        ));
    }
}

/// Client capabilities advertised to `jdt.ls` (Gate 2, Q-2).
#[must_use]
pub fn build_client_capabilities() -> Value {
    json!({
        "workspace": {
            "applyEdit": true,
            "workspaceEdit": {
                "documentChanges": true,
                "resourceOperations": ["create", "rename", "delete"],
            },
            "didChangeConfiguration": { "dynamicRegistration": true },
            "didChangeWatchedFiles": { "dynamicRegistration": true },
            "configuration": true,
            "workspaceFolders": true,
            "executeCommand": { "dynamicRegistration": true },
            "symbol": { "dynamicRegistration": false },
        },
        "textDocument": {
            "synchronization": {
                "dynamicRegistration": true,
                "willSave": false,
                "willSaveWaitUntil": false,
                "didSave": true,
            },
            "completion": {
                "dynamicRegistration": true,
                "contextSupport": true,
                "completionItem": {
                    "snippetSupport": true,
                    "documentationFormat": ["markdown", "plaintext"],
                    "deprecatedSupport": true,
                    "tagSupport": { "valueSet": [1] },
                    "insertReplaceSupport": true,
                    "labelDetailsSupport": true,
                    "resolveSupport": {
                        "properties": ["documentation", "detail", "additionalTextEdits"],
                    },
                },
            },
            "hover": {
                "dynamicRegistration": true,
                "contentFormat": ["markdown", "plaintext"],
            },
            "definition": { "dynamicRegistration": true, "linkSupport": true },
            "codeAction": {
                "dynamicRegistration": true,
                "isPreferredSupport": true,
                "dataSupport": true,
                "codeActionLiteralSupport": {
                    "codeActionKind": {
                        "valueSet": [
                            "", "quickfix", "refactor", "refactor.extract",
                            "refactor.inline", "refactor.rewrite", "source",
                            "source.organizeImports",
                        ],
                    },
                },
                "resolveSupport": { "properties": ["edit"] },
            },
            "rename": { "dynamicRegistration": true, "prepareSupport": true },
            "inlayHint": { "dynamicRegistration": true },
            "publishDiagnostics": {
                "relatedInformation": true,
                "versionSupport": true,
                "tagSupport": { "valueSet": [1, 2] },
            },
        },
        "window": {
            "workDoneProgress": true,
            "showMessage": { "messageActionItem": { "additionalPropertiesSupport": false } },
        },
        "general": { "positionEncodings": ["utf-16"] },
    })
}

/// Shared state written by the notification hook and background tasks.
struct ClientShared {
    state: watch::Sender<LspLifecycleState>,
    events: broadcast::Sender<LspEvent>,
}

impl ClientShared {
    fn emit(&self, event: LspEvent) {
        // No subscribers is fine.
        let _ignored = self.events.send(event);
    }

    /// Unconditional transition (emits [`LspEvent::StateChanged`] when the state changes).
    fn set_state(&self, next: LspLifecycleState) {
        self.transition(move |_| Some(next));
    }

    /// Transition computed from the current state; `None` keeps the current state.
    fn transition(&self, f: impl FnOnce(&LspLifecycleState) -> Option<LspLifecycleState>) {
        let mut change = None;
        self.state.send_if_modified(|current| match f(current) {
            Some(next) if next != *current => {
                let previous = std::mem::replace(current, next.clone());
                change = Some((previous, next));
                true
            }
            _ => false,
        });
        if let Some((previous, current)) = change {
            self.emit(LspEvent::StateChanged { previous, current });
        }
    }

    fn on_notification(&self, notification: &JsonRpcNotification) {
        let params = &notification.params;
        let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
        match notification.method.as_str() {
            "language/status" => {
                let status_type = text("type").unwrap_or_default();
                let message = text("message").unwrap_or_default();
                match status_type.as_str() {
                    "ServiceReady" | "Started" => self.transition(|current| {
                        matches!(
                            current,
                            LspLifecycleState::Initializing | LspLifecycleState::IndexingProject
                        )
                        .then_some(LspLifecycleState::Ready)
                    }),
                    "Error" => {
                        let reason = message.clone();
                        self.transition(|current| {
                            (!matches!(
                                current,
                                LspLifecycleState::ShuttingDown | LspLifecycleState::Stopped
                            ))
                            .then_some(LspLifecycleState::Failed(reason))
                        });
                    }
                    _ => {}
                }
                self.emit(LspEvent::LanguageStatus {
                    status_type,
                    message,
                });
            }
            "$/progress" => {
                let Some(token) = params.get("token").and_then(token_to_string) else {
                    return;
                };
                let value = params.get("value").cloned().unwrap_or(Value::Null);
                let field = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
                self.emit(LspEvent::WorkDoneProgress {
                    token,
                    title: field("title"),
                    message: field("message"),
                    percentage: value
                        .get("percentage")
                        .and_then(Value::as_u64)
                        .and_then(|p| u32::try_from(p).ok()),
                    finished: value.get("kind").and_then(Value::as_str) == Some("end"),
                });
            }
            "textDocument/publishDiagnostics" => {
                self.emit(LspEvent::DiagnosticsReceived {
                    uri: text("uri").unwrap_or_default(),
                    version: params
                        .get("version")
                        .and_then(Value::as_i64)
                        .and_then(|v| i32::try_from(v).ok()),
                    diagnostics_json: params.get("diagnostics").cloned().unwrap_or(Value::Null),
                });
            }
            _ => {}
        }
    }
}

/// LSP client for Eclipse `jdt.ls` (works with any LSP server speaking over a byte stream).
pub struct JdtlsClient {
    session: LspSession,
    shared: Arc<ClientShared>,
    server_handler: Arc<DefaultServerRequestHandler>,
    capabilities: Option<ServerCapabilities>,
    child: tokio::sync::Mutex<Option<Child>>,
    child_pid: Option<u32>,
    stderr_ring: Arc<Mutex<VecDeque<String>>>,
    config: ClientRuntimeConfig,
    background: Vec<JoinHandle<()>>,
}

impl fmt::Debug for JdtlsClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JdtlsClient")
            .field("state", &self.state())
            .field("project_root", &self.config.project_root)
            .field("child_pid", &self.child_pid)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl Drop for JdtlsClient {
    fn drop(&mut self) {
        for task in &self.background {
            task.abort();
        }
        // `kill_on_drop(true)` on the child terminates the server process.
    }
}

impl JdtlsClient {
    /// Spawns `jdt.ls` from a [`JdtlsLaunchSpec`] (`kill_on_drop(true)`) and performs the
    /// `initialize` / `initialized` handshake. Must be called from within a `tokio` runtime.
    ///
    /// # Errors
    /// [`LspDapError::SpawnFailed`] when the process cannot start, or any error of the
    /// `initialize` request (the process is killed in that case).
    pub async fn start_with_spec(
        spec: JdtlsLaunchSpec,
        mut config: ClientRuntimeConfig,
    ) -> Result<Self, LspDapError> {
        seed_from_spec(&mut config, &spec.initialization_options);
        let program = spec.java_bin.display().to_string();
        let spawn_failed = |reason: String| LspDapError::SpawnFailed {
            program: program.clone(),
            reason,
        };
        let mut child = Command::new(&spec.java_bin)
            .args(&spec.jvm_and_app_args)
            .envs(&spec.env)
            .current_dir(&spec.working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| spawn_failed(err.to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| spawn_failed("stdin pipe unavailable".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| spawn_failed("stdout pipe unavailable".to_owned()))?;
        let stderr = child.stderr.take();
        let pid = child.id();
        Self::start_inner(stdout, stdin, stderr, Some(child), pid, config).await
    }

    /// Performs the handshake over arbitrary streams (deterministic tests with a mock server on
    /// `tokio::io::duplex`). Must be called from within a `tokio` runtime.
    ///
    /// # Errors
    /// Any error of the `initialize` request or of the `initialized` notification.
    pub async fn start_with_streams<R, W>(
        reader: R,
        writer: W,
        config: ClientRuntimeConfig,
    ) -> Result<Self, LspDapError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::start_inner(reader, writer, None, None, None, config).await
    }

    async fn start_inner<R, W>(
        reader: R,
        writer: W,
        stderr: Option<ChildStderr>,
        child: Option<Child>,
        child_pid: Option<u32>,
        config: ClientRuntimeConfig,
    ) -> Result<Self, LspDapError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (state, _) = watch::channel(LspLifecycleState::Stopped);
        let (events, _) = broadcast::channel(config.event_capacity.max(1));
        let shared = Arc::new(ClientShared { state, events });
        shared.set_state(LspLifecycleState::Starting);

        let server_handler = Arc::new(DefaultServerRequestHandler::new(
            config.workspace_settings.clone(),
        ));
        let hook_shared = Arc::clone(&shared);
        let hook: NotificationHook = Arc::new(move |n| hook_shared.on_notification(n));
        let handler: Arc<dyn ServerRequestHandler> = server_handler.clone();
        let session = LspSession::from_streams_with_options(
            reader,
            writer,
            SessionOptions {
                server_handler: Some(handler),
                notification_hook: Some(hook),
                ..SessionOptions::default()
            },
        );

        let stderr_ring = Arc::new(Mutex::new(VecDeque::new()));
        let mut background = Vec::new();
        if let Some(stderr) = stderr {
            background.push(tokio::spawn(stderr_loop(
                stderr,
                Arc::clone(&stderr_ring),
                config.stderr_ring_capacity.max(1),
                Arc::clone(&shared),
            )));
        }
        background.push(tokio::spawn(watch_transport(
            session.closed_watch(),
            Arc::clone(&shared),
        )));

        let mut client = Self {
            session,
            shared,
            server_handler,
            capabilities: None,
            child: tokio::sync::Mutex::new(child),
            child_pid,
            stderr_ring,
            config,
            background,
        };
        if let Err(err) = client.handshake().await {
            client
                .shared
                .set_state(LspLifecycleState::Failed(err.to_string()));
            if let Some(mut child) = client.child.get_mut().take() {
                let _ignored = child.kill().await;
            }
            return Err(err);
        }
        Ok(client)
    }

    async fn handshake(&mut self) -> Result<(), LspDapError> {
        self.shared.set_state(LspLifecycleState::Initializing);
        let root = &self.config.project_root;
        let root_uri = path_to_lsp_uri(root)?;
        let folder_name = root.file_name().map_or_else(
            || root.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let init_options = &self.config.initialization_options;
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "Kova", "version": env!("CARGO_PKG_VERSION") },
            "rootPath": root.display().to_string(),
            "rootUri": root_uri.as_str(),
            "workspaceFolders": [{ "uri": root_uri.as_str(), "name": folder_name }],
            "initializationOptions": if init_options.is_null() { Value::Null } else { init_options.clone() },
            "capabilities": build_client_capabilities(),
            "trace": "off",
        });
        let result: InitializeResult = self
            .session
            .request("initialize", params, self.config.initialize_timeout)
            .await?;
        self.capabilities = Some(result.capabilities);
        self.session.notify("initialized", json!({})).await?;
        self.shared.transition(|current| {
            (*current == LspLifecycleState::Initializing)
                .then_some(LspLifecycleState::IndexingProject)
        });
        Ok(())
    }

    /// Waits until the server reports readiness (`language/status` `ServiceReady` / `Started`).
    /// Returns immediately when already [`LspLifecycleState::Ready`].
    ///
    /// # Errors
    /// [`LspDapError::RequestTimeout`] (method `language/status`) when `timeout` elapses and
    /// [`LspDapError::InvalidState`] when the session failed or is shutting down.
    pub async fn wait_until_service_ready(&self, timeout: Duration) -> Result<(), LspDapError> {
        let mut rx = self.shared.state.subscribe();
        let wait = async move {
            loop {
                let state = rx.borrow_and_update().clone();
                match state {
                    LspLifecycleState::Ready => return Ok(()),
                    LspLifecycleState::Starting
                    | LspLifecycleState::Initializing
                    | LspLifecycleState::IndexingProject => {}
                    other => {
                        return Err(LspDapError::InvalidState {
                            current: other.to_string(),
                            expected: LspLifecycleState::Ready.to_string(),
                        });
                    }
                }
                if rx.changed().await.is_err() {
                    return Err(LspDapError::TransportClosed);
                }
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .unwrap_or_else(|_| {
                Err(LspDapError::RequestTimeout {
                    method: "language/status".to_owned(),
                    timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                })
            })
    }

    /// Sends `shutdown` then `exit`, waits for the process to exit within `timeout` and kills it
    /// otherwise. Always ends in [`LspLifecycleState::Stopped`].
    ///
    /// # Errors
    /// [`LspDapError::Io`] when the process could not be killed.
    pub async fn shutdown_graceful(&self, timeout: Duration) -> Result<(), LspDapError> {
        let deadline = Instant::now() + timeout;
        self.shared.set_state(LspLifecycleState::ShuttingDown);
        if !self.session.is_closed() {
            // The server may be unresponsive: errors and timeouts are tolerated.
            let _shutdown: Result<Value, _> = self
                .session
                .request_value("shutdown", Value::Null, timeout)
                .await;
            let _exit = self.session.notify("exit", Value::Null).await;
        }
        let mut child = self.child.lock().await;
        let result = match child.as_mut() {
            Some(process) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match tokio::time::timeout(remaining, process.wait()).await {
                    Ok(Ok(_status)) => Ok(()),
                    Ok(Err(_)) | Err(_) => process
                        .kill()
                        .await
                        .map_err(|err| LspDapError::io("jdt.ls process", &err)),
                }
            }
            None => Ok(()),
        };
        *child = None;
        self.shared.set_state(LspLifecycleState::Stopped);
        result
    }

    /// Sends a request with the configured default timeout.
    ///
    /// # Errors
    /// See [`LspSession::request`].
    pub async fn request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R, LspDapError> {
        self.session
            .request(method, params, self.config.request_timeout)
            .await
    }

    /// Sends a notification.
    ///
    /// # Errors
    /// See [`LspSession::notify`].
    pub async fn notify<P: Serialize>(&self, method: &str, params: P) -> Result<(), LspDapError> {
        self.session.notify(method, params).await
    }

    /// Current lifecycle state.
    #[must_use]
    pub fn state(&self) -> LspLifecycleState {
        self.shared.state.borrow().clone()
    }

    /// Watch channel of the lifecycle state.
    #[must_use]
    pub fn watch_state(&self) -> watch::Receiver<LspLifecycleState> {
        self.shared.state.subscribe()
    }

    /// Subscribes to [`LspEvent`]s published after this call.
    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<LspEvent> {
        self.shared.events.subscribe()
    }

    /// Server capabilities from the `initialize` response.
    #[must_use]
    pub fn capabilities(&self) -> Option<&ServerCapabilities> {
        self.capabilities.as_ref()
    }

    /// OS process id of the server (`None` for stream-based sessions).
    #[must_use]
    pub fn child_pid(&self) -> Option<u32> {
        self.child_pid
    }

    /// Last `stderr` lines of the server (oldest first).
    #[must_use]
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr_ring
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// Underlying JSON-RPC session.
    #[must_use]
    pub fn session(&self) -> &LspSession {
        &self.session
    }

    /// Handler answering server requests (registrations, progress tokens, `applyEdit`).
    #[must_use]
    pub fn server_request_handler(&self) -> &Arc<DefaultServerRequestHandler> {
        &self.server_handler
    }

    /// Project root of this session.
    #[must_use]
    pub fn project_root(&self) -> &Path {
        &self.config.project_root
    }
}

async fn stderr_loop(
    stderr: ChildStderr,
    ring: Arc<Mutex<VecDeque<String>>>,
    capacity: usize,
    shared: Arc<ClientShared>,
) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        {
            let mut ring = ring.lock().unwrap_or_else(PoisonError::into_inner);
            while ring.len() >= capacity {
                ring.pop_front();
            }
            ring.push_back(line.clone());
        }
        shared.emit(LspEvent::ServerStderrLine(line));
    }
}

async fn watch_transport(mut closed: watch::Receiver<bool>, shared: Arc<ClientShared>) {
    if closed.wait_for(|closed| *closed).await.is_ok() {
        shared.transition(|current| {
            (!matches!(
                current,
                LspLifecycleState::ShuttingDown
                    | LspLifecycleState::Stopped
                    | LspLifecycleState::Hibernated
                    | LspLifecycleState::Failed(_)
            ))
            .then(|| LspLifecycleState::Failed(LspDapError::TransportClosed.to_string()))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_cover_gate2_q2() {
        let caps = build_client_capabilities();
        assert_eq!(caps["workspace"]["applyEdit"], json!(true));
        assert_eq!(
            caps["workspace"]["workspaceEdit"]["documentChanges"],
            json!(true)
        );
        assert_eq!(caps["window"]["workDoneProgress"], json!(true));
        assert_eq!(
            caps["textDocument"]["synchronization"]["didSave"],
            json!(true)
        );
        assert_eq!(
            caps["textDocument"]["rename"]["prepareSupport"],
            json!(true)
        );
        assert_eq!(
            caps["textDocument"]["completion"]["completionItem"]["snippetSupport"],
            json!(true)
        );
        assert!(caps["textDocument"]["inlayHint"].is_object());
        assert!(caps["textDocument"]["codeAction"]["codeActionLiteralSupport"].is_object());
        assert!(serde_json::from_value::<lsp_types::ClientCapabilities>(caps).is_ok());
    }

    #[test]
    fn runtime_config_exposes_java_and_kova_sections() {
        let effective = EffectiveConfig::merge(&kova_config::KovaSettings::default(), None);
        let config = ClientRuntimeConfig::from_effective_config(
            "/tmp/p",
            &effective,
            json!({ "settings": { "java": { "home": "/jdk" } } }),
        );
        assert_eq!(config.workspace_settings["java"]["home"], json!("/jdk"));
        assert!(config.workspace_settings["kova"].is_object());
    }

    #[test]
    fn spec_seeds_empty_config() {
        let options = json!({ "settings": { "java": { "home": "/jdk" } } });
        let mut config = ClientRuntimeConfig::new("/tmp/p");
        seed_from_spec(&mut config, &options);
        assert_eq!(config.initialization_options, options);
        assert_eq!(
            config.workspace_settings,
            json!({ "java": { "home": "/jdk" } })
        );

        let mut custom = ClientRuntimeConfig::new("/tmp/p");
        custom.workspace_settings = json!({ "kova": {} });
        seed_from_spec(&mut custom, &options);
        assert_eq!(custom.workspace_settings, json!({ "kova": {} }));
    }

    #[test]
    fn state_display_names() {
        assert_eq!(
            LspLifecycleState::IndexingProject.to_string(),
            "IndexingProject"
        );
        assert_eq!(
            LspLifecycleState::Failed("x".into()).to_string(),
            "Failed(x)"
        );
    }
}
