//! Asynchronous, debounced live-reload file watcher for Kova configuration
//! dotfiles and Aether themes (`KOVA-008`).
//!
//! Runs disk I/O, SHA-256 deduplication, and TOML parsing on a background
//! `tokio` task so the UI thread only performs lock-free `Arc::clone` reads
//! from a [`tokio::sync::watch::Receiver<LiveConfigState>`].

use std::collections::{BTreeSet, HashMap};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kova_theme::{AetherTheme, ThemeRegistry, ThemeRegistryError};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::document::SettingsDocument;
use crate::model::KovaSettings;
use crate::paths::KovaPaths;
use crate::project::{EffectiveConfig, ProjectConfig};

const SETTINGS_FILE_NAME: &str = "settings.toml";
const KEYMAP_FILE_NAME: &str = "keymap.toml";
const PROJECT_CONFIG_FILE_NAME: &str = "kova.toml";
const THEMES_DIR_NAME: &str = "themes";

/// Errors returned while starting or controlling [`ConfigWatcher`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WatcherError {
    /// Failed to initialize the underlying OS filesystem watcher.
    #[error("failed to initialize filesystem watcher: {message}")]
    NotifyInit {
        /// Underlying watcher backend error description.
        message: String,
    },
    /// Failed to attach or update a directory watch.
    #[error("failed to watch path `{path}`: {message}")]
    WatchPath {
        /// Directory path that could not be watched.
        path: String,
        /// Underlying error description.
        message: String,
    },
    /// Failed to initialize watcher state or Tokio runtime integration.
    #[error("watcher initialization failed: {message}")]
    Init {
        /// Human-readable initialization failure reason.
        message: String,
    },
    /// The background watcher task has already terminated.
    #[error("config watcher background task is no longer running")]
    ChannelClosed,
}

/// Configuration options for [`ConfigWatcher::start`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWatcherOptions {
    /// Debounce window used to coalesce bursts of filesystem events.
    pub debounce: Duration,
    /// When `true`, creates missing user Omarchy directories (`~/.config/kova/`
    /// and `themes/`) at startup; never touches project directories.
    pub create_missing_user_dirs: bool,
    /// Optional project root directory whose `kova.toml` will be watched.
    pub project_root: Option<PathBuf>,
    /// Capacity of the [`broadcast`] channel for [`ConfigReloadEvent`]s.
    pub event_channel_capacity: usize,
}

impl Default for ConfigWatcherOptions {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(75),
            create_missing_user_dirs: false,
            project_root: None,
            event_channel_capacity: 64,
        }
    }
}

/// Identifies which configuration target produced a reload or failure event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadTarget {
    /// User `~/.config/kova/settings.toml`.
    Settings,
    /// A custom Aether theme under `~/.config/kova/themes/<id>.toml`.
    Theme(String),
    /// User `~/.config/kova/keymap.toml`.
    Keymap,
    /// Optional project `<project_root>/kova.toml`.
    ProjectConfig,
}

/// Diagnostic and lifecycle events emitted by [`ConfigWatcher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigReloadEvent {
    /// `settings.toml` was reloaded cleanly and [`LiveConfigState`] was updated.
    SettingsReloaded {
        /// Path to `settings.toml`.
        path: PathBuf,
        /// Monotonically increasing reload generation.
        generation: u64,
    },
    /// A custom theme file was loaded or updated cleanly.
    ThemeReloaded {
        /// Parsed `metadata.id` of the reloaded theme.
        theme_id: String,
        /// Path to the theme `.toml` file.
        path: PathBuf,
        /// Monotonically increasing reload generation.
        generation: u64,
    },
    /// A custom theme file was removed from `themes/`.
    ThemeRemoved {
        /// Identifier of the removed theme.
        theme_id: String,
        /// Path of the removed theme `.toml` file.
        path: PathBuf,
        /// Fallback theme ID if the removed theme was currently active.
        fallback_to: Option<String>,
        /// Monotonically increasing reload generation.
        generation: u64,
    },
    /// `keymap.toml` changed on disk and passed basic TOML syntax validation.
    KeymapChanged {
        /// Path to `keymap.toml`.
        path: PathBuf,
    },
    /// Project `<project_root>/kova.toml` was loaded or updated cleanly.
    ProjectConfigReloaded {
        /// Path to `kova.toml`.
        path: PathBuf,
        /// Monotonically increasing reload generation.
        generation: u64,
    },
    /// Project `<project_root>/kova.toml` was removed or detached.
    ProjectConfigRemoved {
        /// Path to `kova.toml`.
        path: PathBuf,
        /// Monotonically increasing reload generation.
        generation: u64,
    },
    /// A reload failed (e.g., invalid TOML); Last-Known-Good state was preserved.
    ReloadFailed {
        /// Target file category that failed to reload.
        target: ReloadTarget,
        /// Path to the offending file.
        path: PathBuf,
        /// Human-readable diagnostic message.
        message: String,
    },
}

/// Immutable, `O(1)`-cloneable snapshot of the active configuration and theme
/// state consumed by headless services and the GPUI render thread.
#[derive(Debug, Clone)]
pub struct LiveConfigState {
    /// Last-Known-Good validated user settings (`settings.toml`).
    pub settings: Arc<KovaSettings>,
    /// Registry containing built-in presets and loaded custom Aether themes.
    pub theme_registry: Arc<ThemeRegistry>,
    /// Pre-resolved active [`AetherTheme`] for `O(1)` access without lookups.
    pub active_theme: Arc<AetherTheme>,
    /// Optional validated project configuration (`<project_root>/kova.toml`).
    pub project_config: Option<Arc<ProjectConfig>>,
    /// Merged effective configuration (`settings` + optional `project_config`).
    pub effective: Arc<EffectiveConfig>,
    /// Monotonically increasing counter incremented on each effective update.
    pub generation: u64,
}

