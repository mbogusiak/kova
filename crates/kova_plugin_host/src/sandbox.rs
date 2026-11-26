//! Wasmtime sandbox configuration, resource limiters, epoch interruption, and WASI capability setup (FR-1, FR-3).

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, ResourceLimiter, Store};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder};

use crate::bindings::KovaPlugin;
use crate::error::PluginHostError;
use crate::host_impl::PluginStoreState;
use crate::manifest::{
    DEFAULT_EXECUTION_TIMEOUT_MS, DEFAULT_MAX_MEMORY_MB, DEFAULT_MAX_TABLES, NetworkRule,
    PluginManifest, WORKSPACE_FS_TOKEN,
};

/// Upper ceiling for per-plugin linear memory (512 MiB).
pub const POLICY_MAX_MEMORY_MB_CEILING: u64 = 512;
/// Upper ceiling for per-plugin table elements.
pub const POLICY_MAX_TABLES_CEILING: usize = 10_000;
/// Upper ceiling for per-call execution timeout (30 s).
pub const POLICY_MAX_TIMEOUT_MS_CEILING: u64 = 30_000;
/// Default interval between Wasmtime epoch ticks while a plugin call is active.
pub const DEFAULT_EPOCH_TICK_MS: u64 = 5;
/// Maximum bytes buffered per plugin stdout/stderr pipe (64 KiB).
pub const DEFAULT_STDIO_BUFFER_BYTES: usize = 64 * 1024;

/// Host-wide security ceilings enforced regardless of what `plugin.toml` requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// Maximum linear memory a plugin may request in `plugin.toml` (MiB).
    pub max_memory_mb_ceiling: u64,
    /// Maximum table elements a plugin may request in `plugin.toml`.
    pub max_tables_ceiling: usize,
    /// Maximum execution timeout per guest call.
    pub max_execution_timeout: Duration,
    /// Tick cadence for `Engine::increment_epoch()` while plugin calls are in flight.
    pub epoch_tick_interval: Duration,
    /// Whether `process_spawn = true` in `plugin.toml` is permitted (default `false`).
    pub allow_process_spawn: bool,
    /// Maximum number of commands a single plugin may register.
    pub max_commands_per_plugin: usize,
    /// Maximum number of tool windows a single plugin may register.
    pub max_tool_windows_per_plugin: usize,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            max_memory_mb_ceiling: POLICY_MAX_MEMORY_MB_CEILING,
            max_tables_ceiling: POLICY_MAX_TABLES_CEILING,
            max_execution_timeout: Duration::from_millis(POLICY_MAX_TIMEOUT_MS_CEILING),
            epoch_tick_interval: Duration::from_millis(DEFAULT_EPOCH_TICK_MS),
            allow_process_spawn: false,
            max_commands_per_plugin: 128,
            max_tool_windows_per_plugin: 16,
        }
    }
}

/// Effective resolved limits applied to a single plugin instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedPluginLimits {
    /// Linear memory limit in bytes.
    pub max_memory_bytes: usize,
    /// Maximum table element count.
    pub max_tables: usize,
    /// Per-call timeout duration.
    pub execution_timeout: Duration,
    /// Optional deterministic instruction fuel budget per call.
    pub fuel: Option<u64>,
}

