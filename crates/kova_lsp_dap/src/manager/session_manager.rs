//! Multi-project `jdt.ls` session manager with RAM limits and auto-hibernation (KOVA-038, FR-3).
//!
//! [`JdtlsSessionManager`] owns one isolated `jdt.ls` session per project (keyed by the
//! workspace hash of [`KovaPaths`]). A watchdog driven by the Tokio clock
//! ([`JdtlsSessionManager::run_watchdog_tick`] / [`JdtlsSessionManager::spawn_watchdog`])
//! samples the RSS of every running server, hibernates background projects that stayed idle
//! for `auto_hibernate_minutes`, and enforces the RSS budget and `max_active_jvms`.
//!
//! Hibernation persists the diagnostics and open documents to
//! `~/.cache/kova/workspaces/<hash>/hibernated_state.json`, shuts `jdt.ls` down and keeps the
//! in-memory [`DiagnosticsStore`] so the *Problems* panel stays populated. Waking
//! ([`JdtlsSessionManager::ensure_ready`], focus regain or an edit) is *single-flight*: every
//! concurrent caller awaits the same launch, which runs in its own task so cancelling a caller
//! never strands the others.
//!
//! Locking discipline: the per-project `std::sync::Mutex` guarding [`ProjectSessionPhase`] is
//! never held across an `.await`; transitions that await (launch, shutdown) are serialised by a
//! per-project async `lifecycle` mutex.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kova_buffer::BufferSnapshot;
use kova_config::{EffectiveConfig, KovaPaths};
use kova_core::perf::budgets::DEFAULT_JVM_HIBERNATION_IDLE;
use kova_toolchain::JdtlsLaunchSpec;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::error::LspDapError;
use crate::lsp::{
    ClientRuntimeConfig, DiagnosticSnapshot, DiagnosticsStore, DocumentSyncTracker, JdtlsClient,
    LspEvent, LspLifecycleState, lsp_uri_to_path,
};
use crate::manager::hibernation::{
    HIBERNATION_SCHEMA_VERSION, HibernatedOpenDocument, HibernationStateSnapshot, HibernationStore,
};
use crate::manager::memory::{
    ChildMemorySample, MemoryPolicy, MemoryPressureLevel, ProcessMemoryProbe,
};

/// Default watchdog period.
pub const DEFAULT_WATCHDOG_INTERVAL: Duration = Duration::from_secs(30);
/// Default time granted to `jdt.ls` for `shutdown` / `exit` before it is killed.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
/// Capacity of the [`SessionEvent`] broadcast channel.
pub const SESSION_EVENT_CAPACITY: usize = 256;

/// Stable identifier of a project (workspace hash of its canonical root).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProjectId(String);

impl ProjectId {
    /// Wraps a workspace hash.
    #[must_use]
    pub fn new(hash: impl Into<String>) -> Self {
        Self(hash.into())
    }

    /// The workspace hash.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Kind of user activity that resets the idle clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActivityKind {
    /// The user edited a buffer of the project (wakes a hibernated session).
    EditorEdit,
    /// A semantic LSP request was issued (wakes a hibernated session).
    LspRequest,
    /// A file of the project was opened (wakes a hibernated session).
    FileOpen,
    /// The project window gained focus.
    FocusGained,
}

impl ActivityKind {
    const fn wakes_session(self) -> bool {
        matches!(self, Self::EditorEdit | Self::LspRequest | Self::FileOpen)
    }
}

/// Lifecycle phase of a project session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectSessionPhase {
    /// Registered, `jdt.ls` not started yet (lazy start).
    Cold,
    /// `jdt.ls` is being launched and initialised.
    Starting,
    /// `jdt.ls` answers semantic requests.
    Ready {
        /// OS process id of the server, when known.
        pid: Option<u32>,
        /// Last sampled RSS in bytes.
        last_rss_bytes: Option<u64>,
    },
    /// Diagnostics are being saved and `jdt.ls` is shutting down.
    Hibernating,
    /// `jdt.ls` stopped to free RAM; diagnostics kept in the cache and the store.
    Hibernated {
        /// Number of diagnostics preserved.
        diagnostics_count: usize,
        /// UNIX timestamp (seconds) of the hibernation.
        hibernated_at_unix_s: u64,
        /// RSS of the server right before it was stopped.
        last_rss_bytes_freed: Option<u64>,
    },
    /// `jdt.ls` is being restarted after the user came back.
    Waking,
    /// Launch failure or unexpected crash; the next [`JdtlsSessionManager::ensure_ready`] retries.
    Failed(String),
}

impl ProjectSessionPhase {
    /// Whether a `jdt.ls` process is running or being launched.
    #[must_use]
    pub const fn holds_jvm(&self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Ready { .. } | Self::Waking | Self::Hibernating
        )
    }

    /// Whether the phase is [`ProjectSessionPhase::Ready`].
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    /// Whether the phase is [`ProjectSessionPhase::Hibernated`].
    #[must_use]
    pub const fn is_hibernated(&self) -> bool {
        matches!(self, Self::Hibernated { .. })
    }
}

/// Per-project settings derived from the effective configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProjectSessionConfig {
    /// Idle time after which a background session is hibernated (`None` disables).
    pub auto_hibernate_idle: Option<Duration>,
    /// RSS budget of the project's `jdt.ls`.
    pub memory_policy: MemoryPolicy,
}

impl Default for ProjectSessionConfig {
    fn default() -> Self {
        Self {
            auto_hibernate_idle: Some(DEFAULT_JVM_HIBERNATION_IDLE),
            memory_policy: MemoryPolicy::default(),
        }
    }
}

