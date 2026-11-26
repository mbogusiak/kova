//! Plugin discovery, lifecycle management, trap isolation, and hot-reload (`PluginManager`, FR-4).

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use kova_config::KovaPaths;
use wasmtime::component::Component;
use wasmtime::{Store, Trap};

use crate::bindings::{
    CommandDescriptor, KovaPlugin, PluginEvent, ToolWindowDescriptor, UiEvent, WidgetTree,
    validate_widget_tree,
};
use crate::error::PluginHostError;
use crate::host_impl::{EditorContextBridge, PluginLogEntry, PluginStoreState};
use crate::manifest::PluginManifest;
use crate::sandbox::{
    PluginEngine, PluginResourceLimiter, ResolvedPluginLimits, SandboxPolicy, build_wasi_ctx,
};

/// Operational status of a managed plugin instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginStatus {
    /// Plugin is loaded, initialized, and ready to process events and UI calls.
    Active,
    /// Plugin is disabled by user/host; its WASM store is unloaded.
    Disabled,
    /// Plugin failed validation, trapped, or exceeded time/memory bounds.
    Faulted {
        /// Human-readable reason explaining why the plugin entered `Faulted`.
        reason: String,
    },
}

/// Snapshot metadata and registrations for a plugin known to [`PluginManager`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSummary {
    /// Parsed `plugin.toml` manifest.
    pub manifest: PluginManifest,
    /// Current lifecycle state.
    pub status: PluginStatus,
    /// Directory from which the plugin was loaded (if disk-backed).
    pub dir: PathBuf,
    /// Commands registered during `init()` or runtime.
    pub commands: Vec<CommandDescriptor>,
    /// Tool Windows registered during `init()` or runtime.
    pub tool_windows: Vec<ToolWindowDescriptor>,
    /// Peak linear memory observed in bytes (0 when unloaded).
    pub peak_memory_bytes: usize,
}

/// Result entry returned when discovering and loading plugins from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginLoadOutcome {
    /// Plugin directory that was inspected.
    pub dir: PathBuf,
    /// Plugin identifier if manifest parsing succeeded.
    pub plugin_id: Option<String>,
    /// Resulting status or error.
    pub result: Result<PluginStatus, PluginHostError>,
}

enum PluginSource {
    Disk,
    InMemory { wasm_bytes: Vec<u8> },
}

struct ActiveRuntime {
    store: Store<PluginStoreState>,
    bindings: KovaPlugin,
    limits: ResolvedPluginLimits,
}

struct ManagedPlugin {
    manifest: PluginManifest,
    dir: PathBuf,
    source: PluginSource,
    status: PluginStatus,
    runtime: Option<ActiveRuntime>,
    last_commands: Vec<CommandDescriptor>,
    last_tool_windows: Vec<ToolWindowDescriptor>,
    last_logs: Vec<PluginLogEntry>,
    last_peak_memory_bytes: usize,
}

impl ManagedPlugin {
    fn sync_cached_metadata(&mut self) {
        if let Some(rt) = &self.runtime {
            let state = rt.store.data();
            self.last_commands = state.registered_commands().to_vec();
            self.last_tool_windows = state.registered_tool_windows().to_vec();
            self.last_logs = state.logs();
            self.last_peak_memory_bytes = state.peak_memory_bytes();
        }
    }

    fn summary(&self) -> PluginSummary {
        if let Some(rt) = &self.runtime {
            let state = rt.store.data();
            PluginSummary {
                manifest: self.manifest.clone(),
                status: self.status.clone(),
                dir: self.dir.clone(),
                commands: state.registered_commands().to_vec(),
                tool_windows: state.registered_tool_windows().to_vec(),
                peak_memory_bytes: state.peak_memory_bytes(),
            }
        } else {
            PluginSummary {
                manifest: self.manifest.clone(),
                status: self.status.clone(),
                dir: self.dir.clone(),
                commands: self.last_commands.clone(),
                tool_windows: self.last_tool_windows.clone(),
                peak_memory_bytes: self.last_peak_memory_bytes,
            }
        }
    }
}