/// Asynchronous live-reload configuration watcher entry point.
#[derive(Debug, Clone, Copy)]
pub struct ConfigWatcher;

impl ConfigWatcher {
    /// Starts an asynchronous file watcher for `paths` using `options`.
    ///
    /// Performs an initial synchronous scan so [`ConfigWatcherHandle::current_state`]
    /// is immediately populated upon return, then spawns a background `tokio`
    /// task that debounces filesystem changes and updates subscribers.
    ///
    /// # Errors
    ///
    /// Returns [`WatcherError::Init`] if no Tokio runtime is active or if
    /// `create_missing_user_dirs` fails, [`WatcherError::NotifyInit`] if the OS
    /// watcher cannot be constructed, or [`WatcherError::WatchPath`] if a
    /// required directory watch cannot be attached.
    pub fn start(
        paths: KovaPaths,
        options: ConfigWatcherOptions,
    ) -> Result<ConfigWatcherHandle, WatcherError> {
        let ConfigWatcherOptions {
            debounce,
            create_missing_user_dirs,
            project_root,
            event_channel_capacity,
        } = options;

        let rt_handle =
            tokio::runtime::Handle::try_current().map_err(|err| WatcherError::Init {
                message: format!("ConfigWatcher::start requires an active Tokio runtime: {err}"),
            })?;

        if create_missing_user_dirs {
            paths
                .ensure_omarchy_dirs()
                .map_err(|err| WatcherError::Init {
                    message: err.to_string(),
                })?;
        }

        if let Some(ref root) = project_root {
            validate_project_root(root)?;
        }

        let (initial_state, initial_live_state, startup_diagnostics) =
            build_initial_state(paths, project_root);

        let (watch_tx, watch_rx) = watch::channel(initial_live_state);
        let cap = event_channel_capacity.max(1);
        let (event_tx, _) = broadcast::channel(cap);
        let (fs_tx, fs_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();

        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            drop(fs_tx.send(res));
        })
        .map_err(|err| WatcherError::NotifyInit {
            message: err.to_string(),
        })?;

        let watch_tracker = attach_initial_watches(
            &mut watcher,
            &initial_state.paths,
            initial_state.project_root.as_deref(),
        )?;

        let task_handle = rt_handle.spawn(run_watcher_loop(
            watcher,
            watch_tracker,
            initial_state,
            debounce,
            fs_rx,
            control_rx,
            watch_tx.clone(),
            event_tx.clone(),
        ));

        Ok(ConfigWatcherHandle {
            watch_tx,
            _watch_rx: watch_rx,
            event_tx,
            control_tx,
            task_handle: Some(task_handle),
            startup_diagnostics,
        })
    }
}

/// Handle controlling a running [`ConfigWatcher`] task and providing lock-free
/// subscriptions to [`LiveConfigState`] and [`ConfigReloadEvent`].
#[derive(Debug)]
pub struct ConfigWatcherHandle {
    watch_tx: watch::Sender<LiveConfigState>,
    _watch_rx: watch::Receiver<LiveConfigState>,
    event_tx: broadcast::Sender<ConfigReloadEvent>,
    control_tx: mpsc::UnboundedSender<ControlMsg>,
    task_handle: Option<JoinHandle<()>>,
    startup_diagnostics: Vec<ConfigReloadEvent>,
}

impl ConfigWatcherHandle {
    /// Subscribes to live configuration snapshots via [`watch::Receiver`].
    #[must_use]
    pub fn subscribe_state(&self) -> watch::Receiver<LiveConfigState> {
        self.watch_tx.subscribe()
    }

    /// Subscribes to reload and diagnostic events via [`broadcast::Receiver`].
    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<ConfigReloadEvent> {
        self.event_tx.subscribe()
    }

    /// Returns a cheap `O(1)` clone of the current [`LiveConfigState`].
    #[must_use]
    pub fn current_state(&self) -> LiveConfigState {
        self.watch_tx.borrow().clone()
    }

    /// Returns any non-fatal diagnostic events encountered during the initial
    /// synchronous load in [`ConfigWatcher::start`].
    #[must_use]
    pub fn startup_diagnostics(&self) -> &[ConfigReloadEvent] {
        &self.startup_diagnostics
    }

    /// Forces an immediate scan and reload of all watched configuration files,
    /// bypassing the debounce delay while preserving SHA-256 deduplication.
    ///
    /// # Errors
    ///
    /// Returns [`WatcherError::ChannelClosed`] if the background watcher task
    /// has already stopped.
    pub async fn reload_now(&self) -> Result<(), WatcherError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.control_tx
            .send(ControlMsg::ReloadNow(reply_tx))
            .map_err(|_| WatcherError::ChannelClosed)?;
        reply_rx.await.map_err(|_| WatcherError::ChannelClosed)?
    }

    /// Attaches or detaches watching of `<project_root>/kova.toml` without
    /// blocking the calling thread or creating any files inside `project_root`.
    ///
    /// # Errors
    ///
    /// Returns [`WatcherError::WatchPath`] if `project_root` is `Some` and does
    /// not refer to an existing directory, or [`WatcherError::ChannelClosed`] if
    /// the background task has already stopped.
    pub fn watch_project(&self, project_root: Option<PathBuf>) -> Result<(), WatcherError> {
        if let Some(ref root) = project_root {
            validate_project_root(root)?;
        }
        self.control_tx
            .send(ControlMsg::WatchProject(project_root))
            .map_err(|_| WatcherError::ChannelClosed)
    }

    /// Stops the filesystem watcher and waits for the background task to finish.
    pub async fn shutdown(mut self) {
        drop(self.control_tx.send(ControlMsg::Shutdown));
        if let Some(handle) = self.task_handle.take() {
            drop(handle.await);
        }
    }
}