impl ProjectSessionConfig {
    /// `auto_hibernate_minutes` (`0` disables) and `jdtls_max_ram_mb` of `effective`.
    #[must_use]
    pub fn from_effective_config(effective: &EffectiveConfig) -> Self {
        let minutes = effective.settings.jvm.auto_hibernate_minutes;
        Self {
            auto_hibernate_idle: (minutes > 0)
                .then(|| Duration::from_secs(u64::from(minutes) * 60)),
            memory_policy: MemoryPolicy::from_effective_config(effective),
        }
    }
}

/// Manager-wide settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionManagerConfig {
    /// Whether the focused project may be hibernated after the idle timeout (default `false`).
    pub hibernate_focused_project: bool,
    /// Maximum number of concurrently running `jdt.ls` processes (LRU hibernation above it).
    pub max_active_jvms: Option<usize>,
    /// Period of [`JdtlsSessionManager::spawn_watchdog`].
    pub watchdog_interval: Duration,
    /// Time granted to `jdt.ls` for `shutdown` / `exit`.
    pub shutdown_timeout: Duration,
}

impl Default for SessionManagerConfig {
    fn default() -> Self {
        Self {
            hibernate_focused_project: false,
            max_active_jvms: None,
            watchdog_interval: DEFAULT_WATCHDOG_INTERVAL,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
        }
    }
}

/// Events published by the manager (status bar, telemetry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// A project changed phase.
    PhaseChanged {
        /// Project.
        project_id: ProjectId,
        /// Phase before the transition.
        previous: ProjectSessionPhase,
        /// Phase after the transition.
        current: ProjectSessionPhase,
    },
    /// The RSS of a running server was sampled.
    MemorySampled {
        /// Project.
        project_id: ProjectId,
        /// Server PID.
        pid: u32,
        /// RSS in bytes.
        rss_bytes: u64,
        /// Pressure relative to the project's budget.
        pressure: MemoryPressureLevel,
    },
    /// A project was hibernated.
    Hibernated {
        /// Project.
        project_id: ProjectId,
        /// Number of diagnostics persisted.
        diagnostics_saved: usize,
        /// RSS freed by stopping the server, when known.
        rss_freed_bytes: Option<u64>,
    },
    /// A project was woken.
    Woken {
        /// Project.
        project_id: ProjectId,
        /// Number of open buffers replayed with `didOpen`.
        restored_documents: usize,
    },
    /// The RSS budget or `max_active_jvms` triggered an action.
    MemoryLimitAction {
        /// Project acted upon.
        project_id: ProjectId,
        /// RSS of that project (last known, `0` when unknown).
        rss_bytes: u64,
        /// `"hibernated"`, `"restarted"` or `"lru_hibernated"`.
        action_taken: String,
    },
}

/// Result of [`JdtlsLauncher::launch`].
#[derive(Debug)]
pub struct LaunchedJdtls {
    /// Initialised client (handshake done).
    pub client: JdtlsClient,
    /// OS process id used for RSS sampling (`None` disables sampling).
    pub pid: Option<u32>,
}

/// Boxed future returned by [`JdtlsLauncher::launch`].
pub type LaunchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<LaunchedJdtls, LspDapError>> + Send + 'a>>;

/// Starts `jdt.ls` for a project (production: [`SpecJdtlsLauncher`]; tests: in-memory streams).
pub trait JdtlsLauncher: Send + Sync {
    /// Launches and initialises a server for `project_root`.
    fn launch<'a>(
        &'a self,
        project_root: &'a Path,
        effective_config: &'a EffectiveConfig,
    ) -> LaunchFuture<'a>;
}

/// Production launcher spawning `jdt.ls` from a resolved [`JdtlsLaunchSpec`].
#[derive(Debug, Clone)]
pub struct SpecJdtlsLauncher {
    spec: JdtlsLaunchSpec,
}

impl SpecJdtlsLauncher {
    /// Launcher for `spec`.
    #[must_use]
    pub const fn new(spec: JdtlsLaunchSpec) -> Self {
        Self { spec }
    }
}

impl JdtlsLauncher for SpecJdtlsLauncher {
    fn launch<'a>(
        &'a self,
        project_root: &'a Path,
        effective_config: &'a EffectiveConfig,
    ) -> LaunchFuture<'a> {
        Box::pin(async move {
            let config = ClientRuntimeConfig::from_effective_config(
                project_root,
                effective_config,
                self.spec.initialization_options.clone(),
            );
            let client = JdtlsClient::start_with_spec(self.spec.clone(), config).await?;
            let pid = client.child_pid();
            Ok(LaunchedJdtls { client, pid })
        })
    }
}

/// A buffer open in the editor, replayed with `didOpen` when a session wakes.
#[derive(Debug, Clone)]
pub struct OpenBuffer {
    /// Absolute path of the document.
    pub path: PathBuf,
    /// LSP language id.
    pub language_id: String,
    /// Current in-memory content (including unsaved edits).
    pub snapshot: BufferSnapshot,
}

/// Supplies the buffers currently open in the editor for a project.
pub trait OpenBufferProvider: Send + Sync {
    /// Buffers to (re)open in a freshly started server.
    fn open_buffers(&self) -> Vec<OpenBuffer>;
}

/// Outcome of one [`JdtlsSessionManager::run_watchdog_tick`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchdogReport {
    /// RSS samples taken.
    pub samples: Vec<(ProjectId, ChildMemorySample)>,
    /// Projects hibernated because they were idle.
    pub idle_hibernated: Vec<ProjectId>,
    /// Actions taken because of the RSS budget or `max_active_jvms`.
    pub memory_actions: Vec<(ProjectId, String)>,
}

/// Handle of the background watchdog; aborts it when dropped.
#[derive(Debug)]
pub struct WatchdogHandle {
    task: JoinHandle<()>,
}

impl WatchdogHandle {
    /// Stops the watchdog.
    pub fn stop(self) {
        self.task.abort();
    }
}

impl Drop for WatchdogHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type WakeResult = Option<Result<Arc<JdtlsClient>, LspDapError>>;