/// Manages discovery, sandboxed execution, hot-reload, and fault isolation of WASM plugins.
pub struct PluginManager {
    plugins_dir: PathBuf,
    workspace_root: Option<PathBuf>,
    policy: SandboxPolicy,
    editor_bridge: Arc<dyn EditorContextBridge>,
    engine: PluginEngine,
    plugins: BTreeMap<String, ManagedPlugin>,
}

impl fmt::Debug for PluginManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginManager")
            .field("plugins_dir", &self.plugins_dir)
            .field("workspace_root", &self.workspace_root)
            .field("policy", &self.policy)
            .field("plugin_count", &self.plugins.len())
            .finish_non_exhaustive()
    }
}

impl PluginManager {
    /// Creates a [`PluginManager`] targeting `~/.local/share/kova/plugins/`
    /// resolved via [`KovaPaths`].
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError`] if Omarchy paths cannot be resolved or the
    /// Wasmtime engine fails to initialize.
    pub fn new(
        policy: SandboxPolicy,
        editor_bridge: Arc<dyn EditorContextBridge>,
    ) -> Result<Self, PluginHostError> {
        let paths = KovaPaths::resolve().map_err(|err| PluginHostError::Io {
            path: PathBuf::from("~/.local/share/kova/plugins"),
            message: err.to_string(),
        })?;
        Self::with_plugins_dir(paths.plugins_dir(), policy, editor_bridge)
    }

    /// Creates a [`PluginManager`] scanning an explicit `plugins_dir` (ideal for tests).
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError::EngineInit`] if Wasmtime initialization fails.
    pub fn with_plugins_dir(
        plugins_dir: PathBuf,
        policy: SandboxPolicy,
        editor_bridge: Arc<dyn EditorContextBridge>,
    ) -> Result<Self, PluginHostError> {
        let engine = PluginEngine::new(policy.epoch_tick_interval)?;
        Ok(Self {
            plugins_dir,
            workspace_root: None,
            policy,
            editor_bridge,
            engine,
            plugins: BTreeMap::new(),
        })
    }

    /// Returns the configured plugins root directory.
    #[must_use]
    pub fn plugins_dir(&self) -> &Path {
        &self.plugins_dir
    }

    /// Updates the active workspace root directory used for `"workspace"` FS grants.
    pub fn set_workspace_root(&mut self, workspace_root: Option<PathBuf>) {
        self.workspace_root = workspace_root;
    }

    /// Discovers all plugin subdirectories inside `plugins_dir` and loads them.
    ///
    /// If `plugins_dir` does not exist yet, returns an empty `Vec` without error.
    /// A failing or trapping plugin is isolated as `Faulted` without preventing
    /// other valid plugins from loading.
    pub fn load_all(&mut self) -> Vec<PluginLoadOutcome> {
        let Ok(entries) = fs::read_dir(&self.plugins_dir) else {
            return Vec::new();
        };

        let mut dirs: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();

        let mut outcomes = Vec::with_capacity(dirs.len());
        for dir in dirs {
            if !dir.join("plugin.toml").is_file() {
                continue;
            }
            let result = self.load_plugin_dir(&dir);
            let plugin_id = self
                .plugins
                .values()
                .find(|p| p.dir == dir)
                .map(|p| p.manifest.id.clone());
            outcomes.push(PluginLoadOutcome {
                dir,
                plugin_id,
                result,
            });
        }
        outcomes
    }

    /// Discovers and initializes plugins asynchronously on a background worker thread
    /// so cold UI startup (`< 120 ms`) is never blocked.
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError`] if engine initialization fails or the background
    /// loader thread aborts unexpectedly.
    pub async fn load_all_async(
        plugins_dir: PathBuf,
        workspace_root: Option<PathBuf>,
        policy: SandboxPolicy,
        editor_bridge: Arc<dyn EditorContextBridge>,
    ) -> Result<(Self, Vec<PluginLoadOutcome>), PluginHostError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        thread::Builder::new()
            .name("kova-plugin-loader".to_owned())
            .spawn(move || {
                let outcome = (|| {
                    let mut manager = Self::with_plugins_dir(plugins_dir, policy, editor_bridge)?;
                    manager.set_workspace_root(workspace_root);
                    let results = manager.load_all();
                    Ok((manager, results))
                })();
                let _unused = tx.send(outcome);
            })
            .map_err(|err| PluginHostError::AsyncWorker {
                message: format!("failed to spawn async plugin loader: {err}"),
            })?;

        rx.await.map_err(|err| PluginHostError::AsyncWorker {
            message: format!("async plugin loader channel closed: {err}"),
        })?
    }

    /// Loads and initializes a single plugin from `plugin_dir` containing `plugin.toml`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError`] if loading, validation, instantiation, or `init()` fails.
    pub fn load_plugin_dir(&mut self, plugin_dir: &Path) -> Result<PluginStatus, PluginHostError> {
        let (manifest, entrypoint) = PluginManifest::load_from_dir(plugin_dir)?;
        if self.plugins.contains_key(&manifest.id) {
            return Err(PluginHostError::DuplicatePlugin {
                plugin_id: manifest.id,
            });
        }

        let wasm_bytes = fs::read(&entrypoint).map_err(|err| PluginHostError::Io {
            path: entrypoint.clone(),
            message: err.to_string(),
        })?;

        let source = PluginSource::Disk;
        self.instantiate_and_register(manifest, &wasm_bytes, plugin_dir.to_path_buf(), source)
    }

    /// Loads and initializes a plugin directly from an already-validated manifest and
    /// component bytes (useful for embedded fixtures and headless tests).
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError`] if policy validation, compilation, instantiation,
    /// or `init()` fails.
    pub fn load_from_bytes(
        &mut self,
        manifest: PluginManifest,
        wasm_bytes: &[u8],
        base_dir: PathBuf,
    ) -> Result<PluginStatus, PluginHostError> {
        manifest.validate()?;
        if self.plugins.contains_key(&manifest.id) {
            return Err(PluginHostError::DuplicatePlugin {
                plugin_id: manifest.id,
            });
        }
        let source = PluginSource::InMemory {
            wasm_bytes: wasm_bytes.to_vec(),
        };
        self.instantiate_and_register(manifest, wasm_bytes, base_dir, source)
    }

    fn instantiate_and_register(
        &mut self,
        manifest: PluginManifest,
        wasm_bytes: &[u8],
        dir: PathBuf,
        source: PluginSource,
    ) -> Result<PluginStatus, PluginHostError> {
        let id = manifest.id.clone();
        match self.build_runtime(&manifest, wasm_bytes) {
            Ok(mut rt) => {
                let init_res = self.call_init(&id, &mut rt);
                match init_res {
                    Ok(()) => {
                        let mut managed = ManagedPlugin {
                            manifest,
                            dir,
                            source,
                            status: PluginStatus::Active,
                            runtime: Some(rt),
                            last_commands: Vec::new(),
                            last_tool_windows: Vec::new(),
                            last_logs: Vec::new(),
                            last_peak_memory_bytes: 0,
                        };
                        managed.sync_cached_metadata();
                        self.plugins.insert(id, managed);
                        Ok(PluginStatus::Active)
                    }
                    Err(err) => {
                        let reason = err.to_string();
                        tracing::error!(plugin_id = %id, "plugin init failed: {reason}");
                        let mut managed = ManagedPlugin {
                            manifest,
                            dir,
                            source,
                            status: PluginStatus::Faulted { reason },
                            runtime: Some(rt),
                            last_commands: Vec::new(),
                            last_tool_windows: Vec::new(),
                            last_logs: Vec::new(),
                            last_peak_memory_bytes: 0,
                        };
                        managed.sync_cached_metadata();
                        // Drop runtime of a faulted plugin to reclaim memory immediately.
                        managed.runtime = None;
                        self.plugins.insert(id, managed);
                        Err(err)
                    }
                }
            }
            Err(err) => {
                let reason = err.to_string();
                tracing::error!(plugin_id = %id, "plugin instantiation failed: {reason}");
                let managed = ManagedPlugin {
                    manifest,
                    dir,
                    source,
                    status: PluginStatus::Faulted { reason },
                    runtime: None,
                    last_commands: Vec::new(),
                    last_tool_windows: Vec::new(),
                    last_logs: Vec::new(),
                    last_peak_memory_bytes: 0,
                };
                self.plugins.insert(id, managed);
                Err(err)
            }
        }
    }

    fn build_runtime(
        &self,
        manifest: &PluginManifest,
        wasm_bytes: &[u8],
    ) -> Result<ActiveRuntime, PluginHostError> {
        let limits = self.policy.resolve_limits(manifest)?;
        let component: Component = self.engine.compile_component(&manifest.id, wasm_bytes)?;
        let wasi = build_wasi_ctx(manifest, self.workspace_root.as_deref())?;
        let limiter = PluginResourceLimiter::new(limits.max_memory_bytes, limits.max_tables);
        let state = PluginStoreState::new(
            manifest.id.clone(),
            manifest.permissions.clone(),
            wasi,
            limiter,
            Arc::clone(&self.editor_bridge),
            self.policy.max_commands_per_plugin,
            self.policy.max_tool_windows_per_plugin,
        );

        let mut store = Store::new(self.engine.engine(), state);
        store.limiter(PluginStoreState::limiter_mut);

        let guard = self.engine.begin_call(&mut store, limits);
        let bindings = KovaPlugin::instantiate(&mut store, &component, self.engine.linker())
            .map_err(|err| classify_wasmtime_err(&manifest.id, "instantiate", &err))?;
        drop(guard);

        Ok(ActiveRuntime {
            store,
            bindings,
            limits,
        })
    }

    fn call_init(&self, plugin_id: &str, rt: &mut ActiveRuntime) -> Result<(), PluginHostError> {
        let _guard = self.engine.begin_call(&mut rt.store, rt.limits);
        let res = rt
            .bindings
            .kova_plugin_plugin()
            .call_init(&mut rt.store)
            .map_err(|err| classify_wasmtime_err(plugin_id, "init", &err))?;
        res.map_err(|msg| PluginHostError::PluginReturnedError {
            plugin_id: plugin_id.to_owned(),
            operation: "init".to_owned(),
            message: msg,
        })
    }

    /// Gracefully disables an active or faulted plugin without restarting the IDE.
    ///
    /// Calls `shutdown()` if the plugin is currently `Active`, drops its Wasmtime
    /// `Store`, and transitions status to [`PluginStatus::Disabled`].
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError::PluginNotFound`] if `plugin_id` is unknown.
    pub fn disable_plugin(&mut self, plugin_id: &str) -> Result<(), PluginHostError> {
        let engine = &self.engine;
        let plugin =
            self.plugins
                .get_mut(plugin_id)
                .ok_or_else(|| PluginHostError::PluginNotFound {
                    plugin_id: plugin_id.to_owned(),
                })?;

        if let Some(mut rt) = plugin.runtime.take() {
            if plugin.status == PluginStatus::Active {
                let _guard = engine.begin_call(&mut rt.store, rt.limits);
                if let Err(err) = rt
                    .bindings
                    .kova_plugin_plugin()
                    .call_shutdown(&mut rt.store)
                {
                    tracing::warn!(plugin_id = %plugin_id, "error during plugin shutdown: {err}");
                }
            }
            let state = rt.store.data();
            plugin.last_commands = state.registered_commands().to_vec();
            plugin.last_tool_windows = state.registered_tool_windows().to_vec();
            plugin.last_logs = state.logs();
            plugin.last_peak_memory_bytes = state.peak_memory_bytes();
        }
        plugin.status = PluginStatus::Disabled;
        Ok(())
    }

    /// Enables a previously disabled plugin (or recovers a faulted plugin) by
    /// re-instantiating it and calling `init()`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError`] if `plugin_id` is unknown or re-initialization fails.
    pub fn enable_plugin(&mut self, plugin_id: &str) -> Result<PluginStatus, PluginHostError> {
        let current_status = self
            .plugins
            .get(plugin_id)
            .ok_or_else(|| PluginHostError::PluginNotFound {
                plugin_id: plugin_id.to_owned(),
            })?
            .status
            .clone();

        if current_status == PluginStatus::Active {
            return Ok(PluginStatus::Active);
        }

        self.reload_plugin(plugin_id)
    }

    /// Hot-reloads a plugin in place without restarting the IDE.
    ///
    /// For disk-backed plugins, re-reads `plugin.toml` and the `.wasm` component
    /// from disk. Shuts down the previous instance cleanly before activating the new one.
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError`] if reloading or `init()` fails (transitioning the
    /// plugin to [`PluginStatus::Faulted`]).
    pub fn reload_plugin(&mut self, plugin_id: &str) -> Result<PluginStatus, PluginHostError> {
        let existing =
            self.plugins
                .get(plugin_id)
                .ok_or_else(|| PluginHostError::PluginNotFound {
                    plugin_id: plugin_id.to_owned(),
                })?;

        let dir = existing.dir.clone();
        let (manifest, wasm_bytes, new_source) = match &existing.source {
            PluginSource::Disk => {
                let (manifest, entrypoint) = PluginManifest::load_from_dir(&dir)?;
                if manifest.id != plugin_id {
                    return Err(PluginHostError::InvalidManifest {
                        plugin_id: plugin_id.to_owned(),
                        reason: format!(
                            "reloaded manifest changed plugin id from `{plugin_id}` to `{}`",
                            manifest.id
                        ),
                    });
                }
                let bytes = fs::read(&entrypoint).map_err(|err| PluginHostError::Io {
                    path: entrypoint.clone(),
                    message: err.to_string(),
                })?;
                (manifest, bytes, PluginSource::Disk)
            }
            PluginSource::InMemory { wasm_bytes } => (
                existing.manifest.clone(),
                wasm_bytes.clone(),
                PluginSource::InMemory {
                    wasm_bytes: wasm_bytes.clone(),
                },
            ),
        };

        // Shut down existing active instance if present.
        drop(self.disable_plugin(plugin_id));

        if let Some(entry) = self.plugins.get_mut(plugin_id) {
            entry.manifest = manifest.clone();
            entry.source = new_source;
        }

        match self.build_runtime(&manifest, &wasm_bytes) {
            Ok(mut rt) => match self.call_init(plugin_id, &mut rt) {
                Ok(()) => {
                    if let Some(entry) = self.plugins.get_mut(plugin_id) {
                        entry.status = PluginStatus::Active;
                        entry.runtime = Some(rt);
                        entry.sync_cached_metadata();
                    }
                    Ok(PluginStatus::Active)
                }
                Err(err) => {
                    self.mark_faulted_with_runtime(plugin_id, rt, &err);
                    Err(err)
                }
            },
            Err(err) => {
                if let Some(entry) = self.plugins.get_mut(plugin_id) {
                    entry.status = PluginStatus::Faulted {
                        reason: err.to_string(),
                    };
                    entry.runtime = None;
                }
                Err(err)
            }
        }
    }

    /// Broadcasts an IDE lifecycle [`PluginEvent`] to every `Active` plugin.
    ///
    /// A plugin that traps or times out during `on-event` is marked [`PluginStatus::Faulted`]
    /// without affecting other plugins.
    pub fn dispatch_event(&mut self, event: &PluginEvent) {
        let active_ids: Vec<String> = self
            .plugins
            .iter()
            .filter(|(_, p)| p.status == PluginStatus::Active)
            .map(|(id, _)| id.clone())
            .collect();

        for id in active_ids {
            drop(self.invoke_plugin(&id, "on-event", |bindings, store| {
                bindings.kova_plugin_plugin().call_on_event(store, event)
            }));
        }
    }

    /// Invokes a command exported by `plugin_id`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError`] if the plugin is missing, disabled, faulted,
    /// or fails during execution.
    pub fn invoke_command(
        &mut self,
        plugin_id: &str,
        command_id: &str,
    ) -> Result<(), PluginHostError> {
        self.invoke_plugin(plugin_id, "on-command", |bindings, store| {
            bindings
                .kova_plugin_plugin()
                .call_on_command(store, command_id)
        })
    }

    /// Requests the declarative [`WidgetTree`] for `panel_id` from `plugin_id`
    /// and validates the returned tree structure.
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError`] if the plugin is not active, fails execution,
    /// or returns an invalid widget tree.
    pub fn render_ui(
        &mut self,
        plugin_id: &str,
        panel_id: &str,
    ) -> Result<WidgetTree, PluginHostError> {
        let tree = self.invoke_plugin(plugin_id, "render-ui", |bindings, store| {
            bindings
                .kova_plugin_plugin()
                .call_render_ui(store, panel_id)
        })?;
        validate_widget_tree(plugin_id, &tree)?;
        Ok(tree)
    }

    /// Forwards an interactive [`UiEvent`] to `plugin_id` and validates any
    /// updated [`WidgetTree`] returned by the plugin.
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError`] if the plugin is not active, fails execution,
    /// or returns an invalid widget tree.
    pub fn dispatch_ui_event(
        &mut self,
        plugin_id: &str,
        panel_id: &str,
        event: &UiEvent,
    ) -> Result<Option<WidgetTree>, PluginHostError> {
        let maybe_tree = self.invoke_plugin(plugin_id, "on-ui-event", |bindings, store| {
            bindings
                .kova_plugin_plugin()
                .call_on_ui_event(store, panel_id, event)
        })?;
        if let Some(ref tree) = maybe_tree {
            validate_widget_tree(plugin_id, tree)?;
        }
        Ok(maybe_tree)
    }

    fn invoke_plugin<T, F>(
        &mut self,
        plugin_id: &str,
        operation: &str,
        f: F,
    ) -> Result<T, PluginHostError>
    where
        F: FnOnce(&KovaPlugin, &mut Store<PluginStoreState>) -> wasmtime::Result<Result<T, String>>,
    {
        let plugin =
            self.plugins
                .get_mut(plugin_id)
                .ok_or_else(|| PluginHostError::PluginNotFound {
                    plugin_id: plugin_id.to_owned(),
                })?;

        match &plugin.status {
            PluginStatus::Active => {}
            PluginStatus::Disabled => {
                return Err(PluginHostError::PluginDisabled {
                    plugin_id: plugin_id.to_owned(),
                });
            }
            PluginStatus::Faulted { reason } => {
                return Err(PluginHostError::PluginFaulted {
                    plugin_id: plugin_id.to_owned(),
                    reason: reason.clone(),
                });
            }
        }

        let Some(rt) = plugin.runtime.as_mut() else {
            return Err(PluginHostError::PluginDisabled {
                plugin_id: plugin_id.to_owned(),
            });
        };

        let guard = self.engine.begin_call(&mut rt.store, rt.limits);
        let call_result = f(&rt.bindings, &mut rt.store);
        drop(guard);

        match call_result {
            Ok(Ok(value)) => {
                plugin.sync_cached_metadata();
                Ok(value)
            }
            Ok(Err(msg)) => {
                plugin.sync_cached_metadata();
                Err(PluginHostError::PluginReturnedError {
                    plugin_id: plugin_id.to_owned(),
                    operation: operation.to_owned(),
                    message: msg,
                })
            }
            Err(err) => {
                let classified = classify_wasmtime_err(plugin_id, operation, &err);
                tracing::error!(plugin_id = %plugin_id, operation = %operation, "plugin faulted: {classified}");
                plugin.sync_cached_metadata();
                plugin.status = PluginStatus::Faulted {
                    reason: classified.to_string(),
                };
                plugin.runtime = None;
                Err(classified)
            }
        }
    }

    fn mark_faulted_with_runtime(
        &mut self,
        plugin_id: &str,
        rt: ActiveRuntime,
        err: &PluginHostError,
    ) {
        if let Some(entry) = self.plugins.get_mut(plugin_id) {
            entry.runtime = Some(rt);
            entry.sync_cached_metadata();
            entry.status = PluginStatus::Faulted {
                reason: err.to_string(),
            };
            entry.runtime = None;
        }
    }

    /// Returns the current [`PluginStatus`] for `plugin_id`, if known.
    #[must_use]
    pub fn plugin_status(&self, plugin_id: &str) -> Option<&PluginStatus> {
        self.plugins.get(plugin_id).map(|p| &p.status)
    }

    /// Returns a summary snapshot for `plugin_id`.
    #[must_use]
    pub fn plugin_summary(&self, plugin_id: &str) -> Option<PluginSummary> {
        self.plugins.get(plugin_id).map(ManagedPlugin::summary)
    }

    /// Lists summaries for all known plugins sorted deterministically by plugin ID.
    #[must_use]
    pub fn list_plugins(&self) -> Vec<PluginSummary> {
        self.plugins.values().map(ManagedPlugin::summary).collect()
    }

    /// Returns all commands registered across currently `Active` plugins as `(plugin_id, command)`.
    #[must_use]
    pub fn all_commands(&self) -> Vec<(String, CommandDescriptor)> {
        let mut out = Vec::new();
        for (id, plugin) in &self.plugins {
            if plugin.status == PluginStatus::Active
                && let Some(rt) = &plugin.runtime
            {
                for cmd in rt.store.data().registered_commands() {
                    out.push((id.clone(), cmd.clone()));
                }
            }
        }
        out
    }

    /// Returns all tool windows registered across currently `Active` plugins as `(plugin_id, panel)`.
    #[must_use]
    pub fn all_tool_windows(&self) -> Vec<(String, ToolWindowDescriptor)> {
        let mut out = Vec::new();
        for (id, plugin) in &self.plugins {
            if plugin.status == PluginStatus::Active
                && let Some(rt) = &plugin.runtime
            {
                for panel in rt.store.data().registered_tool_windows() {
                    out.push((id.clone(), panel.clone()));
                }
            }
        }
        out
    }

    /// Returns recent logs captured for `plugin_id`.
    #[must_use]
    pub fn plugin_logs(&self, plugin_id: &str) -> Vec<PluginLogEntry> {
        let Some(plugin) = self.plugins.get(plugin_id) else {
            return Vec::new();
        };
        if let Some(rt) = &plugin.runtime {
            rt.store.data().logs()
        } else {
            plugin.last_logs.clone()
        }
    }
}