impl Drop for ConfigWatcherHandle {
    fn drop(&mut self) {
        if self.task_handle.is_some() {
            drop(self.control_tx.send(ControlMsg::Shutdown));
        }
    }
}

#[derive(Debug)]
enum ControlMsg {
    ReloadNow(oneshot::Sender<Result<(), WatcherError>>),
    WatchProject(Option<PathBuf>),
    Shutdown,
}

/// Ordered so `Theme` files load before `Settings` when modified in the same
/// debounce window, allowing a newly added theme to be selected immediately.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PendingItem {
    Theme(PathBuf),
    Settings,
    ProjectConfig,
    Keymap,
}

#[derive(Debug)]
struct WatchTracker {
    config_dir: PathBuf,
    canonical_config_dir: Option<PathBuf>,
    themes_dir: PathBuf,
    canonical_themes_dir: Option<PathBuf>,
    project_root: Option<PathBuf>,
    canonical_project_root: Option<PathBuf>,
    watching_config_dir: bool,
    watching_config_parent: Option<PathBuf>,
    watching_themes_dir: bool,
    watching_project_root: Option<PathBuf>,
}

#[derive(Debug)]
struct WatcherState {
    paths: KovaPaths,
    project_root: Option<PathBuf>,
    last_seen_hashes: HashMap<PathBuf, [u8; 32]>,
    theme_file_ids: HashMap<PathBuf, String>,
    settings: Arc<KovaSettings>,
    theme_registry: ThemeRegistry,
    project_config: Option<Arc<ProjectConfig>>,
    effective: Arc<EffectiveConfig>,
    generation: u64,
}

impl WatcherState {
    fn snapshot(&self) -> LiveConfigState {
        LiveConfigState {
            settings: Arc::clone(&self.settings),
            theme_registry: Arc::new(self.theme_registry.clone()),
            active_theme: self.theme_registry.active_arc(),
            project_config: self.project_config.clone(),
            effective: Arc::clone(&self.effective),
            generation: self.generation,
        }
    }

    fn set_project_config(&mut self, next: Option<ProjectConfig>) {
        self.project_config = next.map(Arc::new);
        self.effective = Arc::new(EffectiveConfig::merge(
            &self.settings,
            self.project_config.as_deref(),
        ));
    }
}

fn publish_state(watch_tx: &watch::Sender<LiveConfigState>, state: &mut WatcherState) {
    state.generation += 1;
    drop(watch_tx.send_replace(state.snapshot()));
}

fn emit_event(event_tx: &broadcast::Sender<ConfigReloadEvent>, event: ConfigReloadEvent) {
    drop(event_tx.send(event));
}

fn validate_project_root(project_root: &Path) -> Result<(), WatcherError> {
    if !project_root.is_dir() {
        return Err(WatcherError::WatchPath {
            path: project_root.to_string_lossy().into_owned(),
            message: "project root is not an existing directory".to_owned(),
        });
    }
    Ok(())
}

fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn theme_target_name(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map_or_else(|| path.to_string_lossy().into_owned(), str::to_owned)
}

fn is_valid_theme_filename(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with('~')
        && Path::new(name)
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext == "toml")
}

fn list_theme_files_sorted(themes_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(themes_dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name_str) = file_name.to_str() else {
            continue;
        };
        if is_valid_theme_filename(name_str) {
            let candidate = themes_dir.join(name_str);
            if candidate.is_file() {
                files.push(candidate);
            }
        }
    }
    files.sort();
    files
}

fn build_initial_state(
    paths: KovaPaths,
    project_root: Option<PathBuf>,
) -> (WatcherState, LiveConfigState, Vec<ConfigReloadEvent>) {
    let mut last_seen_hashes = HashMap::new();
    let mut theme_file_ids = HashMap::new();
    let mut theme_registry = ThemeRegistry::new();
    let mut diagnostics = Vec::new();

    load_initial_themes(
        &paths.themes_dir(),
        &mut last_seen_hashes,
        &mut theme_file_ids,
        &mut theme_registry,
        &mut diagnostics,
    );
    let settings = load_initial_settings(
        &paths.settings_file(),
        &mut last_seen_hashes,
        &mut theme_registry,
        &mut diagnostics,
    );
    record_initial_keymap(
        &paths.keymap_file(),
        &mut last_seen_hashes,
        &mut diagnostics,
    );
    let project_config = project_root.as_deref().and_then(|root| {
        load_initial_project_config(
            &KovaPaths::project_config_file(root),
            &mut last_seen_hashes,
            &mut diagnostics,
        )
    });

    let settings = Arc::new(settings);
    let effective = Arc::new(EffectiveConfig::merge(&settings, project_config.as_deref()));

    let state = WatcherState {
        paths,
        project_root,
        last_seen_hashes,
        theme_file_ids,
        settings,
        theme_registry,
        project_config,
        effective,
        generation: 0,
    };
    let live_state = state.snapshot();
    (state, live_state, diagnostics)
}