impl SandboxPolicy {
    /// Validates manifest limits and permissions against host security policy and
    /// computes the effective [`ResolvedPluginLimits`].
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError::PolicyViolation`] if the plugin requests
    /// disallowed capabilities (`process_spawn = true`) or exceeds host ceilings.
    pub fn resolve_limits(
        &self,
        manifest: &PluginManifest,
    ) -> Result<ResolvedPluginLimits, PluginHostError> {
        if manifest.permissions.process_spawn && !self.allow_process_spawn {
            return Err(PluginHostError::PolicyViolation {
                plugin_id: manifest.id.clone(),
                reason: "`process_spawn = true` is disallowed by host security policy".to_owned(),
            });
        }

        let memory_mb = manifest
            .limits
            .max_memory_mb
            .unwrap_or(DEFAULT_MAX_MEMORY_MB);
        if memory_mb > self.max_memory_mb_ceiling {
            return Err(PluginHostError::PolicyViolation {
                plugin_id: manifest.id.clone(),
                reason: format!(
                    "requested max_memory_mb ({memory_mb} MiB) exceeds policy ceiling ({} MiB)",
                    self.max_memory_mb_ceiling
                ),
            });
        }
        let max_memory_bytes =
            usize::try_from(memory_mb.saturating_mul(1024 * 1024)).map_err(|_| {
                PluginHostError::PolicyViolation {
                    plugin_id: manifest.id.clone(),
                    reason: "requested memory limit overflows platform address space".to_owned(),
                }
            })?;

        let max_tables = manifest.limits.max_tables.unwrap_or(DEFAULT_MAX_TABLES);
        if max_tables > self.max_tables_ceiling {
            return Err(PluginHostError::PolicyViolation {
                plugin_id: manifest.id.clone(),
                reason: format!(
                    "requested max_tables ({max_tables}) exceeds policy ceiling ({})",
                    self.max_tables_ceiling
                ),
            });
        }

        let timeout_ms = manifest
            .limits
            .execution_timeout_ms
            .unwrap_or(DEFAULT_EXECUTION_TIMEOUT_MS);
        let execution_timeout = Duration::from_millis(timeout_ms);
        if execution_timeout > self.max_execution_timeout {
            return Err(PluginHostError::PolicyViolation {
                plugin_id: manifest.id.clone(),
                reason: format!(
                    "requested execution_timeout_ms ({timeout_ms} ms) exceeds policy ceiling ({} ms)",
                    self.max_execution_timeout.as_millis()
                ),
            });
        }

        Ok(ResolvedPluginLimits {
            max_memory_bytes,
            max_tables,
            execution_timeout,
            fuel: manifest.limits.fuel,
        })
    }
}

/// Per-store `wasmtime::ResourceLimiter` enforcing linear memory and table ceilings.
#[derive(Debug, Clone)]
pub struct PluginResourceLimiter {
    max_memory_bytes: usize,
    max_tables: usize,
    max_instances: usize,
    max_memories: usize,
    peak_memory_bytes: usize,
    denied_allocations: u64,
}

impl PluginResourceLimiter {
    /// Creates a limiter with the resolved plugin bounds.
    #[must_use]
    pub const fn new(max_memory_bytes: usize, max_tables: usize) -> Self {
        Self {
            max_memory_bytes,
            max_tables,
            max_instances: 16,
            max_memories: 16,
            peak_memory_bytes: 0,
            denied_allocations: 0,
        }
    }

    /// Configured linear memory limit in bytes.
    #[must_use]
    pub const fn max_memory_bytes(&self) -> usize {
        self.max_memory_bytes
    }

    /// Peak linear memory observed by this limiter in bytes.
    #[must_use]
    pub const fn peak_memory_bytes(&self) -> usize {
        self.peak_memory_bytes
    }

    /// Number of memory or table growth attempts rejected by this limiter.
    #[must_use]
    pub const fn denied_allocations(&self) -> u64 {
        self.denied_allocations
    }
}

impl ResourceLimiter for PluginResourceLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let exceeds_policy = desired > self.max_memory_bytes;
        let exceeds_declared_max = maximum.is_some_and(|max| desired > max);
        if exceeds_policy || exceeds_declared_max {
            self.denied_allocations = self.denied_allocations.saturating_add(1);
            return Ok(false);
        }
        if desired > self.peak_memory_bytes {
            self.peak_memory_bytes = desired;
        }
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let exceeds_policy = desired > self.max_tables;
        let exceeds_declared_max = maximum.is_some_and(|max| desired > max);
        if exceeds_policy || exceeds_declared_max {
            self.denied_allocations = self.denied_allocations.saturating_add(1);
            return Ok(false);
        }
        Ok(true)
    }

    fn instances(&self) -> usize {
        self.max_instances
    }

    fn tables(&self) -> usize {
        self.max_tables
    }

    fn memories(&self) -> usize {
        self.max_memories
    }
}

#[derive(Debug, Default)]
struct TickerControl {
    active_calls: usize,
    shutdown: bool,
}

/// Shared Wasmtime sandbox engine with epoch interruption and linked host/WASI bindings.
pub struct PluginEngine {
    engine: Engine,
    linker: Linker<PluginStoreState>,
    epoch_tick_interval: Duration,
    ticker_state: Arc<(Mutex<TickerControl>, Condvar)>,
    ticker_thread: Option<JoinHandle<()>>,
}

impl fmt::Debug for PluginEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginEngine")
            .field("epoch_tick_interval", &self.epoch_tick_interval)
            .finish_non_exhaustive()
    }
}

impl PluginEngine {
    /// Initializes a Wasmtime `Engine` configured with Component Model,
    /// `epoch_interruption`, and `consume_fuel`, plus WASI P2 and Kova host bindings.
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError::EngineInit`] if Wasmtime engine or linker setup fails.
    pub fn new(epoch_tick_interval: Duration) -> Result<Self, PluginHostError> {
        let tick = if epoch_tick_interval.is_zero() {
            Duration::from_millis(DEFAULT_EPOCH_TICK_MS)
        } else {
            epoch_tick_interval
        };

        let mut config = Config::new();
        config.wasm_component_model(true);
        config.epoch_interruption(true);
        config.consume_fuel(true);

        let engine = Engine::new(&config).map_err(|err| PluginHostError::EngineInit {
            message: err.to_string(),
        })?;

        let mut linker = Linker::<PluginStoreState>::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|err| {
            PluginHostError::EngineInit {
                message: format!("failed to register WASI P2 bindings: {err}"),
            }
        })?;
        KovaPlugin::add_to_linker::<PluginStoreState, wasmtime::component::HasSelf<PluginStoreState>>(
            &mut linker,
            |state| state,
        )
        .map_err(|err| PluginHostError::EngineInit {
            message: format!("failed to register Kova WIT host bindings: {err}"),
        })?;

        let ticker_state = Arc::new((Mutex::new(TickerControl::default()), Condvar::new()));
        let bg_state = Arc::clone(&ticker_state);
        let bg_engine = engine.clone();
        let ticker_thread = thread::Builder::new()
            .name("kova-wasm-epoch-ticker".to_owned())
            .spawn(move || run_epoch_ticker(&bg_engine, &bg_state, tick))
            .map_err(|err| PluginHostError::EngineInit {
                message: format!("failed to spawn epoch ticker thread: {err}"),
            })?;

        Ok(Self {
            engine,
            linker,
            epoch_tick_interval: tick,
            ticker_state,
            ticker_thread: Some(ticker_thread),
        })
    }

    /// Returns a reference to the underlying `wasmtime::Engine`.
    #[must_use]
    pub const fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Returns a reference to the configured component `Linker`.
    #[must_use]
    pub const fn linker(&self) -> &Linker<PluginStoreState> {
        &self.linker
    }

    /// Configured epoch tick interval.
    #[must_use]
    pub const fn epoch_tick_interval(&self) -> Duration {
        self.epoch_tick_interval
    }

    /// Computes how many epoch ticks correspond to `timeout` (always at least 1).
    #[must_use]
    pub fn ticks_for_timeout(&self, timeout: Duration) -> u64 {
        let tick_nanos = self.epoch_tick_interval.as_nanos().max(1);
        let timeout_nanos = timeout.as_nanos();
        let ticks = timeout_nanos.div_ceil(tick_nanos).max(1);
        u64::try_from(ticks).unwrap_or(u64::MAX)
    }

    /// Compiles a WASM component from `.wasm` (or WAT in tests) bytes.
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError::ComponentLoad`] if compilation fails.
    pub fn compile_component(
        &self,
        plugin_id: &str,
        bytes: &[u8],
    ) -> Result<Component, PluginHostError> {
        Component::new(&self.engine, bytes).map_err(|err| PluginHostError::ComponentLoad {
            plugin_id: plugin_id.to_owned(),
            message: err.to_string(),
        })
    }

    /// Arms epoch deadline and fuel budget on `store` and returns an RAII guard
    /// that activates the background epoch ticker for the duration of the call.
    pub(crate) fn begin_call(
        &self,
        store: &mut Store<PluginStoreState>,
        limits: ResolvedPluginLimits,
    ) -> EpochCallGuard<'_> {
        let ticks = self.ticks_for_timeout(limits.execution_timeout);
        store.set_epoch_deadline(ticks);
        store.epoch_deadline_trap();

        // `consume_fuel(true)` is enabled on the engine, so every store must have
        // non-zero fuel assigned prior to entering guest code.
        let fuel = limits.fuel.unwrap_or(u64::MAX / 2);
        drop(store.set_fuel(fuel));

        if let Ok(mut guard) = self.ticker_state.0.lock() {
            guard.active_calls = guard.active_calls.saturating_add(1);
            self.ticker_state.1.notify_all();
        }

        EpochCallGuard {
            ticker_state: &self.ticker_state,
        }
    }
}

impl Drop for PluginEngine {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.ticker_state.0.lock() {
            guard.shutdown = true;
            self.ticker_state.1.notify_all();
        }
        if let Some(handle) = self.ticker_thread.take() {
            drop(handle.join());
        }
    }
}

pub(crate) struct EpochCallGuard<'a> {
    ticker_state: &'a (Mutex<TickerControl>, Condvar),
}

impl Drop for EpochCallGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.ticker_state.0.lock() {
            guard.active_calls = guard.active_calls.saturating_sub(1);
        }
    }
}

fn run_epoch_ticker(
    engine: &Engine,
    state: &(Mutex<TickerControl>, Condvar),
    tick_interval: Duration,
) {
    let (lock, cvar) = state;
    let Ok(mut guard) = lock.lock() else {
        return;
    };

    while !guard.shutdown {
        if guard.active_calls == 0 {
            let Ok(next) = cvar.wait(guard) else {
                return;
            };
            guard = next;
            continue;
        }
        let Ok((next, _timeout)) = cvar.wait_timeout(guard, tick_interval) else {
            return;
        };
        guard = next;
        if guard.shutdown {
            break;
        }
        if guard.active_calls > 0 {
            engine.increment_epoch();
        }
    }
}

/// Directory preopen grant derived from `plugin.toml` `fs_read` / `fs_write`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsGrant {
    /// Canonical or host-resolved directory path on the host filesystem.
    pub host_path: PathBuf,
    /// Guest virtual mount path exposed inside WASI preopens.
    pub guest_path: String,
    /// WASI filesystem permissions (`ReadOnly` or `ReadWrite`).
    pub writable: bool,
}

/// Resolves declared `fs_read` and `fs_write` rules into concrete WASI directory preopens.
#[must_use]
pub fn resolve_fs_grants(manifest: &PluginManifest, workspace_root: Option<&Path>) -> Vec<FsGrant> {
    let mut grants: BTreeMap<PathBuf, (String, bool)> = BTreeMap::new();

    let mut insert_grant = |raw: &str, writable: bool| {
        let (host_path, guest_path) = if raw == WORKSPACE_FS_TOKEN {
            let Some(root) = workspace_root else {
                return;
            };
            (root.to_path_buf(), "/workspace".to_owned())
        } else {
            let p = PathBuf::from(raw);
            let guest = p.to_string_lossy().into_owned();
            (p, guest)
        };

        if !host_path.is_dir() {
            return;
        }

        grants
            .entry(host_path)
            .and_modify(|entry| {
                if writable {
                    entry.1 = true;
                }
            })
            .or_insert((guest_path, writable));
    };

    for entry in &manifest.permissions.fs_read {
        insert_grant(entry, false);
    }
    for entry in &manifest.permissions.fs_write {
        insert_grant(entry, true);
    }

    grants
        .into_iter()
        .map(|(host_path, (guest_path, writable))| FsGrant {
            host_path,
            guest_path,
            writable,
        })
        .collect()
}

/// Returns `true` if `addr` matches at least one explicit [`NetworkRule`].
#[must_use]
pub fn is_socket_addr_allowed(rules: &[NetworkRule], addr: SocketAddr) -> bool {
    if rules.is_empty() {
        return false;
    }
    let ip = addr.ip();
    let port = addr.port();
    rules.iter().any(|rule| {
        let port_matches = rule.port.is_none_or(|allowed_port| allowed_port == port);
        if !port_matches {
            return false;
        }
        match rule.ip {
            Some(allowed_ip) => allowed_ip == ip,
            None => rule.host == "localhost" && ip.is_loopback(),
        }
    })
}

/// Returns `true` if `host` (and optional `port`) matches at least one explicit [`NetworkRule`].
#[must_use]
pub fn is_network_host_allowed(rules: &[NetworkRule], host: &str, port: Option<u16>) -> bool {
    let normalized = host.trim().to_ascii_lowercase();
    rules.iter().any(|rule| {
        let host_matches = rule.host == normalized;
        let port_matches = match (rule.port, port) {
            (Some(allowed), Some(req)) => allowed == req,
            (Some(_), None) => false,
            (None, _) => true,
        };
        host_matches && port_matches
    })
}

/// Builds a capability-restricted `WasiCtx` strictly honoring `manifest.permissions`.
///
/// By default:
/// - No host environment variables or CLI arguments are inherited.
/// - Stdin is closed; stdout/stderr write to bounded in-memory pipes.
/// - No filesystem directories are preopened unless explicitly allowed in `fs_read` / `fs_write`.
/// - All TCP, UDP, and DNS lookups are denied unless `network` rules are declared.
///
/// # Errors
///
/// Returns [`PluginHostError`] if network rule parsing or preopening an allowed
/// directory fails.
pub fn build_wasi_ctx(
    manifest: &PluginManifest,
    workspace_root: Option<&Path>,
) -> Result<WasiCtx, PluginHostError> {
    let mut builder = WasiCtxBuilder::new();
    builder.stdout(MemoryOutputPipe::new(DEFAULT_STDIO_BUFFER_BYTES));
    builder.stderr(MemoryOutputPipe::new(DEFAULT_STDIO_BUFFER_BYTES));

    for grant in resolve_fs_grants(manifest, workspace_root) {
        let perms = if grant.writable {
            FsPerms::ReadWrite
        } else {
            FsPerms::ReadOnly
        };
        builder
            .preopened_dir(&grant.host_path, &grant.guest_path, perms)
            .map_err(|err| PluginHostError::Io {
                path: grant.host_path.clone(),
                message: format!("failed to preopen WASI directory: {err}"),
            })?;
    }

    let network_rules = manifest.parsed_network_rules()?;
    if network_rules.is_empty() {
        builder.allow_tcp(false);
        builder.allow_udp(false);
        builder.allow_ip_name_lookup(false);
    } else {
        let has_hostname_rule = network_rules.iter().any(|rule| rule.ip.is_none());
        let rules = Arc::new(network_rules);
        builder.allow_tcp(true);
        builder.allow_udp(true);
        builder.allow_ip_name_lookup(has_hostname_rule);
        builder.socket_addr_check(move |addr, _use_kind| {
            let allowed = is_socket_addr_allowed(&rules, addr);
            Box::pin(async move { allowed })
        });
    }

    Ok(builder.build())
}
