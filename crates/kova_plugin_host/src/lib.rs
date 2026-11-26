//! `kova_plugin_host` — Wasmtime sandbox, WIT contracts and declarative UI bridge for plugins.
//!
//! This crate belongs to the **Headless Core**: it must compile and pass its
//! tests without any windowing or GPU dependency (enforced by KOVA-003).

pub mod bindings;
pub mod error;
pub mod host_impl;
pub mod manager;
pub mod manifest;
pub mod sandbox;
pub mod ui;

pub use bindings::{
    AstSymbol, BadgeProps, ButtonProps, CodeBlockProps, CommandDescriptor, EditorSnapshot,
    FormProps, ListItem, ListProps, LogLevel, MAX_WIDGET_NODES, MarkdownProps, PluginEvent,
    PluginInfo, SelectProps, SemanticIcon, SplitDirection, SplitProps, StyleVariant, TabItem,
    TableColumn, TableProps, TabsProps, TextInputProps, ToggleProps, ToolWindowDescriptor,
    TreeItem, TreeProps, UiEvent, UiEventKind, WidgetKind, WidgetNode, WidgetTree,
    validate_widget_tree,
};
pub use error::PluginHostError;
pub use host_impl::{EditorContextBridge, InMemoryEditorBridge, PluginLogEntry, PluginStoreState};
pub use manager::{PluginLoadOutcome, PluginManager, PluginStatus, PluginSummary};
pub use manifest::{
    DEFAULT_EXECUTION_TIMEOUT_MS, DEFAULT_MAX_MEMORY_MB, DEFAULT_MAX_TABLES, NetworkRule,
    PluginManifest, PluginPermissions, PluginResourceLimits, SUPPORTED_API_VERSION,
    WORKSPACE_FS_TOKEN,
};
pub use sandbox::{
    DEFAULT_EPOCH_TICK_MS, FsGrant, POLICY_MAX_MEMORY_MB_CEILING, POLICY_MAX_TABLES_CEILING,
    POLICY_MAX_TIMEOUT_MS_CEILING, PluginEngine, PluginResourceLimiter, ResolvedPluginLimits,
    SandboxPolicy, build_wasi_ctx, is_network_host_allowed, is_socket_addr_allowed,
    resolve_fs_grants,
};

/// Name of this crate as declared in its `Cargo.toml`.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, "kova_plugin_host");
    }

    #[test]
    fn inherits_workspace_license() {
        assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0");
    }
}