/// Why a hibernation is requested (decides which guards are re-checked under the lock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HibernateReason {
    /// Explicit API call: no guard.
    Explicit,
    /// Idle timeout: background (or allowed focused) and still idle.
    Idle(Duration),
    /// RSS budget / LRU: background only.
    Background,
    /// Restart of an over-budget focused server.
    Restart,
}

struct ProjectState {
    phase: ProjectSessionPhase,
    focused: bool,
    last_activity_at: Instant,
    client: Option<Arc<JdtlsClient>>,
    sync_tracker: Arc<tokio::sync::Mutex<DocumentSyncTracker>>,
    buffer_provider: Option<Arc<dyn OpenBufferProvider>>,
    wake: Option<watch::Receiver<WakeResult>>,
    diagnostics_task: Option<JoinHandle<()>>,
    cancelled: bool,
}

struct ProjectEntry {
    id: ProjectId,
    project_root: PathBuf,
    effective_config: EffectiveConfig,
    launcher: Arc<dyn JdtlsLauncher>,
    config: ProjectSessionConfig,
    diagnostics: Arc<DiagnosticsStore>,
    state: Mutex<ProjectState>,
    lifecycle: tokio::sync::Mutex<()>,
}

impl ProjectEntry {
    fn lock(&self) -> MutexGuard<'_, ProjectState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct ManagerShared {
    paths: KovaPaths,
    config: SessionManagerConfig,
    probe: Arc<dyn ProcessMemoryProbe>,
    projects: Mutex<HashMap<ProjectId, Arc<ProjectEntry>>>,
    events: broadcast::Sender<SessionEvent>,
}

impl ManagerShared {
    fn emit(&self, event: SessionEvent) {
        let _no_subscribers = self.events.send(event);
    }

    fn entries(&self) -> Vec<Arc<ProjectEntry>> {
        self.projects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    fn entry(&self, id: &ProjectId) -> Result<Arc<ProjectEntry>, LspDapError> {
        self.projects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
            .ok_or_else(|| LspDapError::UnknownProject {
                project_id: id.to_string(),
            })
    }

    fn set_phase(&self, entry: &ProjectEntry, state: &mut ProjectState, next: ProjectSessionPhase) {
        if state.phase == next {
            return;
        }
        let previous = std::mem::replace(&mut state.phase, next.clone());
        self.emit(SessionEvent::PhaseChanged {
            project_id: entry.id.clone(),
            previous,
            current: next,
        });
    }
}

/// Multi-project `jdt.ls` session manager. Cheap to clone (shared state).
#[derive(Clone)]
pub struct JdtlsSessionManager {
    shared: Arc<ManagerShared>,
}

impl fmt::Debug for JdtlsSessionManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JdtlsSessionManager")
            .field("config", &self.shared.config)
            .field("projects", &self.shared.entries().len())
            .finish_non_exhaustive()
    }
}

