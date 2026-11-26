//! Thin bridge from [`kova_config::ConfigWatcher`] (`KOVA-008`) to the GPUI thread (`KOVA-016`).
//!
//! This module owns no filesystem watcher, debounce or Last-Known-Good logic of its own:
//! it subscribes to the [`ConfigWatcherHandle`] state/event streams and forwards
//! theme switches and configuration warnings as [`ThemeWatchEvent`]s over a `tokio`
//! channel that the GPUI foreground task awaits.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kova_config::{
    ConfigReloadEvent, ConfigWatcher, ConfigWatcherHandle, ConfigWatcherOptions, KovaPaths,
    PathsError, ReloadTarget, WatcherError,
};
use kova_theme::AetherTheme;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc::UnboundedSender;

/// Debounce applied to bursts of configuration file events (spec: `~100 ms`).
pub const THEME_RELOAD_DEBOUNCE: Duration = Duration::from_millis(100);

/// Environment variable overriding the configuration directory (`KOVA-005`).
pub const CONFIG_HOME_ENV: &str = "KOVA_CONFIG_HOME";

/// Update forwarded from the configuration watcher to the UI.
#[derive(Debug, Clone)]
pub enum ThemeWatchEvent {
    /// The active Aether theme changed (initial load, `theme = ...` switch or theme file edit).
    ThemeChanged(Arc<AetherTheme>),
    /// A configuration file was reloaded successfully (clears stale warnings).
    ConfigReloaded,
    /// A configuration file failed to load; the last valid theme stays active.
    Warning(String),
}

/// Keeps the configuration watcher alive; dropping it stops live reload.
#[derive(Debug)]
pub struct ThemeWatch {
    handle: ConfigWatcherHandle,
}

impl ThemeWatch {
    /// Forces an immediate rescan of the configuration files (bypassing the debounce).
    ///
    /// # Errors
    ///
    /// Returns [`WatcherError::ChannelClosed`] if the watcher task has stopped.
    pub async fn reload_now(&self) -> Result<(), WatcherError> {
        self.handle.reload_now().await
    }
}

/// Resolves the Omarchy paths, honouring an explicit `--config-dir` override.
///
/// A relative override is resolved against the current working directory. Without an
/// override, `KOVA_CONFIG_HOME`, `XDG_CONFIG_HOME` and `~/.config/kova` apply in order.
///
/// # Errors
///
/// Returns [`PathsError`] when no home directory can be determined or an environment
/// override is relative.
pub fn resolve_paths(config_dir_override: Option<&Path>) -> Result<KovaPaths, PathsError> {
    let override_dir = config_dir_override.map(absolutize);
    KovaPaths::resolve_with_env(|key| {
        if key == CONFIG_HOME_ENV
            && let Some(dir) = &override_dir
        {
            return Some(OsString::from(dir.as_os_str()));
        }
        std::env::var_os(key)
    })
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
}

/// Starts the configuration watcher and forwards theme updates to `tx`.
///
/// Must be called from within a `tokio` runtime context (e.g. inside a spawned task),
/// because [`ConfigWatcher::start`] performs its initial scan synchronously. The initial
/// active theme and any startup diagnostics are sent immediately.
///
/// # Errors
///
/// Returns [`WatcherError`] when the watcher cannot be started.
pub fn start(
    paths: KovaPaths,
    tx: UnboundedSender<ThemeWatchEvent>,
) -> Result<ThemeWatch, WatcherError> {
    let options = ConfigWatcherOptions {
        debounce: THEME_RELOAD_DEBOUNCE,
        ..ConfigWatcherOptions::default()
    };
    let handle = ConfigWatcher::start(paths, options)?;

    let mut state_rx = handle.subscribe_state();
    let mut events_rx = handle.subscribe_events();

    let mut active = Arc::clone(&state_rx.borrow_and_update().active_theme);
    drop(tx.send(ThemeWatchEvent::ThemeChanged(Arc::clone(&active))));
    for event in handle.startup_diagnostics() {
        if let Some(message) = warning_for(event) {
            drop(tx.send(ThemeWatchEvent::Warning(message)));
        }
    }

    let state_tx = tx.clone();
    tokio::spawn(async move {
        while state_rx.changed().await.is_ok() {
            let next = Arc::clone(&state_rx.borrow_and_update().active_theme);
            if Arc::ptr_eq(&next, &active) || *next == *active {
                continue;
            }
            active = Arc::clone(&next);
            if state_tx.send(ThemeWatchEvent::ThemeChanged(next)).is_err() {
                break;
            }
        }
    });

    tokio::spawn(async move {
        loop {
            let forwarded = match events_rx.recv().await {
                Ok(event) => warning_for(&event).map_or(ThemeWatchEvent::ConfigReloaded, |m| {
                    ThemeWatchEvent::Warning(m)
                }),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            };
            if tx.send(forwarded).is_err() {
                break;
            }
        }
    });

    Ok(ThemeWatch { handle })
}

/// Formats a status-bar warning for failed reloads; `None` for successful events.
#[must_use]
pub fn warning_for(event: &ConfigReloadEvent) -> Option<String> {
    match event {
        ConfigReloadEvent::ReloadFailed {
            target,
            path,
            message,
        } => {
            let what = match target {
                ReloadTarget::Settings => "settings.toml".to_owned(),
                ReloadTarget::Theme(id) => format!("theme `{id}`"),
                ReloadTarget::Keymap => "keymap.toml".to_owned(),
                ReloadTarget::ProjectConfig => "kova.toml".to_owned(),
            };
            let file = path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into(),
            );
            Some(format!(
                "Config error in {what} ({file}): {message} — keeping previous theme"
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_formats_failed_reloads_only() {
        let failed = ConfigReloadEvent::ReloadFailed {
            target: ReloadTarget::Settings,
            path: PathBuf::from("/cfg/settings.toml"),
            message: "expected `=`".to_owned(),
        };
        let text = warning_for(&failed).expect("warning");
        assert!(text.contains("settings.toml"));
        assert!(text.contains("expected `=`"));

        let ok = ConfigReloadEvent::SettingsReloaded {
            path: PathBuf::from("/cfg/settings.toml"),
            generation: 2,
        };
        assert_eq!(warning_for(&ok), None);
    }

    #[test]
    fn config_dir_override_wins_and_is_absolute() {
        let paths = resolve_paths(Some(Path::new("relative-cfg"))).expect("paths");
        assert!(paths.config_dir().is_absolute());
        assert!(paths.config_dir().ends_with("relative-cfg"));

        let abs = std::env::temp_dir().join("kova-cfg-test");
        let paths = resolve_paths(Some(&abs)).expect("paths");
        assert_eq!(paths.config_dir(), abs.as_path());
    }
}
