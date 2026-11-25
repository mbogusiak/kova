//! `kova_config` — Omarchy path resolution and lossless `toml_edit`
//! configuration management for `settings.toml` and `kova.toml`.
//!
//! This crate belongs to the **Headless Core**: it must compile and pass its
//! tests without any windowing or GPU dependency (enforced by KOVA-003).

pub mod document;
pub mod model;
pub mod paths;
pub mod project;
pub mod watcher;

pub use document::SettingsDocument;
pub use model::{
    ConfigError, EditorSettings, JvmSettings, KeymapPreset, KovaSettings, SettingUpdate,
    TerminalSettings, UiSettings,
};
pub use paths::{KovaPaths, PathsError};
pub use project::{
    EffectiveConfig, ProjectBuildTool, ProjectConfig, ProjectConfigDocument,
    ProjectEditorOverrides, ProjectJvmOverrides, RunConfigKind, RunConfiguration,
};
pub use watcher::{
    ConfigReloadEvent, ConfigWatcher, ConfigWatcherHandle, ConfigWatcherOptions, LiveConfigState,
    ReloadTarget, WatcherError,
};

/// Name of this crate as declared in its `Cargo.toml`.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, "kova_config");
    }

    #[test]
    fn inherits_workspace_license() {
        assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0");
    }
}