fn unix_now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl JdtlsSessionManager {
    /// Creates a manager storing hibernation state under `paths` and sampling RSS with `probe`.
    #[must_use]
    pub fn new(
        paths: KovaPaths,
        config: SessionManagerConfig,
        probe: Arc<dyn ProcessMemoryProbe>,
    ) -> Self {
        let (events, _) = broadcast::channel(SESSION_EVENT_CAPACITY);
        Self {
            shared: Arc::new(ManagerShared {
                paths,
                config,
                probe,
                projects: Mutex::new(HashMap::new()),
                events,
            }),
        }
    }

    /// Manager-wide settings.
    #[must_use]
    pub fn config(&self) -> &SessionManagerConfig {
        &self.shared.config
    }

    /// Subscribes to [`SessionEvent`]s published after this call.
    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<SessionEvent> {
        self.shared.events.subscribe()
    }

    /// Registers a project (idempotent: an already registered root returns its id unchanged).
    /// Diagnostics of a previous hibernation are loaded into the store right away.
    ///
    /// # Errors
    /// [`LspDapError::Io`] when the root cannot be canonicalised or the state file is unreadable.
    pub fn register_project(
        &self,
        project_root: PathBuf,
        effective_config: EffectiveConfig,
        launcher: Arc<dyn JdtlsLauncher>,
    ) -> Result<ProjectId, LspDapError> {
        let hash = self
            .shared
            .paths
            .workspace_hash(&project_root)
            .map_err(|err| LspDapError::Io {
                path: project_root.display().to_string(),
                message: err.to_string(),
            })?;
        let id = ProjectId::new(hash);
        if self.shared.entry(&id).is_ok() {
            return Ok(id);
        }
        let diagnostics = Arc::new(DiagnosticsStore::new());
        if let Some(saved) =
            HibernationStore::load_hibernated_state(&self.shared.paths, &project_root)?
        {
            diagnostics.restore_snapshot(saved.diagnostics);
        }
        let entry = Arc::new(ProjectEntry {
            id: id.clone(),
            config: ProjectSessionConfig::from_effective_config(&effective_config),
            project_root,
            effective_config,
            launcher,
            diagnostics,
            state: Mutex::new(ProjectState {
                phase: ProjectSessionPhase::Cold,
                focused: false,
                last_activity_at: Instant::now(),
                client: None,
                sync_tracker: Arc::new(tokio::sync::Mutex::new(DocumentSyncTracker::new())),
                buffer_provider: None,
                wake: None,
                diagnostics_task: None,
                cancelled: false,
            }),
            lifecycle: tokio::sync::Mutex::new(()),
        });
        self.shared
            .projects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(id.clone())
            .or_insert(entry);
        Ok(id)
    }

    /// Registered project ids (arbitrary order).
    #[must_use]
    pub fn project_ids(&self) -> Vec<ProjectId> {
        self.shared.entries().iter().map(|e| e.id.clone()).collect()
    }

    /// Current phase of a project.
    ///
    /// # Errors
    /// [`LspDapError::UnknownProject`].
    pub fn phase(&self, project_id: &ProjectId) -> Result<ProjectSessionPhase, LspDapError> {
        Ok(self.shared.entry(project_id)?.lock().phase.clone())
    }

    /// Project root of a project.
    ///
    /// # Errors
    /// [`LspDapError::UnknownProject`].
    pub fn project_root(&self, project_id: &ProjectId) -> Result<PathBuf, LspDapError> {
        Ok(self.shared.entry(project_id)?.project_root.clone())
    }

    /// Per-project settings derived from the effective configuration.
    ///
    /// # Errors
    /// [`LspDapError::UnknownProject`].
    pub fn project_config(
        &self,
        project_id: &ProjectId,
    ) -> Result<ProjectSessionConfig, LspDapError> {
        Ok(self.shared.entry(project_id)?.config)
    }

    /// Diagnostics store of a project (kept across hibernation).
    ///
    /// # Errors
    /// [`LspDapError::UnknownProject`].
    pub fn diagnostics(
        &self,
        project_id: &ProjectId,
    ) -> Result<Arc<DiagnosticsStore>, LspDapError> {
        Ok(Arc::clone(&self.shared.entry(project_id)?.diagnostics))
    }

    /// Document sync tracker of the current server (replaced on every wake).
    ///
    /// # Errors
    /// [`LspDapError::UnknownProject`].
    pub fn sync_tracker(
        &self,
        project_id: &ProjectId,
    ) -> Result<Arc<tokio::sync::Mutex<DocumentSyncTracker>>, LspDapError> {
        Ok(Arc::clone(
            &self.shared.entry(project_id)?.lock().sync_tracker,
        ))
    }

    /// Sets the provider of open buffers replayed on every wake (including focus-triggered ones).
    ///
    /// # Errors
    /// [`LspDapError::UnknownProject`].
    pub fn set_buffer_provider(
        &self,
        project_id: &ProjectId,
        provider: Option<Arc<dyn OpenBufferProvider>>,
    ) -> Result<(), LspDapError> {
        self.shared.entry(project_id)?.lock().buffer_provider = provider;
        Ok(())
    }

    /// Updates the focus flag. Gaining focus counts as activity and wakes a hibernated session in
    /// the background (when called inside a Tokio runtime).
    ///
    /// # Errors
    /// [`LspDapError::UnknownProject`].
    pub fn set_project_focus(
        &self,
        project_id: &ProjectId,
        focused: bool,
    ) -> Result<(), LspDapError> {
        let entry = self.shared.entry(project_id)?;
        let mut state = entry.lock();
        let gained = focused && !state.focused;
        state.focused = focused;
        if gained {
            state.last_activity_at = Instant::now();
            if state.phase.is_hibernated() {
                self.wake_in_background(&entry, &mut state);
            }
        }
        Ok(())
    }

    /// Whether the project is focused.
    ///
    /// # Errors
    /// [`LspDapError::UnknownProject`].
    pub fn is_focused(&self, project_id: &ProjectId) -> Result<bool, LspDapError> {
        Ok(self.shared.entry(project_id)?.lock().focused)
    }

    /// Resets the idle clock. Edits, LSP requests and file opens on a hibernated project wake it
    /// in the background (when called inside a Tokio runtime).
    ///
    /// # Errors
    /// [`LspDapError::UnknownProject`].
    pub fn record_activity(
        &self,
        project_id: &ProjectId,
        kind: ActivityKind,
    ) -> Result<(), LspDapError> {
        let entry = self.shared.entry(project_id)?;
        let mut state = entry.lock();
        state.last_activity_at = Instant::now();
        if kind.wakes_session() && state.phase.is_hibernated() {
            self.wake_in_background(&entry, &mut state);
        }
        Ok(())
    }

    fn wake_in_background(&self, entry: &Arc<ProjectEntry>, state: &mut ProjectState) {
        if tokio::runtime::Handle::try_current().is_ok() {
            let _receiver = begin_wake(&self.shared, entry, state);
        }
    }

    /// Returns a ready client, starting or waking `jdt.ls` when needed. Concurrent callers share
    /// one launch (single-flight). `buffer_provider`, when given, replaces the registered one and
    /// its buffers are replayed with `didOpen` on the new server.
    ///
    /// The session is `Ready` once the `initialize` handshake completed; `jdt.ls` may still be
    /// indexing (see [`JdtlsClient::wait_until_service_ready`]).
    ///
    /// # Errors
    /// [`LspDapError::UnknownProject`], [`LspDapError::SessionShutDown`] or the launch error
    /// (delivered to every waiter; the phase becomes `Failed` and the next call retries).
    pub async fn ensure_ready(
        &self,
        project_id: &ProjectId,
        buffer_provider: Option<Arc<dyn OpenBufferProvider>>,
    ) -> Result<Arc<JdtlsClient>, LspDapError> {
        let entry = self.shared.entry(project_id)?;
        if let Some(provider) = buffer_provider {
            entry.lock().buffer_provider = Some(provider);
        }
        loop {
            let mut receiver = {
                let mut state = entry.lock();
                state.last_activity_at = Instant::now();
                if state.cancelled {
                    return Err(LspDapError::SessionShutDown {
                        project_root: entry.project_root.display().to_string(),
                    });
                }
                if state.phase.is_ready() {
                    match state.client.as_ref().map(|c| c.state()) {
                        Some(LspLifecycleState::Failed(reason)) => {
                            mark_crashed(&self.shared, &entry, &mut state, &reason);
                        }
                        Some(LspLifecycleState::Stopped) => {
                            mark_crashed(&self.shared, &entry, &mut state, "server stopped");
                        }
                        Some(_) => {
                            if let Some(client) = &state.client {
                                return Ok(Arc::clone(client));
                            }
                        }
                        None => {}
                    }
                }
                if state.phase == ProjectSessionPhase::Hibernating {
                    None
                } else {
                    begin_wake(&self.shared, &entry, &mut state)
                }
            };
            let Some(receiver) = receiver.as_mut() else {
                // Hibernation in progress: wait for it to finish, then wake.
                drop(entry.lifecycle.lock().await);
                continue;
            };
            let outcome = receiver
                .wait_for(Option::is_some)
                .await
                .map(|value| value.clone());
            if let Ok(Some(result)) = outcome {
                return result;
            }
            // The wake task ended without a result (panic in the launcher or buffer provider):
            // reset the phase so the next call relaunches instead of waiting on a dead channel.
            let mut state = entry.lock();
            if state
                .wake
                .as_ref()
                .is_some_and(|wake| wake.same_channel(receiver))
            {
                state.wake = None;
                let reason = "jdt.ls wake task terminated unexpectedly".to_owned();
                self.shared.set_phase(
                    &entry,
                    &mut state,
                    ProjectSessionPhase::Failed(reason.clone()),
                );
                return Err(LspDapError::SpawnFailed {
                    program: "jdt.ls".to_owned(),
                    reason,
                });
            }
        }
    }

    /// Hibernates a project now (focused or not): persists diagnostics and open documents,
    /// shuts `jdt.ls` down and keeps the diagnostics store. No-op unless the phase is `Ready`.
    ///
    /// # Errors
    /// [`LspDapError::UnknownProject`] or a failure to persist the state (the session then stays
    /// `Ready`).
    pub async fn hibernate_project(&self, project_id: &ProjectId) -> Result<(), LspDapError> {
        let entry = self.shared.entry(project_id)?;
        hibernate_entry(&self.shared, &entry, HibernateReason::Explicit)
            .await
            .map(|_| ())
    }

    /// One deterministic watchdog pass: RSS sampling and budget enforcement, idle hibernation
    /// and `max_active_jvms` LRU hibernation.
    ///
    /// # Errors
    /// The first hibernation error; the remaining checks still run.
    pub async fn run_watchdog_tick(&self) -> Result<WatchdogReport, LspDapError> {
        let shared = &self.shared;
        let mut report = WatchdogReport::default();
        let mut first_error = None;
        let mut record = |result: Result<bool, LspDapError>| match result {
            Ok(done) => done,
            Err(err) => {
                first_error.get_or_insert(err);
                false
            }
        };

        // 1. RSS sampling and budget enforcement.
        for entry in shared.entries() {
            let pid = match &entry.lock().phase {
                ProjectSessionPhase::Ready { pid: Some(pid), .. } => *pid,
                _ => continue,
            };
            let rss = match shared.probe.sample_pid_rss(pid).await {
                Ok(Some(rss)) => rss,
                Ok(None) => continue,
                Err(err) => {
                    record(Err(err));
                    continue;
                }
            };
            let sample = ChildMemorySample {
                pid,
                rss_bytes: rss,
                sampled_at: Instant::now(),
            };
            let pressure = entry.config.memory_policy.evaluate_rss(rss);
            let focused = {
                let mut state = entry.lock();
                if let ProjectSessionPhase::Ready {
                    pid: Some(current),
                    last_rss_bytes,
                } = &mut state.phase
                {
                    if *current == pid {
                        *last_rss_bytes = Some(rss);
                    }
                }
                state.focused
            };
            shared.emit(SessionEvent::MemorySampled {
                project_id: entry.id.clone(),
                pid,
                rss_bytes: rss,
                pressure,
            });
            report.samples.push((entry.id.clone(), sample));
            if pressure != MemoryPressureLevel::Exceeded {
                continue;
            }
            let (reason, action) = if focused {
                (HibernateReason::Restart, "restarted")
            } else {
                (HibernateReason::Background, "hibernated")
            };
            if record(hibernate_entry(shared, &entry, reason).await) {
                if reason == HibernateReason::Restart {
                    let mut state = entry.lock();
                    let _receiver = begin_wake(shared, &entry, &mut state);
                }
                shared.emit(SessionEvent::MemoryLimitAction {
                    project_id: entry.id.clone(),
                    rss_bytes: rss,
                    action_taken: action.to_owned(),
                });
                report
                    .memory_actions
                    .push((entry.id.clone(), action.to_owned()));
            }
        }

        // 2. Idle hibernation.
        for entry in shared.entries() {
            let Some(idle) = entry.config.auto_hibernate_idle else {
                continue;
            };
            if record(hibernate_entry(shared, &entry, HibernateReason::Idle(idle)).await) {
                report.idle_hibernated.push(entry.id.clone());
            }
        }

        // 3. `max_active_jvms` LRU.
        for (id, action) in enforce_max_active_jvms(shared).await {
            match action {
                Ok(()) => report
                    .memory_actions
                    .push((id, "lru_hibernated".to_owned())),
                Err(err) => {
                    record(Err(err));
                }
            }
        }

        match first_error {
            Some(err) => Err(err),
            None => Ok(report),
        }
    }

    /// Spawns a task calling [`Self::run_watchdog_tick`] every `watchdog_interval` (first tick
    /// after one interval). Must be called inside a Tokio runtime.
    #[must_use]
    pub fn spawn_watchdog(&self) -> WatchdogHandle {
        let manager = self.clone();
        let period = self
            .shared
            .config
            .watchdog_interval
            .max(Duration::from_millis(1));
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval_at(Instant::now() + period, period);
            interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                // Errors are transient (a failed save keeps the session Ready); retry next tick.
                let _report = manager.run_watchdog_tick().await;
            }
        });
        WatchdogHandle { task }
    }

    /// Removes a project: cancels or awaits an in-flight launch and stops its server so no JVM
    /// outlives the session.
    ///
    /// # Errors
    /// [`LspDapError::UnknownProject`].
    pub async fn unregister_project(&self, project_id: &ProjectId) -> Result<(), LspDapError> {
        let entry = self
            .shared
            .projects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(project_id)
            .ok_or_else(|| LspDapError::UnknownProject {
                project_id: project_id.to_string(),
            })?;
        stop_entry(&self.shared, &entry).await;
        Ok(())
    }

    /// Stops every session (see [`Self::unregister_project`]).
    pub async fn shutdown_all(&self) {
        let entries: Vec<_> = self
            .shared
            .projects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .map(|(_, entry)| entry)
            .collect();
        for entry in entries {
            stop_entry(&self.shared, &entry).await;
        }
    }
}