fn classify_wasmtime_err(
    plugin_id: &str,
    operation: &str,
    err: &wasmtime::Error,
) -> PluginHostError {
    if let Some(trap) = err.downcast_ref::<Trap>() {
        match trap {
            Trap::Interrupt | Trap::OutOfFuel => {
                return PluginHostError::Timeout {
                    plugin_id: plugin_id.to_owned(),
                    operation: operation.to_owned(),
                    message: format!("{trap}"),
                };
            }
            _ => {
                return PluginHostError::ExecutionTrap {
                    plugin_id: plugin_id.to_owned(),
                    operation: operation.to_owned(),
                    message: format!("{trap}: {err}"),
                };
            }
        }
    }

    let msg = err.to_string();
    if msg.contains("epoch") || msg.contains("fuel") || msg.contains("interrupt") {
        PluginHostError::Timeout {
            plugin_id: plugin_id.to_owned(),
            operation: operation.to_owned(),
            message: msg,
        }
    } else if operation == "instantiate" {
        PluginHostError::Instantiation {
            plugin_id: plugin_id.to_owned(),
            message: msg,
        }
    } else {
        PluginHostError::ExecutionTrap {
            plugin_id: plugin_id.to_owned(),
            operation: operation.to_owned(),
            message: msg,
        }
    }
}
