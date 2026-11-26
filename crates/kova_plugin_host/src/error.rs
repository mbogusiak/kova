//! Error types for `kova_plugin_host`.

use std::path::PathBuf;

/// Errors returned by manifest validation, sandbox setup, and WASM plugin execution.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginHostError {
    /// Underlying filesystem I/O failure.
    #[error("plugin host I/O error at `{path}`: {message}")]
    Io {
        /// Path involved in the failing I/O operation.
        path: PathBuf,
        /// Human-readable description of the I/O failure.
        message: String,
    },

    /// Failed to parse `plugin.toml`.
    #[error("failed to parse plugin manifest `{path}`: {message}")]
    ManifestParse {
        /// Manifest file path (or `<inline>`).
        path: PathBuf,
        /// TOML parse error message.
        message: String,
    },

    /// Manifest failed semantic or security validation.
    #[error("invalid plugin manifest for `{plugin_id}`: {reason}")]
    InvalidManifest {
        /// Plugin identifier (or `<unknown>` if missing).
        plugin_id: String,
        /// Validation failure explanation.
        reason: String,
    },

    /// Manifest declares an incompatible WIT API version.
    #[error(
        "plugin `{plugin_id}` requires unsupported API version `{found}` (host supports `{supported}`)"
    )]
    UnsupportedApiVersion {
        /// Plugin identifier.
        plugin_id: String,
        /// Version declared in `plugin.toml`.
        found: String,
        /// Version supported by this host.
        supported: String,
    },

    /// Entrypoint `.wasm` path escapes the plugin directory or does not exist.
    #[error("invalid plugin entrypoint `{entrypoint}` for `{plugin_id}`: {reason}")]
    InvalidEntrypoint {
        /// Plugin identifier.
        plugin_id: String,
        /// Declared entrypoint path.
        entrypoint: PathBuf,
        /// Reason the entrypoint was rejected.
        reason: String,
    },

    /// Manifest requests a capability disallowed by host security policy.
    #[error("security policy rejected plugin `{plugin_id}`: {reason}")]
    PolicyViolation {
        /// Plugin identifier.
        plugin_id: String,
        /// Policy check that failed.
        reason: String,
    },

    /// Plugin attempted an operation without the required permission in `plugin.toml`.
    #[error("permission denied for plugin `{plugin_id}`: {capability}")]
    PermissionDenied {
        /// Plugin identifier.
        plugin_id: String,
        /// Missing capability description.
        capability: String,
    },

    /// Failed to initialize the Wasmtime engine or linker.
    #[error("failed to initialize Wasmtime sandbox engine: {message}")]
    EngineInit {
        /// Underlying Wasmtime error message.
        message: String,
    },

    /// Failed to compile or validate a WASM component binary.
    #[error("failed to compile WASM component for `{plugin_id}`: {message}")]
    ComponentLoad {
        /// Plugin identifier.
        plugin_id: String,
        /// Compilation error details.
        message: String,
    },

    /// Failed to instantiate a WASM component inside its isolated store.
    #[error("failed to instantiate plugin `{plugin_id}`: {message}")]
    Instantiation {
        /// Plugin identifier.
        plugin_id: String,
        /// Instantiation error details.
        message: String,
    },

    /// Plugin trapped (panicked or raised unreachable) during execution.
    #[error("plugin `{plugin_id}` trapped during `{operation}`: {message}")]
    ExecutionTrap {
        /// Plugin identifier.
        plugin_id: String,
        /// Guest export being invoked.
        operation: String,
        /// Trap diagnostic message.
        message: String,
    },

    /// Plugin exceeded its execution deadline (`epoch_interruption`) or fuel budget.
    #[error("plugin `{plugin_id}` exceeded execution limit during `{operation}`: {message}")]
    Timeout {
        /// Plugin identifier.
        plugin_id: String,
        /// Guest export being invoked.
        operation: String,
        /// Timeout / fuel details.
        message: String,
    },

    /// Plugin returned a guest-level `Err(msg)` from a WIT export.
    #[error("plugin `{plugin_id}` returned an error from `{operation}`: {message}")]
    PluginReturnedError {
        /// Plugin identifier.
        plugin_id: String,
        /// Guest export being invoked.
        operation: String,
        /// Error string returned by the plugin.
        message: String,
    },

    /// Plugin returned an invalid or cyclic declarative UI widget tree.
    #[error("plugin `{plugin_id}` returned an invalid widget tree: {reason}")]
    InvalidWidgetTree {
        /// Plugin identifier.
        plugin_id: String,
        /// Validation failure details.
        reason: String,
    },

    /// No plugin is registered under the requested ID.
    #[error("plugin `{plugin_id}` is not loaded")]
    PluginNotFound {
        /// Requested plugin identifier.
        plugin_id: String,
    },

    /// A plugin with the same ID is already registered.
    #[error("duplicate plugin id `{plugin_id}`")]
    DuplicatePlugin {
        /// Conflicting plugin identifier.
        plugin_id: String,
    },

    /// Plugin is currently disabled and cannot execute exports.
    #[error("plugin `{plugin_id}` is disabled")]
    PluginDisabled {
        /// Plugin identifier.
        plugin_id: String,
    },

    /// Plugin is in `Faulted` state and must be reloaded or re-enabled before use.
    #[error("plugin `{plugin_id}` is faulted: {reason}")]
    PluginFaulted {
        /// Plugin identifier.
        plugin_id: String,
        /// Fault reason recorded when it failed.
        reason: String,
    },

    /// Background plugin worker channel closed unexpectedly.
    #[error("async plugin worker failed: {message}")]
    AsyncWorker {
        /// Failure details.
        message: String,
    },
}