/// Starts a single-flight wake unless one is in progress; returns its receiver.
/// Must be called with the project state locked and inside a Tokio runtime.
fn begin_wake(
    shared: &Arc<ManagerShared>,
    entry: &Arc<ProjectEntry>,
    state: &mut ProjectState,
) -> Option<watch::Receiver<WakeResult>> {
    if let Some(receiver) = &state.wake {
        return Some(receiver.clone());
    }
    if state.cancelled {
        return None;
    }
    let next = if matches!(state.phase, ProjectSessionPhase::Cold) {
        ProjectSessionPhase::Starting
    } else {
        ProjectSessionPhase::Waking
    };
    shared.set_phase(entry, state, next);
    let (sender, receiver) = watch::channel(None);
    state.wake = Some(receiver.clone());
    tokio::spawn(run_wake(Arc::clone(shared), Arc::clone(entry), sender));
    Some(receiver)
}

struct Launched {
    client: Arc<JdtlsClient>,
    pid: Option<u32>,
    tracker: DocumentSyncTracker,
    diagnostics_task: JoinHandle<()>,
    restored: usize,
}

/// Feeds `publishDiagnostics` of a fresh server into the store. Once the server reports
/// readiness (`ServiceReady`: the initial build published every problem), documents restored from
/// the hibernation snapshot that were not republished are dropped and the persisted state is
/// cleared.
async fn forward_diagnostics(
    mut events: broadcast::Receiver<LspEvent>,
    initial: DiagnosticSnapshot,
    already_ready: bool,
    store: Arc<DiagnosticsStore>,
    paths: KovaPaths,
    project_root: PathBuf,
) {
    let mut published: HashSet<String> = HashSet::new();
    for file in initial.files {
        published.insert(file.uri.clone());
        store.merge_file_snapshot(file);
    }
    let mut reconciled = false;
    let reconcile = |published: &HashSet<String>| {
        for uri in store.uris() {
            if !published.contains(&uri) {
                store.clear_uri(&uri);
            }
        }
        let _cleared = HibernationStore::clear_hibernated_state(&paths, &project_root);
    };
    if already_ready {
        reconcile(&published);
        reconciled = true;
    }
    loop {
        match events.recv().await {
            Ok(LspEvent::DiagnosticsReceived {
                uri,
                version,
                diagnostics_json,
            }) => {
                let diagnostics = if diagnostics_json.is_null() {
                    serde_json::json!([])
                } else {
                    diagnostics_json
                };
                if let Ok(params) = serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(
                    serde_json::json!({
                        "uri": uri,
                        "version": version,
                        "diagnostics": diagnostics,
                    }),
                ) {
                    store.apply_publish_diagnostics(params);
                    published.insert(uri);
                }
            }
            Ok(LspEvent::StateChanged {
                current: LspLifecycleState::Ready,
                ..
            }) if !reconciled => {
                reconcile(&published);
                reconciled = true;
            }
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn launch_session(
    shared: &ManagerShared,
    entry: &ProjectEntry,
) -> Result<Launched, LspDapError> {
    let provider = entry.lock().buffer_provider.clone();
    let launched = entry
        .launcher
        .launch(&entry.project_root, &entry.effective_config)
        .await?;
    let client = Arc::new(launched.client);
    let events = client.subscribe_events();
    let initial = client.diagnostics().export_snapshot(&entry.project_root);
    let diagnostics_task = tokio::spawn(forward_diagnostics(
        events,
        initial,
        client.state() == LspLifecycleState::Ready,
        Arc::clone(&entry.diagnostics),
        shared.paths.clone(),
        entry.project_root.clone(),
    ));
    let mut tracker = DocumentSyncTracker::new();
    let buffers = provider.map(|p| p.open_buffers()).unwrap_or_default();
    for buffer in &buffers {
        if let Err(err) = tracker
            .open_document(&client, &buffer.path, &buffer.language_id, &buffer.snapshot)
            .await
        {
            diagnostics_task.abort();
            let _stopped = client.shutdown_graceful(DEFAULT_SHUTDOWN_TIMEOUT).await;
            return Err(err);
        }
    }
    Ok(Launched {
        client,
        pid: launched.pid,
        tracker,
        diagnostics_task,
        restored: buffers.len(),
    })
}

/// Installs a freshly launched server as `Ready`; hands it back when the entry was cancelled.
fn commit_launch(
    shared: &Arc<ManagerShared>,
    entry: &Arc<ProjectEntry>,
    launched: Launched,
) -> Result<Arc<JdtlsClient>, Launched> {
    let mut state = entry.lock();
    state.wake = None;
    if state.cancelled {
        return Err(launched);
    }
    let was_waking = state.phase == ProjectSessionPhase::Waking;
    state.client = Some(Arc::clone(&launched.client));
    state.sync_tracker = Arc::new(tokio::sync::Mutex::new(launched.tracker));
    if let Some(old) = state.diagnostics_task.replace(launched.diagnostics_task) {
        old.abort();
    }
    state.last_activity_at = Instant::now();
    shared.set_phase(
        entry,
        &mut state,
        ProjectSessionPhase::Ready {
            pid: launched.pid,
            last_rss_bytes: None,
        },
    );
    drop(state);
    if was_waking {
        shared.emit(SessionEvent::Woken {
            project_id: entry.id.clone(),
            restored_documents: launched.restored,
        });
    }
    Ok(launched.client)
}

/// Publishes the outcome of a wake exactly once. If the wake task unwinds (panic in the launcher
/// or the buffer provider) or is aborted, dropping it delivers an error to every waiter and marks
/// the session `Failed`, so the next [`JdtlsSessionManager::ensure_ready`] relaunches.
struct WakeCompletion {
    shared: Arc<ManagerShared>,
    entry: Arc<ProjectEntry>,
    sender: watch::Sender<WakeResult>,
    sent: bool,
}

impl WakeCompletion {
    fn finish(mut self, outcome: Result<Arc<JdtlsClient>, LspDapError>) {
        self.sent = true;
        self.sender.send_replace(Some(outcome));
    }
}

impl Drop for WakeCompletion {
    fn drop(&mut self) {
        if self.sent {
            return;
        }
        let reason = "jdt.ls wake task terminated unexpectedly".to_owned();
        {
            let mut state = self.entry.lock();
            state.wake = None;
            if !state.cancelled {
                self.shared.set_phase(
                    &self.entry,
                    &mut state,
                    ProjectSessionPhase::Failed(reason.clone()),
                );
            }
        }
        self.sender.send_replace(Some(Err(LspDapError::SpawnFailed {
            program: "jdt.ls".to_owned(),
            reason,
        })));
    }
}

async fn run_wake(
    shared: Arc<ManagerShared>,
    entry: Arc<ProjectEntry>,
    sender: watch::Sender<WakeResult>,
) {
    let completion = WakeCompletion {
        shared: Arc::clone(&shared),
        entry: Arc::clone(&entry),
        sender,
        sent: false,
    };
    let guard = entry.lifecycle.lock().await;
    let cancelled = entry.lock().cancelled;
    let result = if cancelled {
        Err(LspDapError::SessionShutDown {
            project_root: entry.project_root.display().to_string(),
        })
    } else {
        launch_session(&shared, &entry).await
    };
    let outcome: Result<Arc<JdtlsClient>, LspDapError> = match result {
        Ok(launched) => match commit_launch(&shared, &entry, launched) {
            Ok(client) => Ok(client),
            Err(discarded) => {
                // Unregistered while launching: never leak the new JVM.
                discarded.diagnostics_task.abort();
                let _stopped = discarded
                    .client
                    .shutdown_graceful(shared.config.shutdown_timeout)
                    .await;
                Err(LspDapError::SessionShutDown {
                    project_root: entry.project_root.display().to_string(),
                })
            }
        },
        Err(err) => {
            let mut state = entry.lock();
            state.wake = None;
            if !state.cancelled {
                shared.set_phase(
                    &entry,
                    &mut state,
                    ProjectSessionPhase::Failed(err.to_string()),
                );
            }
            Err(err)
        }
    };
    let succeeded = outcome.is_ok();
    completion.finish(outcome);
    drop(guard);
    if succeeded {
        let _lru = enforce_max_active_jvms(&shared).await;
    }
}

/// Hibernates the least recently used background sessions above `max_active_jvms`.
async fn enforce_max_active_jvms(
    shared: &Arc<ManagerShared>,
) -> Vec<(ProjectId, Result<(), LspDapError>)> {
    let Some(limit) = shared.config.max_active_jvms else {
        return Vec::new();
    };
    let entries = shared.entries();
    let mut active = 0_usize;
    let mut candidates = Vec::new();
    for entry in &entries {
        let state = entry.lock();
        if state.phase.holds_jvm() {
            active += 1;
        }
        if state.phase.is_ready() && !state.focused {
            candidates.push((state.last_activity_at, Arc::clone(entry)));
        }
    }
    candidates.sort_by_key(|(at, _)| *at);
    let mut outcomes = Vec::new();
    for (_, entry) in candidates {
        if active <= limit {
            break;
        }
        let rss = match &entry.lock().phase {
            ProjectSessionPhase::Ready { last_rss_bytes, .. } => last_rss_bytes.unwrap_or(0),
            _ => 0,
        };
        match hibernate_entry(shared, &entry, HibernateReason::Background).await {
            Ok(true) => {
                active -= 1;
                shared.emit(SessionEvent::MemoryLimitAction {
                    project_id: entry.id.clone(),
                    rss_bytes: rss,
                    action_taken: "lru_hibernated".to_owned(),
                });
                outcomes.push((entry.id.clone(), Ok(())));
            }
            Ok(false) => {}
            Err(err) => outcomes.push((entry.id.clone(), Err(err))),
        }
    }
    outcomes
}

/// Re-checks the guards of `reason` under the state lock.
fn may_hibernate(
    config: &SessionManagerConfig,
    state: &ProjectState,
    reason: HibernateReason,
) -> bool {
    match reason {
        HibernateReason::Explicit | HibernateReason::Restart => true,
        HibernateReason::Background => !state.focused,
        HibernateReason::Idle(idle) => {
            (!state.focused || config.hibernate_focused_project)
                && Instant::now().saturating_duration_since(state.last_activity_at) >= idle
        }
    }
}

/// Hibernates `entry` if it is `Ready` and `reason` still holds; returns whether it did.
async fn hibernate_entry(
    shared: &Arc<ManagerShared>,
    entry: &Arc<ProjectEntry>,
    reason: HibernateReason,
) -> Result<bool, LspDapError> {
    // Cheap pre-check: never queue behind a slow launch of a session that is not running.
    if !entry.lock().phase.is_ready() {
        return Ok(false);
    }
    let _guard = entry.lifecycle.lock().await;
    let (client, pid, last_rss, tracker, diagnostics_task, previous) = {
        let mut state = entry.lock();
        let ProjectSessionPhase::Ready {
            pid,
            last_rss_bytes,
        } = state.phase.clone()
        else {
            return Ok(false);
        };
        if state.cancelled || !may_hibernate(&shared.config, &state, reason) {
            return Ok(false);
        }
        let previous = state.phase.clone();
        shared.set_phase(entry, &mut state, ProjectSessionPhase::Hibernating);
        (
            state.client.take(),
            pid,
            last_rss_bytes,
            Arc::clone(&state.sync_tracker),
            state.diagnostics_task.take(),
            previous,
        )
    };

    let rss_freed = match pid {
        Some(pid) => shared
            .probe
            .sample_pid_rss(pid)
            .await
            .ok()
            .flatten()
            .or(last_rss),
        None => last_rss,
    };
    let open_documents: Vec<HibernatedOpenDocument> = tracker
        .lock()
        .await
        .open_documents()
        .map(|(uri, doc)| HibernatedOpenDocument {
            uri: uri.as_str().to_owned(),
            file_path: lsp_uri_to_path(uri).unwrap_or_default(),
            language_id: doc.language_id.clone(),
            version: doc.synced_version.as_u64(),
        })
        .collect();
    let hibernated_at = unix_now_s();
    let diagnostics = entry.diagnostics.export_snapshot(&entry.project_root);
    let diagnostics_saved = diagnostics.diagnostics_count();
    let snapshot = HibernationStateSnapshot {
        schema_version: HIBERNATION_SCHEMA_VERSION,
        project_root: entry.project_root.clone(),
        hibernated_at_unix_s: hibernated_at,
        diagnostics,
        open_documents,
    };
    if let Err(err) = HibernationStore::save_hibernated_state(&shared.paths, &snapshot) {
        let mut state = entry.lock();
        state.client = client;
        state.diagnostics_task = diagnostics_task;
        shared.set_phase(entry, &mut state, previous);
        return Err(err);
    }

    if let Some(task) = diagnostics_task {
        task.abort();
    }
    if let Some(client) = client {
        // The process is killed when the graceful shutdown fails; the error is not actionable.
        let _stopped = client
            .shutdown_graceful(shared.config.shutdown_timeout)
            .await;
    }
    {
        let mut state = entry.lock();
        state.sync_tracker = Arc::new(tokio::sync::Mutex::new(DocumentSyncTracker::new()));
        shared.set_phase(
            entry,
            &mut state,
            ProjectSessionPhase::Hibernated {
                diagnostics_count: diagnostics_saved,
                hibernated_at_unix_s: hibernated_at,
                last_rss_bytes_freed: rss_freed,
            },
        );
    }
    shared.emit(SessionEvent::Hibernated {
        project_id: entry.id.clone(),
        diagnostics_saved,
        rss_freed_bytes: rss_freed,
    });
    Ok(true)
}

/// Drops a crashed client and marks the session `Failed` (the caller then relaunches it).
fn mark_crashed(
    shared: &ManagerShared,
    entry: &ProjectEntry,
    state: &mut ProjectState,
    reason: &str,
) {
    state.client = None;
    if let Some(task) = state.diagnostics_task.take() {
        task.abort();
    }
    state.sync_tracker = Arc::new(tokio::sync::Mutex::new(DocumentSyncTracker::new()));
    shared.set_phase(
        entry,
        state,
        ProjectSessionPhase::Failed(format!("jdt.ls terminated: {reason}")),
    );
}

/// Cancels pending launches and stops the server of a removed entry.
async fn stop_entry(shared: &Arc<ManagerShared>, entry: &Arc<ProjectEntry>) {
    entry.lock().cancelled = true;
    // Waits for an in-flight wake or hibernation; a wake finishing now sees `cancelled`.
    let _guard = entry.lifecycle.lock().await;
    let (client, task) = {
        let mut state = entry.lock();
        (state.client.take(), state.diagnostics_task.take())
    };
    if let Some(task) = task {
        task.abort();
    }
    if let Some(client) = client {
        let _stopped = client
            .shutdown_graceful(shared.config.shutdown_timeout)
            .await;
    }
    let mut state = entry.lock();
    shared.set_phase(entry, &mut state, ProjectSessionPhase::Cold);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_config_follows_effective_settings() {
        let mut effective = EffectiveConfig::merge(&kova_config::KovaSettings::default(), None);
        let defaults = ProjectSessionConfig::from_effective_config(&effective);
        assert_eq!(
            defaults.auto_hibernate_idle,
            Some(DEFAULT_JVM_HIBERNATION_IDLE)
        );
        assert_eq!(defaults, ProjectSessionConfig::default());

        effective.settings.jvm.auto_hibernate_minutes = 0;
        effective.settings.jvm.jdtls_max_ram_mb = 1024;
        let disabled = ProjectSessionConfig::from_effective_config(&effective);
        assert_eq!(disabled.auto_hibernate_idle, None);
        assert_eq!(disabled.memory_policy.configured_xmx_mb, 1024);

        effective.settings.jvm.auto_hibernate_minutes = 3;
        assert_eq!(
            ProjectSessionConfig::from_effective_config(&effective).auto_hibernate_idle,
            Some(Duration::from_secs(180))
        );
    }

    #[test]
    fn phase_predicates() {
        assert!(ProjectSessionPhase::Waking.holds_jvm());
        assert!(!ProjectSessionPhase::Cold.holds_jvm());
        assert!(
            ProjectSessionPhase::Ready {
                pid: None,
                last_rss_bytes: None
            }
            .is_ready()
        );
        assert!(
            ProjectSessionPhase::Hibernated {
                diagnostics_count: 0,
                hibernated_at_unix_s: 0,
                last_rss_bytes_freed: None
            }
            .is_hibernated()
        );
        assert!(!ActivityKind::FocusGained.wakes_session());
        assert!(ActivityKind::EditorEdit.wakes_session());
    }

    #[test]
    fn manager_config_defaults() {
        let config = SessionManagerConfig::default();
        assert!(!config.hibernate_focused_project);
        assert_eq!(config.max_active_jvms, None);
        assert_eq!(config.watchdog_interval, DEFAULT_WATCHDOG_INTERVAL);
    }
}