fn load_initial_themes(
    themes_dir: &Path,
    last_seen_hashes: &mut HashMap<PathBuf, [u8; 32]>,
    theme_file_ids: &mut HashMap<PathBuf, String>,
    theme_registry: &mut ThemeRegistry,
    diagnostics: &mut Vec<ConfigReloadEvent>,
) {
    if !themes_dir.is_dir() {
        return;
    }
    for theme_path in list_theme_files_sorted(themes_dir) {
        match std::fs::read(&theme_path) {
            Ok(bytes) => {
                last_seen_hashes.insert(theme_path.clone(), sha256_bytes(&bytes));
                match parse_theme_bytes(&bytes) {
                    Ok(theme) => {
                        let id = theme_registry.register(theme);
                        theme_file_ids.insert(theme_path, id);
                    }
                    Err(message) => diagnostics.push(ConfigReloadEvent::ReloadFailed {
                        target: ReloadTarget::Theme(theme_target_name(&theme_path)),
                        path: theme_path,
                        message,
                    }),
                }
            }
            Err(err) => diagnostics.push(ConfigReloadEvent::ReloadFailed {
                target: ReloadTarget::Theme(theme_target_name(&theme_path)),
                path: theme_path,
                message: err.to_string(),
            }),
        }
    }
}

fn load_initial_settings(
    settings_path: &Path,
    last_seen_hashes: &mut HashMap<PathBuf, [u8; 32]>,
    theme_registry: &mut ThemeRegistry,
    diagnostics: &mut Vec<ConfigReloadEvent>,
) -> KovaSettings {
    match std::fs::read(settings_path) {
        Ok(bytes) => {
            last_seen_hashes.insert(settings_path.to_path_buf(), sha256_bytes(&bytes));
            match parse_settings_bytes(&bytes) {
                Ok(parsed) => {
                    if let Err(err) = theme_registry.set_active(&parsed.ui.theme) {
                        diagnostics.push(ConfigReloadEvent::ReloadFailed {
                            target: ReloadTarget::Settings,
                            path: settings_path.to_path_buf(),
                            message: err.to_string(),
                        });
                    }
                    parsed
                }
                Err(message) => {
                    diagnostics.push(ConfigReloadEvent::ReloadFailed {
                        target: ReloadTarget::Settings,
                        path: settings_path.to_path_buf(),
                        message,
                    });
                    KovaSettings::default()
                }
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => KovaSettings::default(),
        Err(err) => {
            diagnostics.push(ConfigReloadEvent::ReloadFailed {
                target: ReloadTarget::Settings,
                path: settings_path.to_path_buf(),
                message: err.to_string(),
            });
            KovaSettings::default()
        }
    }
}

fn record_initial_keymap(
    keymap_path: &Path,
    last_seen_hashes: &mut HashMap<PathBuf, [u8; 32]>,
    diagnostics: &mut Vec<ConfigReloadEvent>,
) {
    if let Ok(bytes) = std::fs::read(keymap_path) {
        last_seen_hashes.insert(keymap_path.to_path_buf(), sha256_bytes(&bytes));
        if let Err(message) = validate_keymap_bytes(&bytes) {
            diagnostics.push(ConfigReloadEvent::ReloadFailed {
                target: ReloadTarget::Keymap,
                path: keymap_path.to_path_buf(),
                message,
            });
        }
    }
}

fn load_initial_project_config(
    proj_path: &Path,
    last_seen_hashes: &mut HashMap<PathBuf, [u8; 32]>,
    diagnostics: &mut Vec<ConfigReloadEvent>,
) -> Option<Arc<ProjectConfig>> {
    match std::fs::read(proj_path) {
        Ok(bytes) => {
            last_seen_hashes.insert(proj_path.to_path_buf(), sha256_bytes(&bytes));
            match parse_project_bytes(&bytes) {
                Ok(cfg) => Some(Arc::new(cfg)),
                Err(message) => {
                    diagnostics.push(ConfigReloadEvent::ReloadFailed {
                        target: ReloadTarget::ProjectConfig,
                        path: proj_path.to_path_buf(),
                        message,
                    });
                    None
                }
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => {
            diagnostics.push(ConfigReloadEvent::ReloadFailed {
                target: ReloadTarget::ProjectConfig,
                path: proj_path.to_path_buf(),
                message: err.to_string(),
            });
            None
        }
    }
}

fn attach_initial_watches(
    watcher: &mut RecommendedWatcher,
    paths: &KovaPaths,
    project_root: Option<&Path>,
) -> Result<WatchTracker, WatcherError> {
    let config_dir = paths.config_dir().to_path_buf();
    let themes_dir = paths.themes_dir();

    let mut tracker = WatchTracker {
        config_dir: config_dir.clone(),
        canonical_config_dir: std::fs::canonicalize(&config_dir).ok(),
        themes_dir: themes_dir.clone(),
        canonical_themes_dir: std::fs::canonicalize(&themes_dir).ok(),
        project_root: project_root.map(Path::to_path_buf),
        canonical_project_root: project_root.and_then(|p| std::fs::canonicalize(p).ok()),
        watching_config_dir: false,
        watching_config_parent: None,
        watching_themes_dir: false,
        watching_project_root: None,
    };

    if config_dir.is_dir() {
        watch_non_recursive(watcher, &config_dir)?;
        tracker.watching_config_dir = true;
    } else if let Some(parent) = config_dir.parent()
        && parent.is_dir()
    {
        watch_non_recursive(watcher, parent)?;
        tracker.watching_config_parent = Some(parent.to_path_buf());
    }

    if themes_dir.is_dir() {
        watch_non_recursive(watcher, &themes_dir)?;
        tracker.watching_themes_dir = true;
    }

    if let Some(root) = project_root {
        watch_non_recursive(watcher, root)?;
        tracker.watching_project_root = Some(root.to_path_buf());
    }

    Ok(tracker)
}

fn watch_non_recursive(watcher: &mut RecommendedWatcher, path: &Path) -> Result<(), WatcherError> {
    watcher
        .watch(path, RecursiveMode::NonRecursive)
        .map_err(|err| WatcherError::WatchPath {
            path: path.to_string_lossy().into_owned(),
            message: err.to_string(),
        })
}

fn unwatch_if_unused(watcher: &mut RecommendedWatcher, tracker: &WatchTracker, path: &Path) {
    let still_used = (tracker.watching_config_dir
        && paths_match(
            &tracker.config_dir,
            tracker.canonical_config_dir.as_deref(),
            path,
        ))
        || (tracker.watching_themes_dir
            && paths_match(
                &tracker.themes_dir,
                tracker.canonical_themes_dir.as_deref(),
                path,
            ))
        || tracker
            .watching_config_parent
            .as_deref()
            .is_some_and(|p| p == path)
        || tracker
            .watching_project_root
            .as_deref()
            .is_some_and(|p| paths_match(p, tracker.canonical_project_root.as_deref(), path));

    if !still_used {
        drop(watcher.unwatch(path));
    }
}

fn paths_match(raw_dir: &Path, canonical_dir: Option<&Path>, candidate: &Path) -> bool {
    if candidate == raw_dir {
        return true;
    }
    if let Some(canon) = canonical_dir {
        if candidate == canon {
            return true;
        }
        if let Ok(candidate_canon) = std::fs::canonicalize(candidate) {
            return candidate_canon == canon;
        }
    }
    false
}

#[allow(
    clippy::too_many_arguments,
    reason = "single internal background actor loop coordinates watcher channels and state"
)]
async fn run_watcher_loop(
    mut watcher: RecommendedWatcher,
    mut tracker: WatchTracker,
    mut state: WatcherState,
    debounce: Duration,
    mut fs_rx: mpsc::UnboundedReceiver<Result<Event, notify::Error>>,
    mut control_rx: mpsc::UnboundedReceiver<ControlMsg>,
    watch_tx: watch::Sender<LiveConfigState>,
    event_tx: broadcast::Sender<ConfigReloadEvent>,
) {
    let mut pending: BTreeSet<PendingItem> = BTreeSet::new();
    let mut debounce_active = false;
    let sleep = tokio::time::sleep(Duration::MAX);
    tokio::pin!(sleep);

    loop {
        tokio::select! {
            biased;
            maybe_ctrl = control_rx.recv() => {
                match maybe_ctrl {
                    Some(ControlMsg::Shutdown) | None => {
                        break;
                    }
                    Some(ControlMsg::ReloadNow(reply_tx)) => {
                        debounce_active = false;
                        let mut batch = std::mem::take(&mut pending);
                        ensure_dynamic_watches(&mut watcher, &mut tracker, &mut batch);
                        collect_all_targets(&state, &mut batch);
                        process_batch(&mut state, batch, &watch_tx, &event_tx).await;
                        drop(reply_tx.send(Ok(())));
                    }
                    Some(ControlMsg::WatchProject(new_root)) => {
                        handle_watch_project(
                            &mut watcher,
                            &mut tracker,
                            &mut state,
                            new_root,
                            &mut pending,
                            &watch_tx,
                            &event_tx,
                        )
                        .await;
                    }
                }
            }
            maybe_evt = fs_rx.recv() => {
                match maybe_evt {
                    Some(Ok(event)) => {
                        let before_len = pending.len();
                        let had_items = !pending.is_empty();
                        classify_notify_event(&mut watcher, &mut tracker, &event, &mut pending);
                        if pending.len() > before_len || (had_items && !pending.is_empty() && event_touches_watched(&tracker, &event)) {
                            debounce_active = true;
                            sleep.as_mut().reset(Instant::now() + debounce);
                        }
                    }
                    Some(Err(_)) => {}
                    None => break,
                }
            }
            () = &mut sleep, if debounce_active => {
                debounce_active = false;
                let batch = std::mem::take(&mut pending);
                process_batch(&mut state, batch, &watch_tx, &event_tx).await;
            }
        }
    }

    drop(watcher);
}

fn event_touches_watched(tracker: &WatchTracker, event: &Event) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    event
        .paths
        .iter()
        .any(|path| classify_path(tracker, path).is_some())
}

fn classify_path(tracker: &WatchTracker, path: &Path) -> Option<PendingItem> {
    let file_name = path.file_name()?.to_str()?;
    let parent = path.parent()?;

    if paths_match(
        &tracker.config_dir,
        tracker.canonical_config_dir.as_deref(),
        parent,
    ) {
        return match file_name {
            SETTINGS_FILE_NAME => Some(PendingItem::Settings),
            KEYMAP_FILE_NAME => Some(PendingItem::Keymap),
            _ => None,
        };
    }

    if paths_match(
        &tracker.themes_dir,
        tracker.canonical_themes_dir.as_deref(),
        parent,
    ) {
        if is_valid_theme_filename(file_name) {
            return Some(PendingItem::Theme(tracker.themes_dir.join(file_name)));
        }
        return None;
    }

    if let Some(ref project_root) = tracker.project_root
        && paths_match(
            project_root,
            tracker.canonical_project_root.as_deref(),
            parent,
        )
        && file_name == PROJECT_CONFIG_FILE_NAME
    {
        return Some(PendingItem::ProjectConfig);
    }

    None
}

fn ensure_dynamic_watches(
    watcher: &mut RecommendedWatcher,
    tracker: &mut WatchTracker,
    pending: &mut BTreeSet<PendingItem>,
) {
    if !tracker.watching_config_dir
        && tracker.config_dir.is_dir()
        && watch_non_recursive(watcher, &tracker.config_dir).is_ok()
    {
        tracker.watching_config_dir = true;
        tracker.canonical_config_dir = std::fs::canonicalize(&tracker.config_dir).ok();
        if let Some(prev_parent) = tracker.watching_config_parent.take() {
            unwatch_if_unused(watcher, tracker, &prev_parent);
        }
        pending.insert(PendingItem::Settings);
        pending.insert(PendingItem::Keymap);
    }

    if !tracker.watching_themes_dir
        && tracker.themes_dir.is_dir()
        && watch_non_recursive(watcher, &tracker.themes_dir).is_ok()
    {
        tracker.watching_themes_dir = true;
        tracker.canonical_themes_dir = std::fs::canonicalize(&tracker.themes_dir).ok();
        for theme_path in list_theme_files_sorted(&tracker.themes_dir) {
            pending.insert(PendingItem::Theme(theme_path));
        }
    }
}

fn classify_notify_event(
    watcher: &mut RecommendedWatcher,
    tracker: &mut WatchTracker,
    event: &Event,
    pending: &mut BTreeSet<PendingItem>,
) {
    if matches!(event.kind, EventKind::Access(_)) {
        return;
    }

    ensure_dynamic_watches(watcher, tracker, pending);

    for path in &event.paths {
        if let Some(item) = classify_path(tracker, path) {
            pending.insert(item);
            continue;
        }

        if let Some(file_name) = path.file_name().and_then(|s| s.to_str())
            && file_name == THEMES_DIR_NAME
            && path.parent().is_some_and(|parent| {
                paths_match(
                    &tracker.config_dir,
                    tracker.canonical_config_dir.as_deref(),
                    parent,
                )
            })
        {
            ensure_dynamic_watches(watcher, tracker, pending);
        }
    }
}

fn collect_all_targets(state: &WatcherState, batch: &mut BTreeSet<PendingItem>) {
    batch.insert(PendingItem::Settings);
    batch.insert(PendingItem::Keymap);
    if state.project_root.is_some() {
        batch.insert(PendingItem::ProjectConfig);
    }

    let themes_dir = state.paths.themes_dir();
    if themes_dir.is_dir() {
        for theme_path in list_theme_files_sorted(&themes_dir) {
            batch.insert(PendingItem::Theme(theme_path));
        }
    }
    for known_theme_path in state.theme_file_ids.keys() {
        batch.insert(PendingItem::Theme(known_theme_path.clone()));
    }
}

async fn handle_watch_project(
    watcher: &mut RecommendedWatcher,
    tracker: &mut WatchTracker,
    state: &mut WatcherState,
    new_root: Option<PathBuf>,
    pending: &mut BTreeSet<PendingItem>,
    watch_tx: &watch::Sender<LiveConfigState>,
    event_tx: &broadcast::Sender<ConfigReloadEvent>,
) {
    let old_watched = tracker.watching_project_root.take();
    let old_root = tracker.project_root.take();
    tracker.canonical_project_root = None;
    pending.remove(&PendingItem::ProjectConfig);

    if let Some(old_path) = old_watched {
        unwatch_if_unused(watcher, tracker, &old_path);
    }
    if let Some(ref prev_root) = old_root {
        state
            .last_seen_hashes
            .remove(&KovaPaths::project_config_file(prev_root));
    }

    let Some(root) = new_root else {
        state.project_root = None;
        if state.project_config.is_some() {
            state.set_project_config(None);
            publish_state(watch_tx, state);
            let removed_path = old_root.as_deref().map_or_else(
                || PathBuf::from(PROJECT_CONFIG_FILE_NAME),
                KovaPaths::project_config_file,
            );
            emit_event(
                event_tx,
                ConfigReloadEvent::ProjectConfigRemoved {
                    path: removed_path,
                    generation: state.generation,
                },
            );
        }
        return;
    };

    if let Err(err) = watch_non_recursive(watcher, &root) {
        state.project_root = None;
        emit_event(
            event_tx,
            ConfigReloadEvent::ReloadFailed {
                target: ReloadTarget::ProjectConfig,
                path: KovaPaths::project_config_file(&root),
                message: err.to_string(),
            },
        );
        return;
    }

    tracker.project_root = Some(root.clone());
    tracker.canonical_project_root = std::fs::canonicalize(&root).ok();
    tracker.watching_project_root = Some(root.clone());
    state.project_root = Some(root.clone());

    load_attached_project_config(
        state,
        &KovaPaths::project_config_file(&root),
        watch_tx,
        event_tx,
    )
    .await;
}

async fn load_attached_project_config(
    state: &mut WatcherState,
    proj_path: &Path,
    watch_tx: &watch::Sender<LiveConfigState>,
    event_tx: &broadcast::Sender<ConfigReloadEvent>,
) {
    match tokio::fs::read(proj_path).await {
        Ok(bytes) => {
            state
                .last_seen_hashes
                .insert(proj_path.to_path_buf(), sha256_bytes(&bytes));
            match parse_project_bytes(&bytes) {
                Ok(cfg) => {
                    state.set_project_config(Some(cfg));
                    publish_state(watch_tx, state);
                    emit_event(
                        event_tx,
                        ConfigReloadEvent::ProjectConfigReloaded {
                            path: proj_path.to_path_buf(),
                            generation: state.generation,
                        },
                    );
                }
                Err(message) => {
                    clear_project_config_on_error(state, watch_tx);
                    emit_event(
                        event_tx,
                        ConfigReloadEvent::ReloadFailed {
                            target: ReloadTarget::ProjectConfig,
                            path: proj_path.to_path_buf(),
                            message,
                        },
                    );
                }
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if state.project_config.is_some() {
                state.set_project_config(None);
                publish_state(watch_tx, state);
                emit_event(
                    event_tx,
                    ConfigReloadEvent::ProjectConfigRemoved {
                        path: proj_path.to_path_buf(),
                        generation: state.generation,
                    },
                );
            }
        }
        Err(err) => {
            clear_project_config_on_error(state, watch_tx);
            emit_event(
                event_tx,
                ConfigReloadEvent::ReloadFailed {
                    target: ReloadTarget::ProjectConfig,
                    path: proj_path.to_path_buf(),
                    message: err.to_string(),
                },
            );
        }
    }
}

fn clear_project_config_on_error(
    state: &mut WatcherState,
    watch_tx: &watch::Sender<LiveConfigState>,
) {
    if state.project_config.is_some() {
        state.set_project_config(None);
        publish_state(watch_tx, state);
    }
}

async fn process_batch(
    state: &mut WatcherState,
    batch: BTreeSet<PendingItem>,
    watch_tx: &watch::Sender<LiveConfigState>,
    event_tx: &broadcast::Sender<ConfigReloadEvent>,
) {
    for item in batch {
        match item {
            PendingItem::Theme(path) => {
                process_theme_change(state, path, watch_tx, event_tx).await;
            }
            PendingItem::Settings => {
                process_settings_change(state, watch_tx, event_tx).await;
            }
            PendingItem::ProjectConfig => {
                process_project_change(state, watch_tx, event_tx).await;
            }
            PendingItem::Keymap => {
                process_keymap_change(state, event_tx).await;
            }
        }
    }
}

async fn process_theme_change(
    state: &mut WatcherState,
    path: PathBuf,
    watch_tx: &watch::Sender<LiveConfigState>,
    event_tx: &broadcast::Sender<ConfigReloadEvent>,
) {
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let hash = sha256_bytes(&bytes);
            if state.last_seen_hashes.get(&path) == Some(&hash) {
                return;
            }
            state.last_seen_hashes.insert(path.clone(), hash);

            match parse_theme_bytes(&bytes) {
                Ok(theme) => {
                    let theme_id = theme.metadata.id.clone();
                    if let Some(prev_id) =
                        state.theme_file_ids.insert(path.clone(), theme_id.clone())
                        && prev_id != theme_id
                        && !state.theme_registry.restore_builtin(&prev_id)
                    {
                        let _ = state.theme_registry.unregister(&prev_id);
                    }

                    state.theme_registry.register(theme);
                    if state.settings.ui.theme == theme_id {
                        drop(state.theme_registry.set_active(&theme_id));
                    }

                    publish_state(watch_tx, state);
                    emit_event(
                        event_tx,
                        ConfigReloadEvent::ThemeReloaded {
                            theme_id,
                            path,
                            generation: state.generation,
                        },
                    );
                }
                Err(message) => {
                    let target_id = state
                        .theme_file_ids
                        .get(&path)
                        .cloned()
                        .unwrap_or_else(|| theme_target_name(&path));
                    emit_event(
                        event_tx,
                        ConfigReloadEvent::ReloadFailed {
                            target: ReloadTarget::Theme(target_id),
                            path,
                            message,
                        },
                    );
                }
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            state.last_seen_hashes.remove(&path);
            let Some(theme_id) = state.theme_file_ids.remove(&path) else {
                return;
            };

            let fallback_to = if state.theme_registry.restore_builtin(&theme_id) {
                None
            } else {
                state.theme_registry.unregister_custom(&theme_id)
            };

            publish_state(watch_tx, state);
            emit_event(
                event_tx,
                ConfigReloadEvent::ThemeRemoved {
                    theme_id,
                    path,
                    fallback_to,
                    generation: state.generation,
                },
            );
        }
        Err(err) => {
            emit_event(
                event_tx,
                ConfigReloadEvent::ReloadFailed {
                    target: ReloadTarget::Theme(theme_target_name(&path)),
                    path,
                    message: err.to_string(),
                },
            );
        }
    }
}

async fn process_settings_change(
    state: &mut WatcherState,
    watch_tx: &watch::Sender<LiveConfigState>,
    event_tx: &broadcast::Sender<ConfigReloadEvent>,
) {
    let path = state.paths.settings_file();
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let hash = sha256_bytes(&bytes);
            if state.last_seen_hashes.get(&path) == Some(&hash) {
                return;
            }
            state.last_seen_hashes.insert(path.clone(), hash);

            let next_settings = match parse_settings_bytes(&bytes) {
                Ok(parsed) => parsed,
                Err(message) => {
                    emit_event(
                        event_tx,
                        ConfigReloadEvent::ReloadFailed {
                            target: ReloadTarget::Settings,
                            path,
                            message,
                        },
                    );
                    return;
                }
            };

            match state.theme_registry.set_active(&next_settings.ui.theme) {
                Ok(_) => {
                    state.settings = Arc::new(next_settings);
                    state.effective = Arc::new(EffectiveConfig::merge(
                        &state.settings,
                        state.project_config.as_deref(),
                    ));
                    publish_state(watch_tx, state);
                    emit_event(
                        event_tx,
                        ConfigReloadEvent::SettingsReloaded {
                            path,
                            generation: state.generation,
                        },
                    );
                }
                Err(ThemeRegistryError::UnknownTheme { id }) => {
                    emit_event(
                        event_tx,
                        ConfigReloadEvent::ReloadFailed {
                            target: ReloadTarget::Settings,
                            path,
                            message: format!(
                                "unknown theme `{id}`; preserving last-known-good active theme `{}`",
                                state.theme_registry.active_id()
                            ),
                        },
                    );
                }
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if state.last_seen_hashes.remove(&path).is_some() {
                let default_settings = KovaSettings::default();
                drop(state.theme_registry.set_active(&default_settings.ui.theme));
                state.settings = Arc::new(default_settings);
                state.effective = Arc::new(EffectiveConfig::merge(
                    &state.settings,
                    state.project_config.as_deref(),
                ));
                publish_state(watch_tx, state);
                emit_event(
                    event_tx,
                    ConfigReloadEvent::SettingsReloaded {
                        path,
                        generation: state.generation,
                    },
                );
            }
        }
        Err(err) => {
            emit_event(
                event_tx,
                ConfigReloadEvent::ReloadFailed {
                    target: ReloadTarget::Settings,
                    path,
                    message: err.to_string(),
                },
            );
        }
    }
}

