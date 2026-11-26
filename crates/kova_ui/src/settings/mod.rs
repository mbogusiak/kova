//! Settings view (`Cmd+,` / `Ctrl+Alt+S`, `KOVA-029`) kept in two-way sync with
//! `~/.config/kova/settings.toml`.
//!
//! - [`model`] — headless [`SettingsFormModel`]: descriptors, filtering, validation, the
//!   translation into `kova_config::SettingUpdate` and SHA-256 self-write suppression.
//! - [`view`] — the [`SettingsView`] overlay state and its GPUI rendering.
//!
//! The UI never writes files: a GUI edit produces a [`SettingsWriteRequest`] that the host
//! (`kova_app`) persists atomically on its `tokio` runtime through the installed
//! [`SettingsWriteSink`]. External edits arrive as [`SettingsSource`] snapshots read by the
//! host after each `kova_config::ConfigWatcher` reload.

pub mod model;
pub mod view;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use kova_config::SettingsDocument;

pub use model::{
    ControlKind, ReloadOutcome, SETTING_DESCRIPTORS, SelectSource, SettingDescriptor,
    SettingFieldError, SettingValue, SettingsCategory, SettingsFormModel,
};
pub use view::{SettingsKeyOutcome, SettingsView};

/// Snapshot of the user configuration files delivered by the host after each reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsSource {
    /// Absolute path of `settings.toml`.
    pub settings_path: PathBuf,
    /// Absolute path of `keymap.toml`.
    pub keymap_path: PathBuf,
    /// File contents; `None` when the file does not exist yet (Omakase defaults apply).
    pub contents: Option<String>,
    /// Theme ids known to the `ThemeRegistry` (built-ins and user themes).
    pub theme_ids: Vec<String>,
}

/// Request to persist `settings.toml` after a GUI edit.
#[derive(Debug, Clone)]
pub struct SettingsWriteRequest {
    /// Destination (`KovaPaths::settings_file()`).
    pub path: PathBuf,
    /// Document to write atomically (comments and formatting preserved).
    pub document: SettingsDocument,
}

/// Host callback that persists a [`SettingsWriteRequest`] off the UI thread.
pub type SettingsWriteSink = Arc<dyn Fn(SettingsWriteRequest) + Send + Sync>;

/// Wrapper giving [`SettingsWriteSink`] a `Debug` implementation.
#[derive(Clone)]
pub struct DebugSink(pub SettingsWriteSink);

impl fmt::Debug for DebugSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SettingsWriteSink")
    }
}