async fn process_keymap_change(
    state: &mut WatcherState,
    event_tx: &broadcast::Sender<ConfigReloadEvent>,
) {
    let path = state.paths.keymap_file();
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let hash = sha256_bytes(&bytes);
            if state.last_seen_hashes.get(&path) == Some(&hash) {
                return;
            }
            state.last_seen_hashes.insert(path.clone(), hash);

            if let Err(message) = validate_keymap_bytes(&bytes) {
                emit_event(
                    event_tx,
                    ConfigReloadEvent::ReloadFailed {
                        target: ReloadTarget::Keymap,
                        path,
                        message,
                    },
                );
            } else {
                emit_event(event_tx, ConfigReloadEvent::KeymapChanged { path });
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if state.last_seen_hashes.remove(&path).is_some() {
                emit_event(event_tx, ConfigReloadEvent::KeymapChanged { path });
            }
        }
        Err(err) => {
            emit_event(
                event_tx,
                ConfigReloadEvent::ReloadFailed {
                    target: ReloadTarget::Keymap,
                    path,
                    message: err.to_string(),
                },
            );
        }
    }
}

async fn process_project_change(
    state: &mut WatcherState,
    watch_tx: &watch::Sender<LiveConfigState>,
    event_tx: &broadcast::Sender<ConfigReloadEvent>,
) {
    let Some(ref project_root) = state.project_root else {
        return;
    };
    let path = KovaPaths::project_config_file(project_root);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let hash = sha256_bytes(&bytes);
            if state.last_seen_hashes.get(&path) == Some(&hash) {
                return;
            }
            state.last_seen_hashes.insert(path.clone(), hash);

            match parse_project_bytes(&bytes) {
                Ok(cfg) => {
                    state.set_project_config(Some(cfg));
                    publish_state(watch_tx, state);
                    emit_event(
                        event_tx,
                        ConfigReloadEvent::ProjectConfigReloaded {
                            path,
                            generation: state.generation,
                        },
                    );
                }
                Err(message) => {
                    emit_event(
                        event_tx,
                        ConfigReloadEvent::ReloadFailed {
                            target: ReloadTarget::ProjectConfig,
                            path,
                            message,
                        },
                    );
                }
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            state.last_seen_hashes.remove(&path);
            if state.project_config.is_some() {
                state.set_project_config(None);
                publish_state(watch_tx, state);
                emit_event(
                    event_tx,
                    ConfigReloadEvent::ProjectConfigRemoved {
                        path,
                        generation: state.generation,
                    },
                );
            }
        }
        Err(err) => {
            emit_event(
                event_tx,
                ConfigReloadEvent::ReloadFailed {
                    target: ReloadTarget::ProjectConfig,
                    path,
                    message: err.to_string(),
                },
            );
        }
    }
}

fn parse_settings_bytes(bytes: &[u8]) -> Result<KovaSettings, String> {
    let text = std::str::from_utf8(bytes).map_err(|err| format!("invalid UTF-8: {err}"))?;
    SettingsDocument::from_toml_str(text)
        .map(|doc| doc.settings().clone())
        .map_err(|err| err.to_string())
}

fn parse_theme_bytes(bytes: &[u8]) -> Result<AetherTheme, String> {
    let text = std::str::from_utf8(bytes).map_err(|err| format!("invalid UTF-8: {err}"))?;
    AetherTheme::from_toml_str(text).map_err(|err| err.to_string())
}

fn parse_project_bytes(bytes: &[u8]) -> Result<ProjectConfig, String> {
    let text = std::str::from_utf8(bytes).map_err(|err| format!("invalid UTF-8: {err}"))?;
    ProjectConfig::from_toml_str(text).map_err(|err| err.to_string())
}

fn validate_keymap_bytes(bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|err| format!("invalid UTF-8: {err}"))?;
    text.parse::<toml_edit::DocumentMut>()
        .map(|_| ())
        .map_err(|err| format!("failed to parse keymap TOML: {err}"))
}
